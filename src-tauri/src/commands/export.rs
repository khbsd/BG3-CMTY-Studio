use crate::error::AppError;
use crate::export::writer::FileReport;
use crate::export::{self, ExportContext, FileAction};
use std::path::PathBuf;

/// Result of a save-project operation, sent to the frontend.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SaveProjectResult {
    pub files_created: Vec<FileReport>,
    pub files_updated: Vec<FileReport>,
    pub files_deleted: Vec<FileReport>,
    pub files_unchanged: usize,
    pub total_entries: usize,
    pub errors: Vec<String>,
    pub dry_run: bool,
}

/// Reset all staging tracking flags after a successful save.
///
/// For each data table listed in `_table_meta`:
///   - Clear `_is_new` and `_is_modified` flags on tracked rows
///   - Hard-delete rows marked `_is_deleted`
///   - Update `_table_meta.row_count` to reflect the new state
///
/// Also clears the undo journal if it exists.
fn reset_staging_tracking(conn: &rusqlite::Connection) -> Result<(), AppError> {
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT table_name FROM _table_meta")
            .map_err(|e| AppError::internal(format!("List staging tables: {e}")))?;
        let collected: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| AppError::internal(format!("Query staging tables: {e}")))?
            .filter_map(|r| r.ok())
            .collect();
        collected
    };

    for table in &tables {
        // Reset tracking flags on new/modified rows
        conn.execute(
            &format!(
                "UPDATE \"{table}\" SET _is_new=0, _is_modified=0 WHERE _is_new=1 OR _is_modified=1"
            ),
            [],
        )
        .map_err(|e| AppError::internal(format!("Reset tracking for {table}: {e}")))?;

        // Hard-delete rows that were soft-deleted
        conn.execute(&format!("DELETE FROM \"{table}\" WHERE _is_deleted=1"), [])
            .map_err(|e| AppError::internal(format!("Purge deleted from {table}: {e}")))?;

        // Update row count in _table_meta
        let _ = conn.execute(
            &format!(
                "UPDATE _table_meta SET row_count = (SELECT COUNT(*) FROM \"{table}\") WHERE table_name = ?1"
            ),
            [table.as_str()],
        );
    }

    // Clear the undo journal if it exists
    let journal_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='_staging_undo_journal'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);

    if journal_exists {
        conn.execute("DELETE FROM _staging_undo_journal", [])
            .map_err(|e| AppError::internal(format!("Clear undo journal: {e}")))?;
    }

    Ok(())
}

/// Synchronous inner implementation of the save pipeline.
fn save_project_sync(
    staging_db_path: String,
    ref_base_path: String,
    mod_path: String,
    mod_name: String,
    mod_folder: String,
    backup: bool,
    dry_run: bool,
) -> Result<SaveProjectResult, AppError> {
    // 1. Open staging DB connection
    let staging_db = PathBuf::from(&staging_db_path);
    if !staging_db.is_file() {
        return Err(
            AppError::not_found("staging_db_not_found").with_context("path", &staging_db_path)
        );
    }
    let conn = rusqlite::Connection::open(&staging_db)
        .map_err(|e| AppError::io_error(format!("Open staging DB: {e}")))?;

    let ref_base = PathBuf::from(&ref_base_path);
    let mod_path_buf = PathBuf::from(&mod_path);

    // 2. Build ExportContext
    let ctx = ExportContext {
        staging_conn: conn,
        ref_base_path: ref_base,
        mod_path: mod_path_buf.clone(),
        mod_name,
        mod_folder,
    };

    // 3. Get handlers from registry
    let handlers = &*export::HANDLER_REGISTRY;

    // 4. Build export plan
    let mut plan = export::build_export_plan(&ctx, handlers)?;

    // 5. Render content for each unit that needs writing
    for unit in plan.units.iter_mut() {
        if unit.action == FileAction::Delete || unit.action == FileAction::Unchanged {
            continue;
        }
        let handler = handlers
            .iter()
            .find(|h| h.name() == unit.handler_name)
            .ok_or_else(|| {
                AppError::internal(format!(
                    "No handler found for '{}' (unit: {})",
                    unit.handler_name,
                    unit.output_path.display()
                ))
            })?;
        let content = handler.render(unit, &ctx)?;
        unit.content = Some(content);
    }

    // 6. Compute file delta
    let mut delta = export::delta::compute_file_delta(&mut plan, &mod_path_buf)?;

    // 7. If dry_run, build result from delta and return early
    if dry_run {
        let files_created: Vec<FileReport> = delta
            .creates
            .iter()
            .map(|e| FileReport {
                path: e.unit.output_path.display().to_string(),
                handler: e.unit.handler_name.clone(),
                entry_count: e.unit.entry_count,
                bytes_written: e.unit.content.as_ref().map_or(0, |c| c.len()),
                backed_up: false,
            })
            .collect();
        let files_updated: Vec<FileReport> = delta
            .updates
            .iter()
            .map(|e| FileReport {
                path: e.unit.output_path.display().to_string(),
                handler: e.unit.handler_name.clone(),
                entry_count: e.unit.entry_count,
                bytes_written: e.unit.content.as_ref().map_or(0, |c| c.len()),
                backed_up: false,
            })
            .collect();
        let files_deleted: Vec<FileReport> = delta
            .deletes
            .iter()
            .map(|e| FileReport {
                path: e.unit.output_path.display().to_string(),
                handler: e.unit.handler_name.clone(),
                entry_count: e.unit.entry_count,
                bytes_written: 0,
                backed_up: false,
            })
            .collect();

        let total_entries = files_created.iter().map(|f| f.entry_count).sum::<usize>()
            + files_updated.iter().map(|f| f.entry_count).sum::<usize>()
            + files_deleted.iter().map(|f| f.entry_count).sum::<usize>();

        return Ok(SaveProjectResult {
            files_created,
            files_updated,
            files_deleted,
            files_unchanged: delta.unchanged.len(),
            total_entries,
            errors: Vec::new(),
            dry_run: true,
        });
    }

    // 8. Write files atomically
    let report = export::writer::write_files_atomic(&mut delta, backup)?;

    // 9. Reset staging tracking flags
    reset_staging_tracking(&ctx.staging_conn)?;

    // 10. Reclaim space from hard-deleted rows and compacted undo journal
    ctx.staging_conn
        .execute_batch("VACUUM")
        .map_err(|e| AppError::internal(format!("VACUUM staging DB: {e}")))?;

    // 11. Return result
    Ok(SaveProjectResult {
        files_created: report.files_created,
        files_updated: report.files_updated,
        files_deleted: report.files_deleted,
        files_unchanged: report.files_unchanged,
        total_entries: report.total_entries,
        errors: report.errors,
        dry_run: false,
    })
}

/// Synchronous inner implementation of section-scoped save.
///
/// Only runs handlers whose `claimed_table_prefixes()` match `table_name`.
/// Does NOT reset staging tracking flags or VACUUM (lightweight for auto-save).
fn save_section_sync(
    staging_db_path: String,
    ref_base_path: String,
    mod_path: String,
    mod_name: String,
    mod_folder: String,
    table_name: String,
) -> Result<SaveProjectResult, AppError> {
    let staging_db = PathBuf::from(&staging_db_path);
    if !staging_db.is_file() {
        return Err(
            AppError::not_found("staging_db_not_found").with_context("path", &staging_db_path)
        );
    }
    let conn = rusqlite::Connection::open(&staging_db)
        .map_err(|e| AppError::io_error(format!("Open staging DB: {e}")))?;

    let ref_base = PathBuf::from(&ref_base_path);
    let mod_path_buf = PathBuf::from(&mod_path);

    let ctx = ExportContext {
        staging_conn: conn,
        ref_base_path: ref_base,
        mod_path: mod_path_buf.clone(),
        mod_name,
        mod_folder,
    };

    // Strip optional "staging_" prefix for matching against handler prefixes
    let bare_table = table_name.strip_prefix("staging_").unwrap_or(&table_name);

    // Find handlers that claim this table
    let all_handlers = &*export::HANDLER_REGISTRY;
    let matching_handlers: Vec<&Box<dyn export::FileTypeHandler>> = all_handlers
        .iter()
        .filter(|h| {
            h.claimed_table_prefixes()
                .iter()
                .any(|prefix| bare_table.starts_with(prefix))
        })
        .collect();

    if matching_handlers.is_empty() {
        return Ok(SaveProjectResult {
            files_created: Vec::new(),
            files_updated: Vec::new(),
            files_deleted: Vec::new(),
            files_unchanged: 0,
            total_entries: 0,
            errors: vec![format!("No handler claims table '{}'", table_name)],
            dry_run: false,
        });
    }

    // Plan + render only for matching handlers
    let mut plan = export::ExportPlan::default();
    for handler in &matching_handlers {
        let units = handler.plan(&ctx)?;
        plan.units.extend(units);
    }

    for unit in plan.units.iter_mut() {
        if unit.action == FileAction::Delete || unit.action == FileAction::Unchanged {
            continue;
        }
        let handler = matching_handlers
            .iter()
            .find(|h| h.name() == unit.handler_name)
            .ok_or_else(|| {
                AppError::internal(format!(
                    "No handler found for '{}' (unit: {})",
                    unit.handler_name,
                    unit.output_path.display()
                ))
            })?;
        let content = handler.render(unit, &ctx)?;
        unit.content = Some(content);
    }

    // Compute delta and write (no backup for auto-save)
    let mut delta = export::delta::compute_file_delta(&mut plan, &mod_path_buf)?;
    let report = export::writer::write_files_atomic(&mut delta, false)?;

    // NOTE: Do NOT reset staging tracking or VACUUM — only full save does that.

    Ok(SaveProjectResult {
        files_created: report.files_created,
        files_updated: report.files_updated,
        files_deleted: report.files_deleted,
        files_unchanged: report.files_unchanged,
        total_entries: report.total_entries,
        errors: report.errors,
        dry_run: false,
    })
}

/// Save only the files related to a specific staging table.
///
/// Lightweight alternative to `cmd_save_project` for auto-save after mutations.
/// Does NOT reset staging tracking flags or VACUUM.
#[tauri::command]
pub async fn cmd_save_section(
    staging_db_path: String,
    ref_base_path: String,
    mod_path: String,
    mod_name: String,
    mod_folder: String,
    table_name: String,
) -> Result<SaveProjectResult, AppError> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::task::spawn_blocking(move || {
            save_section_sync(
                staging_db_path,
                ref_base_path,
                mod_path,
                mod_name,
                mod_folder,
                table_name,
            )
        }),
    )
    .await
    {
        Ok(join_result) => join_result
            .map_err(|e| AppError::task_panicked(format!("Section save task panicked: {e}")))?,
        Err(_) => Err(AppError::timeout("Section save timed out after 30s")),
    }
}

/// Save the current staging DB state to mod files on disk.
///
/// Orchestrates the full export pipeline: plan → render → delta → write.
/// When `dry_run` is true, computes the delta but skips disk writes.
#[tauri::command]
pub async fn cmd_save_project(
    staging_db_path: String,
    ref_base_path: String,
    mod_path: String,
    mod_name: String,
    mod_folder: String,
    backup: bool,
    dry_run: bool,
) -> Result<SaveProjectResult, AppError> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        tokio::task::spawn_blocking(move || {
            save_project_sync(
                staging_db_path,
                ref_base_path,
                mod_path,
                mod_name,
                mod_folder,
                backup,
                dry_run,
            )
        }),
    )
    .await
    {
        Ok(join_result) => {
            join_result.map_err(|e| AppError::task_panicked(format!("Save task panicked: {e}")))?
        }
        Err(_) => Err(AppError::timeout("Save operation timed out after 300s")),
    }
}
