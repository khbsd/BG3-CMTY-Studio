//! LSX file export handler.
//!
//! Implements `FileTypeHandler` for `.lsx` file export: plans which files to
//! create/update/delete based on staging DB state, then renders each file by
//! querying rows and producing well-formed LSX XML via `entries_to_lsx()`.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::error::AppError;
use crate::models::LsxEntry;
use crate::serializers::lsx_writer::{entries_to_lsx, reconstruct_lsx_entry};

use super::{
    all_rows_deleted, has_tracked_changes, list_staging_tables, ExportContext, ExportUnit,
    FileAction, FileTypeHandler,
};

pub struct LsxHandler;

impl FileTypeHandler for LsxHandler {
    fn name(&self) -> &str {
        "LsxHandler"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec!["lsx__"]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec![]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let tables = list_staging_tables(&ctx.staging_conn, "lsx__")?;
        let mut units = Vec::new();

        for table in &tables {
            if !has_tracked_changes(&ctx.staging_conn, &table.table_name)? {
                continue;
            }

            let region_id = match &table.region_id {
                Some(r) if !r.is_empty() => r.as_str(),
                _ => continue,
            };

            // output_path is relative to mod_path
            let output_path = std::path::PathBuf::from("Public")
                .join(&ctx.mod_folder)
                .join(region_id)
                .join(format!("{region_id}.lsx"));

            if all_rows_deleted(&ctx.staging_conn, &table.table_name)? {
                units.push(ExportUnit {
                    handler_name: self.name().to_string(),
                    output_path,
                    action: FileAction::Delete,
                    entry_count: 0,
                    content: None,
                });
                continue;
            }

            let entry_count = count_active_rows(&ctx.staging_conn, &table.table_name)?;

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

        Ok(units)
    }

    fn render(&self, unit: &ExportUnit, ctx: &ExportContext) -> Result<Vec<u8>, AppError> {
        let region_id = unit
            .output_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| AppError::internal("Cannot extract region_id from output path"))?;

        let table_name: String = ctx
            .staging_conn
            .query_row(
                "SELECT table_name FROM _table_meta \
                 WHERE source_type = 'lsx' AND region_id = ?1 \
                 AND table_name LIKE 'lsx__%'",
                [region_id],
                |row| row.get(0),
            )
            .map_err(|e| {
                AppError::internal(format!("No LSX table for region '{region_id}': {e}"))
            })?;

        validate_table_name(&table_name)?;

        let node_id: String = ctx
            .staging_conn
            .query_row(
                "SELECT COALESCE(node_id, '') FROM _table_meta WHERE table_name = ?1",
                [&table_name],
                |row| row.get(0),
            )
            .unwrap_or_default();

        let column_types = query_column_types(&ctx.staging_conn, &table_name)?;
        let junction_tables = find_junction_tables(&ctx.staging_conn, &table_name)?;

        let entries = query_lsx_entries(
            &ctx.staging_conn,
            &table_name,
            &node_id,
            &column_types,
            &junction_tables,
        )?;

        let xml = entries_to_lsx(&entries, region_id);
        Ok(xml.into_bytes())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Validate that a table name contains only safe characters (alphanumeric + underscore).
fn validate_table_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(AppError::security(format!(
            "Invalid table name '{name}': only ASCII alphanumeric and underscores allowed"
        )));
    }
    Ok(())
}

/// Count non-deleted rows in a staging table.
fn count_active_rows(conn: &Connection, table_name: &str) -> Result<usize, AppError> {
    validate_table_name(table_name)?;
    let sql = format!("SELECT COUNT(*) FROM \"{table_name}\" WHERE \"_is_deleted\" = 0");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|c| c as usize)
        .map_err(|e| AppError::internal(format!("Count active rows in '{table_name}': {e}")))
}

/// Query `_column_types` for BG3 type mappings for a given table.
fn query_column_types(
    conn: &Connection,
    table_name: &str,
) -> Result<HashMap<String, String>, AppError> {
    let mut stmt = conn
        .prepare("SELECT column_name, bg3_type FROM _column_types WHERE table_name = ?1")
        .map_err(|e| AppError::internal(format!("Prepare column_types query: {e}")))?;

    let rows = stmt
        .query_map([table_name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|e| AppError::internal(format!("Query column_types: {e}")))?;

    let mut map = HashMap::new();
    for row in rows {
        let (col, bg3_type) =
            row.map_err(|e| AppError::internal(format!("Read column_types row: {e}")))?;
        if let Some(t) = bg3_type {
            map.insert(col, t);
        }
    }
    Ok(map)
}

/// Discover junction tables for a parent table.
///
/// Junction tables follow the naming convention `jn__{parent_table}__{group_id}`
/// with columns `parent_id TEXT, child_id TEXT`.
fn find_junction_tables(
    conn: &Connection,
    parent_table: &str,
) -> Result<Vec<(String, String)>, AppError> {
    let prefix = format!("jn__{parent_table}__");
    let pattern = format!("{prefix}%");

    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE ?1")
        .map_err(|e| AppError::internal(format!("Find junction tables: {e}")))?;

    let rows = stmt
        .query_map([&pattern], |row| row.get::<_, String>(0))
        .map_err(|e| AppError::internal(format!("Query junction tables: {e}")))?;

    let mut result = Vec::new();
    for row in rows {
        let jn_name =
            row.map_err(|e| AppError::internal(format!("Read junction table name: {e}")))?;
        validate_table_name(&jn_name)?;
        let group_id = jn_name[prefix.len()..].to_string();
        if !group_id.is_empty() {
            result.push((jn_name, group_id));
        }
    }
    Ok(result)
}

/// Query child GUIDs from all junction tables for a given parent row.
fn query_children(
    conn: &Connection,
    junction_tables: &[(String, String)],
    parent_id: &str,
) -> Result<HashMap<String, Vec<String>>, AppError> {
    let mut children = HashMap::new();

    for (jn_table, group_id) in junction_tables {
        let sql = format!("SELECT child_id FROM \"{jn_table}\" WHERE parent_id = ?1");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::internal(format!("Prepare junction '{jn_table}': {e}")))?;

        let rows = stmt
            .query_map([parent_id], |row| row.get::<_, String>(0))
            .map_err(|e| AppError::internal(format!("Query junction '{jn_table}': {e}")))?;

        let mut child_ids = Vec::new();
        for row in rows {
            child_ids
                .push(row.map_err(|e| AppError::internal(format!("Read junction child: {e}")))?);
        }

        if !child_ids.is_empty() {
            children.insert(group_id.clone(), child_ids);
        }
    }

    Ok(children)
}

/// Collect raw `(pk_value, attributes)` pairs from a staging table.
///
/// A raw row: (rowid string, column_name → value map).
type RawRow = (String, HashMap<String, String>);

/// Rows are collected eagerly so the statement borrow on `conn` is released
/// before junction-table queries run for each row.
fn collect_raw_rows(conn: &Connection, table_name: &str) -> Result<Vec<RawRow>, AppError> {
    let sql = format!("SELECT * FROM \"{table_name}\" WHERE \"_is_deleted\" = 0");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| AppError::internal(format!("Prepare query for '{table_name}': {e}")))?;

    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();

    // LSX tables always use UUID or MapKey as the PK column.
    let pk_col = col_names
        .iter()
        .find(|c| c.as_str() == "UUID")
        .or_else(|| col_names.iter().find(|c| c.as_str() == "MapKey"))
        .cloned()
        .unwrap_or_else(|| "UUID".to_string());

    let mapped = stmt
        .query_map([], |row| {
            let mut raw_attributes = HashMap::new();
            let mut pk_value = String::new();

            for (i, col_name) in col_names.iter().enumerate() {
                // Skip internal/tracking columns (prefixed with `_`)
                if col_name.starts_with('_') {
                    continue;
                }

                let value: String = row.get::<_, Option<String>>(i)?.unwrap_or_default();

                if *col_name == pk_col {
                    pk_value = value.clone();
                }

                raw_attributes.insert(col_name.clone(), value);
            }

            Ok((pk_value, raw_attributes))
        })
        .map_err(|e| AppError::internal(format!("Query '{table_name}': {e}")))?;

    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("Read rows from '{table_name}': {e}")))
}

/// Query all non-deleted rows from a staging table and reconstruct `LsxEntry`
/// values with attribute types and child relationships from junction tables.
fn query_lsx_entries(
    conn: &Connection,
    table_name: &str,
    node_id: &str,
    column_types: &HashMap<String, String>,
    junction_tables: &[(String, String)],
) -> Result<Vec<LsxEntry>, AppError> {
    let raw_rows = collect_raw_rows(conn, table_name)?;
    let mut entries = Vec::with_capacity(raw_rows.len());

    for (pk_value, raw_attributes) in &raw_rows {
        let raw_attribute_types: HashMap<String, String> = raw_attributes
            .keys()
            .filter_map(|col| column_types.get(col).map(|t| (col.clone(), t.clone())))
            .collect();

        let raw_children = query_children(conn, junction_tables, pk_value)?;

        let entry = reconstruct_lsx_entry(
            pk_value,
            node_id,
            raw_attributes,
            &raw_attribute_types,
            &raw_children,
        );

        entries.push(entry);
    }

    Ok(entries)
}
