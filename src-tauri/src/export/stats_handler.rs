//! Stats `.txt` file export handler.
//!
//! Implements [`FileTypeHandler`] for BG3 Stats data tables (`stats__*`).
//! Each staging table whose name starts with `stats__` is claimed by this
//! handler.  At plan time, tables are grouped by source file (via `_file_id`
//! → `_source_files`).  At render time, rows are reconstructed into
//! [`StatsEntry`] values and serialized through [`write_stats_file`].

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::params;

use crate::error::AppError;
use crate::models::StatsEntry;
use crate::parsers::stats_txt::write_stats_file;

use super::{
    all_rows_deleted, has_tracked_changes, list_staging_tables, ExportContext, ExportUnit,
    FileAction, FileTypeHandler,
};

/// Meta columns that are never exported as data fields.
const META_COLUMNS: &[&str] = &[
    "_entry_name",
    "_file_id",
    "_SourceID",
    "_type",
    "_using",
    "_is_modified",
    "_is_new",
    "_is_deleted",
];

pub struct StatsHandler;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Validate that a table/identifier name contains only safe characters
/// (ASCII alphanumeric + underscore).
fn validate_identifier(name: &str) -> Result<(), AppError> {
    if name.is_empty() {
        return Err(AppError::invalid_input("Identifier must not be empty"));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(AppError::security(format!(
            "Invalid identifier '{name}': only ASCII alphanumeric + underscore allowed"
        )));
    }
    Ok(())
}

/// Derive a default output filename from a stats table name.
/// e.g. `stats__Spell` → `Spell`
fn entry_type_from_table(table_name: &str) -> String {
    table_name
        .strip_prefix("stats__")
        .unwrap_or(table_name)
        .to_string()
}

/// Resolve the output path for a stats file within the mod folder.
///
/// If a `_source_files` row exists for the given `file_id`, its `path` column
/// is used.  Otherwise a default path based on the entry type is generated.
fn resolve_output_path(
    conn: &rusqlite::Connection,
    file_id: Option<i64>,
    entry_type: &str,
    mod_folder: &str,
) -> PathBuf {
    if let Some(fid) = file_id {
        if let Ok(path) = conn.query_row(
            "SELECT path FROM _source_files WHERE file_id = ?1",
            params![fid],
            |row| row.get::<_, String>(0),
        ) {
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
    }
    PathBuf::from(format!(
        "Public/{mod_folder}/Stats/Generated/Data/{entry_type}.txt"
    ))
}

/// Discover data-column names for a stats table (excludes meta columns).
fn data_columns(conn: &rusqlite::Connection, table_name: &str) -> Result<Vec<String>, AppError> {
    validate_identifier(table_name)?;

    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table_name}\")"))
        .map_err(|e| AppError::internal(format!("PRAGMA table_info: {e}")))?;

    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AppError::internal(format!("column query: {e}")))?
        .filter_map(|r| r.ok())
        .filter(|name| !META_COLUMNS.contains(&name.as_str()))
        .collect();

    Ok(cols)
}

/// Strips inherited fields from entries whose parent exists in the vanilla
/// ref-base DB.  Only fields that **differ** from the parent are retained.
fn strip_inherited_fields(
    entries: &mut [StatsEntry],
    table_name: &str,
    ctx: &ExportContext,
) -> Result<(), AppError> {
    let parent_names: Vec<String> = entries
        .iter()
        .filter_map(|e| e.parent.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if parent_names.is_empty() {
        return Ok(());
    }

    let ref_path = ctx.ref_base_path.to_string_lossy().replace('\'', "''");

    ctx.staging_conn
        .execute_batch(&format!("ATTACH DATABASE '{ref_path}' AS ref_base"))
        .map_err(|e| AppError::internal(format!("ATTACH ref_base: {e}")))?;

    let ref_table_exists: bool = ctx
        .staging_conn
        .query_row(
            "SELECT COUNT(*) FROM ref_base.sqlite_master \
             WHERE type = 'table' AND name = ?1",
            params![table_name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if !ref_table_exists {
        ctx.staging_conn
            .execute_batch("DETACH DATABASE ref_base")
            .ok();
        return Ok(());
    }

    let data_cols = data_columns(&ctx.staging_conn, table_name)?;
    let mut parent_data: HashMap<String, HashMap<String, String>> = HashMap::new();

    for parent_name in &parent_names {
        let sql = format!("SELECT * FROM ref_base.\"{table_name}\" WHERE _entry_name = ?1");
        let mut stmt = match ctx.staging_conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("").to_string())
            .collect();

        let result = stmt.query_row(params![parent_name], |row| {
            let mut pdata = HashMap::new();
            for dc in &data_cols {
                if let Some(idx) = col_names.iter().position(|c| c == dc) {
                    if let Ok(Some(val)) = row.get::<_, Option<String>>(idx) {
                        pdata.insert(dc.clone(), val);
                    }
                }
            }
            Ok(pdata)
        });

        if let Ok(pdata) = result {
            parent_data.insert(parent_name.clone(), pdata);
        }
    }

    ctx.staging_conn
        .execute_batch("DETACH DATABASE ref_base")
        .map_err(|e| AppError::internal(format!("DETACH ref_base: {e}")))?;

    for entry in entries.iter_mut() {
        if let Some(ref parent_name) = entry.parent {
            if let Some(pdata) = parent_data.get(parent_name) {
                entry.data.retain(|key, val| match pdata.get(key) {
                    Some(parent_val) => val != parent_val,
                    None => true,
                });
            }
        }
    }

    Ok(())
}

// ── FileTypeHandler implementation ──────────────────────────────────────────

impl FileTypeHandler for StatsHandler {
    fn name(&self) -> &str {
        "StatsHandler"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec!["stats__"]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec![]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let tables = list_staging_tables(&ctx.staging_conn, "stats__")?;
        let mut units: Vec<ExportUnit> = Vec::new();

        for table_info in &tables {
            validate_identifier(&table_info.table_name)?;

            if !has_tracked_changes(&ctx.staging_conn, &table_info.table_name)? {
                continue;
            }

            let entry_type = entry_type_from_table(&table_info.table_name);

            let all_deleted = all_rows_deleted(&ctx.staging_conn, &table_info.table_name)?;

            if all_deleted {
                let output_path =
                    resolve_output_path(&ctx.staging_conn, None, &entry_type, &ctx.mod_folder);
                units.push(ExportUnit {
                    handler_name: self.name().to_string(),
                    output_path,
                    action: FileAction::Delete,
                    entry_count: 0,
                    content: None,
                });
                continue;
            }

            // Discover distinct file_ids among non-deleted rows.
            let sql = format!(
                "SELECT DISTINCT _file_id FROM \"{}\" WHERE _is_deleted = 0",
                table_info.table_name
            );
            let mut stmt = ctx
                .staging_conn
                .prepare(&sql)
                .map_err(|e| AppError::internal(format!("plan prepare: {e}")))?;

            let file_ids: Vec<Option<i64>> = stmt
                .query_map([], |row| row.get::<_, Option<i64>>(0))
                .map_err(|e| AppError::internal(format!("plan query: {e}")))?
                .filter_map(|r| r.ok())
                .collect();

            if file_ids.is_empty() {
                continue;
            }

            // Group by output path, accumulating entry counts.
            let mut path_counts: HashMap<PathBuf, usize> = HashMap::new();

            for file_id in &file_ids {
                let output_path =
                    resolve_output_path(&ctx.staging_conn, *file_id, &entry_type, &ctx.mod_folder);

                let count: usize = if file_id.is_some() {
                    ctx.staging_conn
                        .query_row(
                            &format!(
                                "SELECT COUNT(*) FROM \"{}\" WHERE _is_deleted = 0 AND _file_id = ?1",
                                table_info.table_name
                            ),
                            params![file_id],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap_or(0) as usize
                } else {
                    ctx.staging_conn
                        .query_row(
                            &format!(
                                "SELECT COUNT(*) FROM \"{}\" WHERE _is_deleted = 0 AND _file_id IS NULL",
                                table_info.table_name
                            ),
                            [],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap_or(0) as usize
                };

                *path_counts.entry(output_path).or_insert(0) += count;
            }

            for (output_path, entry_count) in path_counts {
                let action = if ctx.mod_path.join(&output_path).exists() {
                    FileAction::Update
                } else {
                    FileAction::Create
                };

                units.push(ExportUnit {
                    handler_name: self.name().to_string(),
                    output_path,
                    action,
                    entry_count,
                    content: None,
                });
            }
        }

        Ok(units)
    }

    fn render(&self, unit: &ExportUnit, ctx: &ExportContext) -> Result<Vec<u8>, AppError> {
        let tables = list_staging_tables(&ctx.staging_conn, "stats__")?;
        let mut all_entries: Vec<StatsEntry> = Vec::new();

        for table_info in &tables {
            validate_identifier(&table_info.table_name)?;

            let entry_type = entry_type_from_table(&table_info.table_name);
            let data_cols = data_columns(&ctx.staging_conn, &table_info.table_name)?;

            // Discover file_ids that map to this unit's output path.
            let sql = format!(
                "SELECT DISTINCT _file_id FROM \"{}\" WHERE _is_deleted = 0",
                table_info.table_name
            );
            let mut stmt = ctx
                .staging_conn
                .prepare(&sql)
                .map_err(|e| AppError::internal(format!("render prepare: {e}")))?;

            let file_ids: Vec<Option<i64>> = stmt
                .query_map([], |row| row.get::<_, Option<i64>>(0))
                .map_err(|e| AppError::internal(format!("render query: {e}")))?
                .filter_map(|r| r.ok())
                .collect();

            for file_id in &file_ids {
                let resolved =
                    resolve_output_path(&ctx.staging_conn, *file_id, &entry_type, &ctx.mod_folder);

                if resolved != unit.output_path {
                    continue;
                }

                // Query matching rows.
                let (row_sql, has_param) = if file_id.is_some() {
                    (
                        format!(
                            "SELECT * FROM \"{}\" WHERE _is_deleted = 0 AND _file_id = ?1",
                            table_info.table_name
                        ),
                        true,
                    )
                } else {
                    (
                        format!(
                            "SELECT * FROM \"{}\" WHERE _is_deleted = 0 AND _file_id IS NULL",
                            table_info.table_name
                        ),
                        false,
                    )
                };

                let mut row_stmt = ctx
                    .staging_conn
                    .prepare(&row_sql)
                    .map_err(|e| AppError::internal(format!("render row prepare: {e}")))?;

                let col_count = row_stmt.column_count();
                let col_names: Vec<String> = (0..col_count)
                    .map(|i| row_stmt.column_name(i).unwrap_or("").to_string())
                    .collect();

                // Build closure that maps a row to StatsEntry.
                let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<StatsEntry> {
                    let entry_name: String = row.get(
                        col_names
                            .iter()
                            .position(|c| c == "_entry_name")
                            .unwrap_or(0),
                    )?;
                    let etype: Option<String> =
                        row.get(col_names.iter().position(|c| c == "_type").unwrap_or(0))?;
                    let using: Option<String> =
                        row.get(col_names.iter().position(|c| c == "_using").unwrap_or(0))?;

                    let mut data = HashMap::new();
                    for dc in &data_cols {
                        if let Some(idx) = col_names.iter().position(|c| c == dc) {
                            if let Ok(Some(val)) = row.get::<_, Option<String>>(idx) {
                                if !val.is_empty() {
                                    data.insert(dc.clone(), val);
                                }
                            }
                        }
                    }

                    Ok(StatsEntry {
                        name: entry_name,
                        entry_type: etype.unwrap_or_else(|| entry_type.clone()),
                        parent: using.filter(|s| !s.is_empty()),
                        data,
                    })
                };

                let rows_iter = if has_param {
                    row_stmt
                        .query_map(params![file_id], map_row)
                        .map_err(|e| AppError::internal(format!("render rows: {e}")))?
                } else {
                    row_stmt
                        .query_map([], map_row)
                        .map_err(|e| AppError::internal(format!("render rows: {e}")))?
                };

                let mut entries: Vec<StatsEntry> = Vec::new();
                for r in rows_iter {
                    entries.push(r.map_err(|e| AppError::internal(format!("render row: {e}")))?);
                }

                // Strip inherited fields by comparing against vanilla parent.
                strip_inherited_fields(&mut entries, &table_info.table_name, ctx)?;

                all_entries.extend(entries);
            }
        }

        let txt = write_stats_file(&all_entries);
        Ok(txt.into_bytes())
    }
}
