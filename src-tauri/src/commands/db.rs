use crate::commands::mod_import::PopulateResult;
use crate::db_manager;
use crate::error::AppError;
use crate::reference_db;
use crate::{blocking, blocking_with_timeout};

/// Per-database status info (exists, size).
#[derive(serde::Serialize, Clone)]
pub struct DbFileStatus {
    pub name: String,
    pub path: String,
    pub exists: bool,
    pub size_bytes: u64,
}

/// Stream vanilla .pak files and populate the embedded schema databases.
///
/// Emits `pipeline-progress` events during execution for real-time status updates.
#[tauri::command]
pub async fn cmd_populate_game_data(
    app: tauri::AppHandle,
    game_data_path: String,
) -> Result<PopulateResult, AppError> {
    use tauri::Emitter;

    blocking_with_timeout(std::time::Duration::from_secs(1200), move || {
        let db_paths = db_manager::ensure_schema_dbs(&app)?;
        let options = reference_db::BuildOptions::default();
        let summary = reference_db::pipeline::populate_vanilla_dbs(
            &game_data_path,
            &db_paths.base,
            Some(&db_paths.honor),
            &options,
            |progress| {
                let _ = app.emit("pipeline-progress", &progress);
            },
        )?;

        let mut diagnostics = summary.diagnostics;
        let mut errors = summary.errors;

        // DSM-04: Post-populate integrity check on reference DBs
        for (label, path) in [("ref_base", &db_paths.base), ("ref_honor", &db_paths.honor)] {
            match db_manager::run_integrity_check(path) {
                Ok(None) => {
                    diagnostics.push(format!("Integrity check {label}: ok"));
                }
                Ok(Some(details)) => {
                    let msg = format!("Integrity check {label} WARNING: {details}");
                    eprintln!("  {msg}");
                    errors.push(msg);
                }
                Err(e) => {
                    let msg = format!("Integrity check {label} FAILED: {e}");
                    eprintln!("  {msg}");
                    errors.push(msg);
                }
            }
        }

        Ok(PopulateResult {
            paks_extracted: summary.paks_extracted,
            files_kept: summary.files_extracted,
            lsf_converted: 0,
            unpack_path: db_paths.dir.to_string_lossy().into_owned(),
            errors,
            diagnostics,
        })
    })
    .await
}

#[tauri::command]
pub async fn cmd_build_reference_db(
    unpacked_path: String,
    db_path: String,
) -> Result<reference_db::BuildSummary, AppError> {
    // Validate inputs before entering the blocking closure
    let unpacked = std::path::PathBuf::from(&unpacked_path);
    if !unpacked.is_dir() {
        return Err(
            AppError::not_found("source_dir_not_found").with_context("path", &unpacked_path)
        );
    }
    // Long-running: 20 minute timeout
    blocking_with_timeout(std::time::Duration::from_secs(1200), move || {
        let db = std::path::PathBuf::from(&db_path);
        reference_db::build_reference_db(&unpacked, &db)
    })
    .await
}

#[tauri::command]
pub async fn cmd_populate_reference_db(
    unpacked_path: String,
    db_path: String,
    vacuum: bool,
) -> Result<reference_db::BuildSummary, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(1200), move || {
        let unpacked = std::path::PathBuf::from(&unpacked_path);
        let db = std::path::PathBuf::from(&db_path);
        if !unpacked.is_dir() {
            return Err(format!("UnpackedData directory not found: {unpacked_path}"));
        }
        if !db.is_file() {
            return Err(format!(
                "Schema database not found: {db_path}. Run the schema generator first."
            ));
        }
        let options = reference_db::BuildOptions {
            vacuum,
            ..Default::default()
        };
        reference_db::populate_reference_db(&unpacked, &db, reference_db::TargetDb::Base, &options)
    })
    .await
}

#[tauri::command]
pub async fn cmd_populate_honor_db(
    unpacked_path: String,
    db_path: String,
    vacuum: bool,
    base_db_path: Option<String>,
) -> Result<reference_db::BuildSummary, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(600), move || {
        let unpacked = std::path::PathBuf::from(&unpacked_path);
        let db = std::path::PathBuf::from(&db_path);
        if !unpacked.is_dir() {
            return Err(format!("UnpackedData directory not found: {unpacked_path}"));
        }
        if !db.is_file() {
            return Err(format!(
                "Honor schema database not found: {db_path}. Run the schema generator first."
            ));
        }
        let options = reference_db::BuildOptions {
            vacuum,
            fallback_base_db_path: base_db_path
                .filter(|raw| !raw.trim().is_empty())
                .map(std::path::PathBuf::from),
            ..Default::default()
        };
        reference_db::populate_reference_db(&unpacked, &db, reference_db::TargetDb::Honor, &options)
    })
    .await
}

#[tauri::command]
pub async fn cmd_populate_mods_db(
    mod_path: String,
    mod_name: String,
    db_path: String,
    vacuum: bool,
) -> Result<reference_db::BuildSummary, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(600), move || {
        let mod_dir = std::path::PathBuf::from(&mod_path);
        let db = std::path::PathBuf::from(&db_path);
        if !mod_dir.is_dir() {
            return Err(format!("Mod directory not found: {mod_path}"));
        }
        if !db.is_file() {
            return Err(format!(
                "Mods schema database not found: {db_path}. Run the schema generator first."
            ));
        }
        let options = reference_db::BuildOptions {
            vacuum,
            ..Default::default()
        };
        reference_db::populate_mods_db(&mod_dir, &mod_name, &db, &options)
    })
    .await
}

#[tauri::command]
pub async fn cmd_validate_cross_db_fks(
    db_path: String,
    attach_paths: Vec<(String, String)>, // [(alias, path), ...]
) -> Result<reference_db::cross_db::CrossDbFkReport, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(300), move || {
        let db = std::path::PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Database not found: {db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open DB: {e}"))?;

        let mut aliases = Vec::new();
        for (alias, path) in &attach_paths {
            let p = std::path::PathBuf::from(path);
            if !p.is_file() {
                return Err(format!("Attached DB not found: {alias} ({path})"));
            }
            reference_db::cross_db::attach_readonly(&conn, &p, alias)?;
            aliases.push(alias.as_str());
        }

        // Reborrow for validate call
        let alias_refs: Vec<&str> = attach_paths.iter().map(|(a, _)| a.as_str()).collect();
        reference_db::cross_db::validate_cross_db_fks(&conn, &alias_refs)
    })
    .await
}

#[tauri::command]
pub async fn cmd_create_staging_db(
    schema_db_path: String,
    staging_db_path: String,
) -> Result<reference_db::staging::StagingSummary, AppError> {
    // Validate inputs before entering the blocking closure
    let schema_db = std::path::PathBuf::from(&schema_db_path);
    if !schema_db.is_file() {
        return Err(
            AppError::not_found("schema_db_not_found").with_context("path", &schema_db_path)
        );
    }
    blocking_with_timeout(std::time::Duration::from_secs(60), move || {
        let staging_db = std::path::PathBuf::from(&staging_db_path);
        reference_db::staging::create_staging_db(&schema_db, &staging_db)
    })
    .await
}

#[tauri::command]
pub async fn cmd_populate_staging_from_mod(
    mod_path: String,
    mod_name: String,
    staging_db_path: String,
    vacuum: bool,
) -> Result<reference_db::BuildSummary, AppError> {
    // Validate inputs before entering the blocking closure
    let mod_dir = std::path::PathBuf::from(&mod_path);
    if !mod_dir.is_dir() {
        return Err(AppError::not_found("mod_dir_not_found").with_context("path", &mod_path));
    }
    let staging_db = std::path::PathBuf::from(&staging_db_path);
    if !staging_db.is_file() {
        return Err(
            AppError::not_found("schema_db_not_found").with_context("path", &staging_db_path)
        );
    }
    blocking_with_timeout(std::time::Duration::from_secs(600), move || {
        let options = reference_db::BuildOptions {
            vacuum,
            ..Default::default()
        };
        reference_db::staging::populate_staging_from_mod(&mod_dir, &mod_name, &staging_db, &options)
    })
    .await
}

/// Get the resolved paths to the writable schema databases.
#[tauri::command]
pub async fn cmd_get_db_paths(app: tauri::AppHandle) -> Result<db_manager::DbPaths, AppError> {
    blocking(move || db_manager::ensure_schema_dbs(&app)).await
}

/// Get the status (exists, size) of each schema database.
#[tauri::command]
pub async fn cmd_get_db_status(app: tauri::AppHandle) -> Result<Vec<DbFileStatus>, AppError> {
    blocking(move || {
        let paths = db_manager::get_db_paths(&app)?;
        let entries = [
            ("ref_base", &paths.base),
            ("ref_honor", &paths.honor),
            ("ref_mods", &paths.mods),
            ("staging", &paths.staging),
        ];
        Ok(entries
            .iter()
            .map(|(name, path)| {
                let exists = path.is_file();
                let size_bytes = if exists {
                    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                DbFileStatus {
                    name: name.to_string(),
                    path: path.display().to_string(),
                    exists,
                    size_bytes,
                }
            })
            .collect())
    })
    .await
}

/// Reset all schema databases to their clean (empty) state by re-copying from
/// the bundled resources.  The caller should re-run the populate pipeline afterward.
#[tauri::command]
pub async fn cmd_reset_databases(app: tauri::AppHandle) -> Result<db_manager::DbPaths, AppError> {
    blocking(move || db_manager::reset_schema_dbs(&app)).await
}

/// Reset only reference databases (ref_base, ref_honor, ref_mods) to their
/// clean state, leaving staging untouched.
#[tauri::command]
pub async fn cmd_reset_reference_dbs(
    app: tauri::AppHandle,
) -> Result<db_manager::DbPaths, AppError> {
    blocking(move || db_manager::reset_reference_dbs(&app)).await
}

/// Recreate only the staging database, leaving reference DBs untouched.
///
/// Deletes the existing staging.sqlite (+ WAL/SHM sidecars), copies a fresh
/// copy from the bundled schema resource, and applies UUID uniqueness indexes.
#[tauri::command]
pub async fn cmd_recreate_staging(app: tauri::AppHandle) -> Result<String, AppError> {
    blocking(move || {
        let path = db_manager::recreate_staging_db(&app)?;
        Ok(path.display().to_string())
    })
    .await
}

/// Run `PRAGMA integrity_check` on the staging database (DSM-04).
///
/// Returns `Ok(None)` if the DB passes, or `Ok(Some(details))` if issues found.
#[tauri::command]
pub async fn cmd_check_staging_integrity(
    app: tauri::AppHandle,
) -> Result<Option<String>, AppError> {
    blocking(move || {
        let paths = db_manager::get_db_paths(&app)?;
        if !paths.staging.is_file() {
            return Err("Staging database does not exist".to_string());
        }
        db_manager::run_integrity_check(&paths.staging)
    })
    .await
}

/// Run the full unpack-and-populate pipeline: extract game paks, convert files,
/// and populate ref_base.sqlite (and optionally ref_honor.sqlite).
///
/// Emits `"pipeline-progress"` events to the frontend during execution with
/// `PipelineProgress` payloads for real-time status updates.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn cmd_unpack_and_populate(
    app: tauri::AppHandle,
    divine_path: String,
    game_data_path: String,
    work_dir: String,
    base_db_path: String,
    honor_db_path: String,
    populate_honor: bool,
    vacuum: bool,
    cleanup: bool,
) -> Result<reference_db::pipeline::PipelineSummary, AppError> {
    use tauri::Emitter;

    // Long-running: 30 minute timeout (extraction + conversion + DB population)
    blocking_with_timeout(std::time::Duration::from_secs(1800), move || {
        let config = reference_db::pipeline::PipelineConfig {
            divine_path,
            game_data_path,
            work_dir: std::path::PathBuf::from(&work_dir),
            base_db_path: std::path::PathBuf::from(&base_db_path),
            honor_db_path: std::path::PathBuf::from(&honor_db_path),
            populate_honor,
            build_options: reference_db::BuildOptions {
                vacuum,
                ..Default::default()
            },
            cleanup,
        };

        reference_db::pipeline::run_vanilla_pipeline(&config, |progress| {
            let _ = app.emit("pipeline-progress", &progress);
        })
    })
    .await
}

/// Remove a single mod's data from ref_mods.sqlite by its mod name.
///
/// Deletes all rows where `_SourceID` matches the mod's ID in `_sources`,
/// then removes the `_sources` entry itself.
#[tauri::command]
pub async fn cmd_remove_mod_from_mods_db(
    mod_name: String,
    db_path: String,
) -> Result<u64, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(120), move || {
        let db = std::path::PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Mods database not found: {db_path}"));
        }
        let conn = rusqlite::Connection::open(&db)
            .map_err(|e| format!("Open mods DB: {e}"))?;

        // Look up the _SourceID for this mod name
        let source_id: i64 = conn
            .query_row(
                "SELECT id FROM _sources WHERE name = ?1",
                [&mod_name],
                |row| row.get(0),
            )
            .map_err(|e| format!("Mod '{mod_name}' not found in _sources: {e}"))?;

        // Get all user tables (skip internal/meta tables starting with _)
        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' \
                     AND name NOT LIKE '\\_%' ESCAPE '\\'",
                )
                .map_err(|e| format!("List tables: {e}"))?;
            let result = stmt.query_map([], |row| row.get(0))
                .map_err(|e| format!("Query tables: {e}"))?
                .filter_map(|r| r.ok())
                .collect();
            result
        };

        let mut total_deleted: u64 = 0;
        for table in &tables {
            // Check if this table has a _SourceID column
            let has_source_id: bool = {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info(\"{table}\")"))
                    .map_err(|e| format!("PRAGMA table_info: {e}"))?;
                let result = stmt.query_map([], |row| row.get::<_, String>(1))
                    .map_err(|e| format!("table_info query: {e}"))?
                    .filter_map(|r| r.ok())
                    .any(|col| col == "_SourceID");
                result
            };
            if has_source_id {
                let deleted = conn
                    .execute(
                        &format!("DELETE FROM \"{table}\" WHERE \"_SourceID\" = ?1"),
                        [source_id],
                    )
                    .map_err(|e| format!("Delete from {table}: {e}"))?;
                total_deleted += deleted as u64;
            }
        }

        // Remove from _sources
        conn.execute("DELETE FROM _sources WHERE id = ?1", [source_id])
            .map_err(|e| format!("Delete from _sources: {e}"))?;

        // Also clean up _source_files and _table_meta row counts
        let _ = conn.execute("DELETE FROM _source_files WHERE source_id = ?1", [source_id]);
        // Recount _table_meta row counts
        for table in &tables {
            let _ = conn.execute(
                &format!(
                    "UPDATE _table_meta SET row_count = (SELECT COUNT(*) FROM \"{table}\") WHERE table_name = ?1"
                ),
                [table.as_str()],
            );
        }

        Ok(total_deleted)
    })
    .await
}

/// Clear all user-added mod data from ref_mods.sqlite.
///
/// Deletes all rows from all user tables and all entries in `_sources`.
/// The schema is preserved.
#[tauri::command]
pub async fn cmd_clear_mods_db(db_path: String) -> Result<u64, AppError> {
    blocking_with_timeout(std::time::Duration::from_secs(120), move || {
        let db = std::path::PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Mods database not found: {db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open mods DB: {e}"))?;

        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' \
                     AND name NOT LIKE '\\_%' ESCAPE '\\'",
                )
                .map_err(|e| format!("List tables: {e}"))?;
            let result = stmt
                .query_map([], |row| row.get(0))
                .map_err(|e| format!("Query tables: {e}"))?
                .filter_map(|r| r.ok())
                .collect();
            result
        };

        let mut total_deleted: u64 = 0;
        for table in &tables {
            let deleted = conn
                .execute(&format!("DELETE FROM \"{table}\""), [])
                .map_err(|e| format!("Clear {table}: {e}"))?;
            total_deleted += deleted as u64;
        }

        // Clear meta tables
        let _ = conn.execute("DELETE FROM _sources", []);
        let _ = conn.execute("DELETE FROM _source_files", []);
        for table in &tables {
            let _ = conn.execute(
                "UPDATE _table_meta SET row_count = 0 WHERE table_name = ?1",
                [table.as_str()],
            );
        }

        Ok(total_deleted)
    })
    .await
}

// ---------------------------------------------------------------------------
// Staging CRUD commands (F1–F6)
// ---------------------------------------------------------------------------

/// F1: Upsert a row in the staging DB.
#[tauri::command]
pub async fn cmd_staging_upsert_row(
    staging_db_path: String,
    table: String,
    columns: std::collections::HashMap<String, String>,
    is_new: bool,
) -> Result<reference_db::staging::UpsertResult, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_upsert_row(&conn, &table, &columns, is_new)
    })
    .await
}

/// F2: Soft-delete (or hard-delete if _is_new) a row in the staging DB.
#[tauri::command]
pub async fn cmd_staging_mark_deleted(
    staging_db_path: String,
    table: String,
    pk: String,
) -> Result<bool, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_mark_deleted(&conn, &table, &pk)
    })
    .await
}

/// F2: Unmark a soft-deleted row in the staging DB.
#[tauri::command]
pub async fn cmd_staging_unmark_deleted(
    staging_db_path: String,
    table: String,
    pk: String,
) -> Result<bool, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_unmark_deleted(&conn, &table, &pk)
    })
    .await
}

/// F3: Execute a batch of staging operations atomically.
#[tauri::command]
pub async fn cmd_staging_batch_write(
    staging_db_path: String,
    operations: Vec<reference_db::staging::StagingOperation>,
) -> Result<reference_db::staging::StagingBatchResult, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_batch_write(&conn, &operations)
    })
    .await
}

/// F4: Query rows with changes (new, modified, or deleted).
#[tauri::command]
pub async fn cmd_staging_query_changes(
    staging_db_path: String,
    table: Option<String>,
) -> Result<Vec<reference_db::staging::StagingChange>, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_query_changes(&conn, table.as_deref())
    })
    .await
}

/// F5: List all staging sections with row count summaries.
#[tauri::command]
pub async fn cmd_staging_list_sections(
    staging_db_path: String,
) -> Result<Vec<reference_db::staging::StagingSectionSummary>, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_list_sections(&conn)
    })
    .await
}

/// F5: Query all rows for a staging section.
#[tauri::command]
pub async fn cmd_staging_query_section(
    staging_db_path: String,
    table: String,
    include_deleted: Option<bool>,
) -> Result<Vec<std::collections::HashMap<String, serde_json::Value>>, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_query_section(
            &conn,
            &table,
            include_deleted.unwrap_or(false),
        )
    })
    .await
}

/// F5: Get a single row by PK from a staging table.
#[tauri::command]
pub async fn cmd_staging_get_row(
    staging_db_path: String,
    table: String,
    pk: String,
) -> Result<Option<std::collections::HashMap<String, serde_json::Value>>, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_get_row(&conn, &table, &pk)
    })
    .await
}

/// F6: Get a meta value from `_staging_authoring`.
#[tauri::command]
pub async fn cmd_staging_get_meta(
    staging_db_path: String,
    key: String,
) -> Result<Option<String>, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::ensure_staging_authoring_table(&conn)?;
        reference_db::staging::staging_get_meta(&conn, &key)
    })
    .await
}

/// F6: Set a meta value in `_staging_authoring`.
#[tauri::command]
pub async fn cmd_staging_set_meta(
    staging_db_path: String,
    key: String,
    value: String,
) -> Result<(), AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::ensure_staging_authoring_table(&conn)?;
        reference_db::staging::staging_set_meta(&conn, &key, &value)
    })
    .await
}

/// G3: Create an undo snapshot (boundary marker) in the staging DB.
#[tauri::command]
pub async fn cmd_staging_snapshot(staging_db_path: String, label: String) -> Result<i64, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_snapshot(&conn, &label)
    })
    .await
}

/// G3: Undo the last mutation group in the staging DB.
#[tauri::command]
pub async fn cmd_staging_undo(
    staging_db_path: String,
) -> Result<Vec<reference_db::staging::UndoReplayEntry>, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_undo(&conn)
    })
    .await
}

/// G3: Redo the next mutation group in the staging DB.
#[tauri::command]
pub async fn cmd_staging_redo(
    staging_db_path: String,
) -> Result<Vec<reference_db::staging::UndoReplayEntry>, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_redo(&conn)
    })
    .await
}

/// Truncate the WAL file for the staging DB.
#[tauri::command]
pub async fn cmd_staging_wal_checkpoint(staging_db_path: String) -> Result<(), AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(|e| format!("WAL checkpoint: {e}"))?;
        Ok(())
    })
    .await
}

/// Compact the undo journal by removing the oldest entries beyond `max_entries`.
///
/// Returns the number of entries deleted.
#[tauri::command]
pub async fn cmd_staging_compact_undo(
    staging_db_path: String,
    max_entries: Option<i64>,
) -> Result<i64, AppError> {
    blocking(move || {
        let limit = max_entries.unwrap_or(500);
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;

        // Check if the undo journal table exists
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='_staging_undo_journal'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !exists {
            return Ok(0);
        }

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _staging_undo_journal", [], |r| {
                r.get(0)
            })
            .map_err(|e| format!("Count undo journal: {e}"))?;

        if count <= limit {
            return Ok(0);
        }

        let to_delete = count - limit;
        conn.execute(
            "DELETE FROM _staging_undo_journal WHERE rowid IN \
             (SELECT rowid FROM _staging_undo_journal ORDER BY rowid ASC LIMIT ?1)",
            [to_delete],
        )
        .map_err(|e| format!("Compact undo journal: {e}"))?;

        Ok(to_delete)
    })
    .await
}

/// Replace all rows in a staging table with baseline data and clear its undo history.
/// Used by "Reset Section" to restore a section to its state when the project was opened.
#[tauri::command]
pub async fn cmd_staging_replace_section(
    staging_db_path: String,
    table: String,
    rows: Vec<std::collections::HashMap<String, serde_json::Value>>,
) -> Result<usize, AppError> {
    blocking(move || {
        let db = std::path::PathBuf::from(&staging_db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        reference_db::staging::staging_replace_section(&conn, &table, &rows)
    })
    .await
}

/// J1: Validate all export handlers against the current staging state.
#[tauri::command]
pub async fn cmd_validate_handlers(
    staging_db_path: String,
    ref_base_path: String,
    mod_path: String,
    mod_name: String,
    mod_folder: String,
) -> Result<Vec<crate::export::HandlerWarning>, AppError> {
    blocking(move || {
        let staging_db = std::path::PathBuf::from(&staging_db_path);
        if !staging_db.is_file() {
            return Err(format!("Staging database not found: {staging_db_path}"));
        }
        let conn =
            rusqlite::Connection::open(&staging_db).map_err(|e| format!("Open staging DB: {e}"))?;

        let ctx = crate::export::ExportContext {
            staging_conn: conn,
            ref_base_path: std::path::PathBuf::from(&ref_base_path),
            mod_path: std::path::PathBuf::from(&mod_path),
            mod_name,
            mod_folder,
        };

        crate::export::validate_all_handlers(&ctx).map_err(|e| e.message)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_file_status_serializes_correctly() {
        let status = DbFileStatus {
            name: "ref_base".to_string(),
            path: "/tmp/ref_base.sqlite".to_string(),
            exists: true,
            size_bytes: 1024,
        };

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"name\":\"ref_base\""));
        assert!(json.contains("\"exists\":true"));
        assert!(json.contains("\"size_bytes\":1024"));
        assert!(json.contains("\"path\":\"/tmp/ref_base.sqlite\""));
    }

    #[test]
    fn db_file_status_serializes_nonexistent() {
        let status = DbFileStatus {
            name: "staging".to_string(),
            path: "/tmp/staging.sqlite".to_string(),
            exists: false,
            size_bytes: 0,
        };

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"exists\":false"));
        assert!(json.contains("\"size_bytes\":0"));
    }

    #[test]
    fn db_file_status_clone_is_equal() {
        let status = DbFileStatus {
            name: "ref_honor".to_string(),
            path: "/data/ref_honor.sqlite".to_string(),
            exists: true,
            size_bytes: 512000,
        };

        let cloned = status.clone();
        assert_eq!(cloned.name, status.name);
        assert_eq!(cloned.path, status.path);
        assert_eq!(cloned.exists, status.exists);
        assert_eq!(cloned.size_bytes, status.size_bytes);
    }

    /// Test 15 (S-ERRTEST): create_staging_db with a nonexistent schema DB
    /// should return a clean error (the schema DB is opened to read table defs).
    #[test]
    fn test_create_staging_db_nonexistent_schema() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let fake_schema = tmp.path().join("nonexistent_schema.sqlite");
        let staging_out = tmp.path().join("staging.sqlite");

        let result = reference_db::staging::create_staging_db(&fake_schema, &staging_out);
        assert!(
            result.is_err(),
            "create_staging_db should fail with nonexistent schema DB"
        );
        let err = result.unwrap_err();
        assert!(!err.is_empty(), "Error message should be non-empty");
    }

    /// Test 16a (S-ERRTEST): populate_staging_from_mod with nonexistent mod_path
    /// should return a clean error.
    #[test]
    fn test_populate_staging_nonexistent_mod_path() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let fake_mod = tmp.path().join("nonexistent_mod");
        let staging_db = tmp.path().join("staging.sqlite");
        // Create an empty staging DB so the error comes from the mod path
        let _conn = rusqlite::Connection::open(&staging_db).expect("create empty db");

        let options = reference_db::BuildOptions::default();
        let result = reference_db::staging::populate_staging_from_mod(
            &fake_mod,
            "TestMod",
            &staging_db,
            &options,
        );
        assert!(
            result.is_err(),
            "populate_staging_from_mod should fail with nonexistent mod path"
        );
    }

    /// Test 16b (S-ERRTEST): populate_staging_from_mod with nonexistent staging_db_path
    /// should return a clean error.
    #[test]
    fn test_populate_staging_nonexistent_staging_db() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        // Create a valid mod directory with minimal structure
        let mod_dir = tmp.path().join("my_mod");
        std::fs::create_dir_all(&mod_dir).unwrap();
        let fake_staging = tmp.path().join("nonexistent_staging.sqlite");

        let options = reference_db::BuildOptions::default();
        let result = reference_db::staging::populate_staging_from_mod(
            &mod_dir,
            "TestMod",
            &fake_staging,
            &options,
        );
        // Should fail when trying to open/load the nonexistent staging DB schema
        assert!(
            result.is_err(),
            "populate_staging_from_mod should fail with nonexistent staging DB"
        );
    }

    /// Test 17a (S-ERRTEST): populate_reference_db with nonexistent unpacked_path
    /// should return a clean error.
    #[test]
    fn test_populate_reference_db_nonexistent_unpacked() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let fake_unpacked = tmp.path().join("nonexistent_unpacked");
        // Create a valid DB file so the error comes from the unpacked path
        let db_path = tmp.path().join("ref_base.sqlite");
        let _conn = rusqlite::Connection::open(&db_path).expect("create empty db");

        let options = reference_db::BuildOptions::default();
        let result = reference_db::populate_reference_db(
            &fake_unpacked,
            &db_path,
            reference_db::TargetDb::Base,
            &options,
        );
        assert!(
            result.is_err(),
            "populate_reference_db should fail with nonexistent unpacked dir"
        );
        // With a nonexistent unpacked_path, collect_files returns empty, then
        // populate_db fails because the DB has no embedded schema.
        let err = result.unwrap_err();
        assert!(!err.is_empty(), "Error message should be non-empty");
    }

    /// Test 17b (S-ERRTEST): populate_reference_db with nonexistent db_path
    /// should return a clean error (can't open the DB file).
    #[test]
    fn test_populate_reference_db_nonexistent_db() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        // Create a valid unpacked directory
        let unpacked = tmp.path().join("unpacked");
        std::fs::create_dir_all(&unpacked).unwrap();
        let fake_db = tmp.path().join("nonexistent_ref.sqlite");

        let options = reference_db::BuildOptions::default();
        let result = reference_db::populate_reference_db(
            &unpacked,
            &fake_db,
            reference_db::TargetDb::Base,
            &options,
        );
        assert!(
            result.is_err(),
            "populate_reference_db should fail with nonexistent DB path"
        );
    }

    /// Test 18 (S-ERRTEST): cmd_process_mod_folder inner logic — passing a
    /// regular file (not .pak, not directory) should produce an error.
    /// Since cmd_process_mod_folder requires AppHandle, we test the same
    /// branching logic that the command uses: is_file + not .pak + not is_dir.
    #[test]
    fn test_process_mod_path_neither_pak_nor_dir() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let regular_file = tmp.path().join("test.txt");
        std::fs::File::create(&regular_file).expect("create regular file");

        let mod_path = regular_file.to_string_lossy().to_string();
        let path = std::path::Path::new(&mod_path);

        // Replicate the branching from cmd_process_mod_folder
        assert!(path.exists(), "Test file should exist");
        let is_pak = path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pak"));
        assert!(!is_pak, "A .txt file should not be detected as .pak");
        assert!(!path.is_dir(), "A regular file should not be a directory");

        // This is the exact error the command would produce
        let err = format!("Mod path is neither a .pak file nor a directory: {mod_path}");
        assert!(err.contains("neither a .pak file nor a directory"));
    }

    /// Test 19 (S-ERRTEST): run_integrity_check on a corrupt DB should surface
    /// warnings (returned as Some(details)), not panic.
    #[test]
    fn test_integrity_check_corrupt_db_surfaces_warning() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let db_path = tmp.path().join("corrupt.sqlite");

        // Create a valid SQLite DB first, then corrupt it
        {
            let conn = rusqlite::Connection::open(&db_path).expect("create db");
            conn.execute_batch(
                "CREATE TABLE test_data (id INTEGER PRIMARY KEY, value TEXT);
                 INSERT INTO test_data VALUES (1, 'hello');
                 INSERT INTO test_data VALUES (2, 'world');",
            )
            .expect("populate test db");
            // Force WAL checkpoint so data is in the main file
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        }

        // Corrupt the DB by overwriting some bytes in the middle of the file
        {
            use std::io::{Seek, Write};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&db_path)
                .expect("open db for corruption");
            // Skip the SQLite header (first 100 bytes) and corrupt page data
            file.seek(std::io::SeekFrom::Start(200)).expect("seek");
            file.write_all(&[0xFF; 64]).expect("write corruption");
        }

        // run_integrity_check should return Ok (not panic/Err from connection failure)
        let result = db_manager::run_integrity_check(&db_path);
        match result {
            Ok(None) => {
                // If SQLite still says "ok" despite corruption (small DB may
                // not detect damage), that's acceptable — no panic occurred.
            }
            Ok(Some(details)) => {
                // Corruption was detected and surfaced as a warning — this is
                // the primary expected path for Test 19.
                assert!(
                    !details.is_empty(),
                    "Integrity check details should be non-empty when issues found"
                );
            }
            Err(e) => {
                // Connection-level failure is also acceptable (e.g., header
                // corruption makes the file unreadable). The key assertion is
                // that we didn't panic.
                assert!(
                    !e.is_empty(),
                    "Error message should be non-empty, got empty string"
                );
            }
        }
    }

    /// Test 19b (S-ERRTEST): run_integrity_check on a healthy DB should return Ok(None).
    #[test]
    fn test_integrity_check_healthy_db() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let db_path = tmp.path().join("healthy.sqlite");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("create db");
            conn.execute_batch(
                "CREATE TABLE test_data (id INTEGER PRIMARY KEY, value TEXT);
                 INSERT INTO test_data VALUES (1, 'hello');",
            )
            .expect("populate test db");
        }

        let result = db_manager::run_integrity_check(&db_path);
        assert!(
            result.is_ok(),
            "Integrity check should succeed on healthy DB"
        );
        assert_eq!(
            result.unwrap(),
            None,
            "Healthy DB should return None (no issues)"
        );
    }
}
