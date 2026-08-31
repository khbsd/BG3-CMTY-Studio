//! Staging database — active mod editing workspace.
//!
//! The staging DB is a per-mod SQLite file used for authoring.  It follows
//! the same table schema as `ref_base.sqlite` with three additions per data
//! table:
//!
//!   - `_is_modified` INTEGER DEFAULT 0 — row has been edited in staging
//!   - `_is_new`      INTEGER DEFAULT 0 — row was created in staging (not from vanilla)
//!   - `_is_deleted`  INTEGER DEFAULT 0 — row is marked for deletion on export
//!
//! The staging DB uses WAL mode to allow concurrent reads (UI queries) during
//! writes (user edits).  FK constraints are omitted — cross-DB validation via
//! ATTACH against ref_base handles referential integrity.
//!
//! Lifecycle:
//!   1. `create_staging_db()` — create empty schema with tracking columns
//!   2. (optionally) `populate_staging_from_mod()` — import existing mod files
//!   3. User edits via INSERT/UPDATE/DELETE through Tauri commands
//!   4. Export: query staging rows grouped by table, serialize to LSX/stats/loca

use crate::reference_db::builder::{self, load_schema_from_db};
use crate::reference_db::schema::*;
use crate::reference_db::{BuildOptions, BuildSummary};
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// Summary returned after staging DB creation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StagingSummary {
    pub db_path: String,
    pub total_tables: usize,
    pub junction_tables: usize,
    pub elapsed_secs: f64,
    pub db_size_mb: f64,
}

/// Create a staging database with empty schema + tracking columns.
///
/// `schema_db_path` — path to ref_base.sqlite (schema source).
///                     The embedded schema blob is read from here.
/// `staging_db_path` — output path for the new staging.sqlite file.
///
/// The staging DB gets:
///   - All data tables from the reference schema (same columns)
///   - Three extra tracking columns per data table
///   - No FK constraints (cross-DB ATTACH handles refs to vanilla)
///   - WAL mode for concurrent read/write
///   - Meta tables (_sources, _source_files, _column_types, _table_meta)
///   - Embedded schema blob (same as ref_base for compatibility)
pub fn create_staging_db(
    schema_db_path: &Path,
    staging_db_path: &Path,
) -> Result<StagingSummary, String> {
    let start = Instant::now();

    let schema = load_schema_from_db(schema_db_path)?;

    // Remove existing staging DB and WAL/SHM files
    let _ = std::fs::remove_file(staging_db_path);
    let db_str = staging_db_path.to_string_lossy();
    for suffix in &["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_str}{suffix}"));
    }

    let conn = Connection::open(staging_db_path).map_err(|e| format!("Create staging DB: {e}"))?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA page_size = 8192;
         PRAGMA foreign_keys = OFF;",
    )
    .map_err(|e| format!("Pragma error: {e}"))?;

    // Wrap all DDL + metadata in a single transaction to avoid per-statement fsyncs.
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Begin transaction: {e}"))?;

    // Meta tables (same as reference DB)
    create_staging_meta_tables(&tx)?;

    // Embedded schema storage
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS _embedded_schema (
            key   TEXT PRIMARY KEY,
            value BLOB NOT NULL
         ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("Create _embedded_schema: {e}"))?;

    // Data tables with staging tracking columns, no FK constraints
    for ts in schema.tables.values() {
        create_staging_data_table(&tx, ts)?;
    }

    // Junction tables (no FKs, with tracking columns)
    for jt in &schema.junction_tables {
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS \"{}\" (\n\
             \x20   parent_id {} NOT NULL,\n\
             \x20   child_id {} NOT NULL,\n\
             \x20   \"_is_modified\" INTEGER NOT NULL DEFAULT 0,\n\
             \x20   \"_is_new\" INTEGER NOT NULL DEFAULT 0,\n\
             \x20   \"_is_deleted\" INTEGER NOT NULL DEFAULT 0,\n\
             \x20   PRIMARY KEY (parent_id, child_id)\n\
             ) WITHOUT ROWID",
            jt.table_name, jt.parent_pk_type, jt.child_pk_type,
        );
        tx.execute_batch(&ddl)
            .map_err(|e| format!("Create junction '{}': {}", jt.table_name, e))?;
    }

    // DSM-03: Add UNIQUE indexes on non-PK UUID columns to prevent duplicate entries
    for (table_name, ts) in &schema.tables {
        if ts.pk_strategy != PkStrategy::Uuid {
            for col in &ts.columns {
                if col.name == "UUID" {
                    let idx = format!("uq_{}_{}", table_name, col.name);
                    tx.execute_batch(&format!(
                        "CREATE UNIQUE INDEX IF NOT EXISTS \"{}\" ON \"{}\"(\"{}\")",
                        idx, table_name, col.name
                    ))
                    .map_err(|e| format!("UUID index on {}.{}: {}", table_name, col.name, e))?;
                }
            }
        }
    }

    // Store schema blob (matches ref_base format for pipeline compatibility)
    let blob = rmp_serde::to_vec(&schema).map_err(|e| format!("Serialize schema: {e}"))?;
    tx.execute(
        "INSERT OR REPLACE INTO _embedded_schema (key, value) VALUES ('schema', ?1)",
        rusqlite::params![blob],
    )
    .map_err(|e| format!("Store schema blob: {e}"))?;

    // Populate column types metadata
    populate_staging_column_types(&tx, &schema)?;

    tx.commit()
        .map_err(|e| format!("Commit staging DDL: {e}"))?;

    let total_tables = schema.tables.len();
    let junction_tables = schema.junction_tables.len();

    eprintln!(
        "  Staging DB created: {total_tables} data tables, {junction_tables} junctions (no FKs, WAL mode)",
    );

    // VACUUM the empty DB
    conn.execute_batch("PRAGMA journal_mode = DELETE; VACUUM;")
        .map_err(|e| format!("Vacuum staging DB: {e}"))?;

    conn.close()
        .map_err(|(_conn, e)| format!("Close error: {e}"))?;

    let db_size_bytes = std::fs::metadata(staging_db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    Ok(StagingSummary {
        db_path: staging_db_path.display().to_string(),
        total_tables: total_tables + junction_tables,
        junction_tables,
        elapsed_secs: start.elapsed().as_secs_f64(),
        db_size_mb: db_size_bytes as f64 / (1024.0 * 1024.0),
    })
}

/// Populate a staging DB from existing mod files.
///
/// This uses the same parallel parse + sequential insert pipeline as the
/// reference DB builder, but writes into a staging DB (with tracking columns
/// defaulting to `_is_new=0, _is_modified=0, _is_deleted=0`).
///
/// Files imported this way represent the mod's current state — they aren't
/// "new" in the staging sense (they already exist on disk).
pub fn populate_staging_from_mod(
    mod_path: &Path,
    mod_name: &str,
    staging_db_path: &Path,
    options: &BuildOptions,
) -> Result<BuildSummary, String> {
    let start = Instant::now();

    let t0 = Instant::now();
    let files = crate::reference_db::collect_mod_files(mod_path, mod_name)?;
    let total_files = files.len();
    let collect_secs = t0.elapsed().as_secs_f64();
    eprintln!(
        "  Phase: collect_mod    {collect_secs:.1}s  ({total_files} files for staging '{mod_name}')"
    );

    // Use the standard populate pipeline — the staging DB has the same column
    // structure (with extra DEFAULT columns that populate automatically).
    let result = builder::populate_db(staging_db_path, &files, mod_path, options)?;

    let db_size_bytes = std::fs::metadata(staging_db_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let mut phase_times = result.phase_times;
    phase_times.collect_files = collect_secs;

    Ok(BuildSummary {
        db_path: staging_db_path.display().to_string(),
        total_files,
        total_rows: result.total_rows,
        total_tables: result.total_tables,
        fk_constraints: result.fk_constraints,
        file_errors: result.file_errors,
        row_errors: result.row_errors,
        elapsed_secs: start.elapsed().as_secs_f64(),
        db_size_mb: db_size_bytes as f64 / (1024.0 * 1024.0),
        phase_times,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Meta tables for the staging DB.  Same as reference DB meta tables.
fn create_staging_meta_tables(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE _sources (
            id   INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE _source_files (
            file_id   INTEGER PRIMARY KEY AUTOINCREMENT,
            path      TEXT NOT NULL UNIQUE,
            file_type TEXT NOT NULL,
            mod_name  TEXT,
            region_id TEXT,
            row_count INTEGER DEFAULT 0,
            file_size INTEGER
        );

        CREATE TABLE _column_types (
            table_name  TEXT NOT NULL,
            column_name TEXT NOT NULL,
            bg3_type    TEXT,
            sqlite_type TEXT NOT NULL,
            PRIMARY KEY (table_name, column_name)
        ) WITHOUT ROWID;

        CREATE TABLE _table_meta (
            table_name    TEXT PRIMARY KEY,
            source_type   TEXT NOT NULL,
            region_id     TEXT,
            node_id       TEXT,
            parent_tables TEXT,
            row_count     INTEGER DEFAULT 0
        ) WITHOUT ROWID;

        CREATE TABLE IF NOT EXISTS _staging_authoring (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            _is_new INTEGER NOT NULL DEFAULT 0,
            _is_modified INTEGER NOT NULL DEFAULT 0,
            _is_deleted INTEGER NOT NULL DEFAULT 0,
            _original_hash TEXT
        ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("Staging meta table error: {e}"))
}

/// Create a data table for the staging DB.
///
/// Same schema as `create_data_table` but:
///   - No FK constraints (cross-DB handles this)
///   - Three extra tracking columns with DEFAULT values
fn create_staging_data_table(conn: &Connection, ts: &TableSchema) -> Result<(), String> {
    let mut ddl = String::new();
    ddl.push_str(&format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" (\n",
        ts.table_name
    ));

    // PK column
    match &ts.pk_strategy {
        PkStrategy::Rowid => {
            ddl.push_str("    \"_row_id\" INTEGER PRIMARY KEY");
        }
        pk => {
            ddl.push_str(&format!(
                "    \"{}\" {} PRIMARY KEY",
                pk.pk_column(),
                pk.pk_sql_type()
            ));
        }
    }

    // _file_id column
    if ts.has_file_id {
        ddl.push_str(",\n    _file_id INTEGER");
    }

    // _SourceID column (same as base, no FK constraint here)
    if ts.pk_strategy != PkStrategy::Rowid {
        ddl.push_str(",\n    \"_SourceID\" INTEGER");
    }

    // Data columns
    for col in &ts.columns {
        ddl.push_str(&format!(",\n    \"{}\" {}", col.name, col.sqlite_type));
    }

    // Staging tracking columns (with DEFAULTs so the existing insert pipeline works)
    ddl.push_str(",\n    \"_is_modified\" INTEGER NOT NULL DEFAULT 0");
    ddl.push_str(",\n    \"_is_new\" INTEGER NOT NULL DEFAULT 0");
    ddl.push_str(",\n    \"_is_deleted\" INTEGER NOT NULL DEFAULT 0");

    ddl.push_str("\n)");

    conn.execute_batch(&ddl).map_err(|e| {
        format!(
            "Create staging table '{}': {}\nDDL: {}",
            ts.table_name, e, ddl
        )
    })?;

    Ok(())
}

/// Populate _column_types for the staging DB.
fn populate_staging_column_types(
    conn: &Connection,
    schema: &DiscoveredSchema,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(
            "INSERT OR IGNORE INTO _column_types (table_name, column_name, bg3_type, sqlite_type) \
             VALUES (?1, ?2, ?3, ?4)",
        )
        .map_err(|e| format!("Prepare column_types insert: {e}"))?;

    for (table_name, ts) in &schema.tables {
        // PK column
        let pk_col = ts.pk_strategy.pk_column();
        let pk_type = ts.pk_strategy.pk_sql_type();
        stmt.execute(rusqlite::params![
            table_name,
            pk_col,
            Option::<&str>::None,
            pk_type
        ])
        .map_err(|e| format!("Insert column_types {table_name}.{pk_col}: {e}"))?;

        // Data columns
        for col in &ts.columns {
            let bg3_ref: Option<&str> = if col.bg3_type.is_empty() {
                None
            } else {
                Some(&col.bg3_type)
            };
            stmt.execute(rusqlite::params![
                table_name,
                col.name,
                bg3_ref,
                col.sqlite_type
            ])
            .map_err(|e| format!("Insert column_types {}.{}: {}", table_name, col.name, e))?;
        }

        // Staging-specific columns
        for staging_col in &["_is_modified", "_is_new", "_is_deleted"] {
            stmt.execute(rusqlite::params![
                table_name,
                staging_col,
                Option::<&str>::None,
                "INTEGER"
            ])
            .map_err(|e| format!("Insert column_types {table_name}.{staging_col}: {e}"))?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Staging CRUD operations (F1–F6)
// ---------------------------------------------------------------------------

/// Result of an upsert operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpsertResult {
    pub pk_value: String,
    pub was_insert: bool,
}

/// Upsert a row into a staging table.
///
/// - `is_new`: true → entry created in staging (not from vanilla)
/// - If row exists:
///   - If existing row has `_is_new=1`, preserve it (don't downgrade to modified)
///   - Otherwise set `_is_modified=1`
/// - If row doesn't exist: insert with `_is_new=is_new, _is_modified=!is_new`
pub fn staging_upsert_row(
    conn: &Connection,
    table: &str,
    columns: &HashMap<String, String>,
    is_new: bool,
) -> Result<UpsertResult, String> {
    let resolved = resolve_staging_table(conn, table)?;
    let table = resolved.as_str();
    let pk_col = get_pk_column(conn, table)?;

    // Filter to only columns that exist in the schema (the frontend form may
    // send prefixed keys like "Boolean:AllowImprovement" that don't correspond
    // to actual DB columns).
    let schema = load_embedded_schema(conn)?;
    let known_cols: std::collections::HashSet<&str> = schema
        .tables
        .get(table)
        .map(|ts| ts.columns.iter().map(|c| c.name.as_str()).collect())
        .unwrap_or_default();
    let mut columns: HashMap<String, String> = columns
        .iter()
        .filter(|(k, _)| known_cols.contains(k.as_str()) || *k == &pk_col)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Auto-generate _row_id for new entries in Rowid-PK tables
    if pk_col == "_row_id" && is_new && !columns.contains_key("_row_id") {
        let next_id: i64 = conn
            .query_row(
                &format!("SELECT COALESCE(MAX(\"_row_id\"), 0) + 1 FROM \"{table}\""),
                [],
                |row| row.get(0),
            )
            .unwrap_or(1);
        columns.insert("_row_id".to_string(), next_id.to_string());
    }

    let pk_value = columns
        .get(&pk_col)
        .ok_or_else(|| format!("Missing PK column '{pk_col}' in upsert data for table '{table}'"))?
        .clone();

    // Undo: capture before state
    let journal_active = undo_journal_exists(conn);
    let old_row_json = if journal_active {
        capture_row_as_json(conn, table, &pk_col, &pk_value)?
    } else {
        None
    };

    // Check if row already exists
    let existing_is_new: Option<bool> = {
        let sql = format!("SELECT \"_is_new\" FROM \"{table}\" WHERE \"{pk_col}\" = ?1");
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("Prepare check: {e}"))?;
        stmt.query_row([&pk_value], |row| {
            let v: i32 = row.get(0)?;
            Ok(v == 1)
        })
        .optional()
        .map_err(|e| format!("Check existing: {e}"))?
    };

    let was_insert = existing_is_new.is_none();

    if let Some(prev_is_new) = existing_is_new {
        // Row exists → UPDATE
        let set_clauses: Vec<String> = columns
            .iter()
            .filter(|(k, _)| **k != pk_col)
            .map(|(k, _)| format!("\"{k}\" = ?"))
            .collect();

        if set_clauses.is_empty() {
            let tracking = if prev_is_new {
                "\"_is_new\" = 1"
            } else {
                "\"_is_modified\" = 1"
            };
            let sql = format!("UPDATE \"{table}\" SET {tracking} WHERE \"{pk_col}\" = ?1");
            conn.execute(&sql, rusqlite::params![pk_value])
                .map_err(|e| format!("Update tracking: {e}"))?;
        } else {
            let tracking = if prev_is_new {
                ", \"_is_new\" = 1"
            } else {
                ", \"_is_modified\" = 1"
            };
            let all_sets = format!("{}{}", set_clauses.join(", "), tracking);
            let sql = format!("UPDATE \"{table}\" SET {all_sets} WHERE \"{pk_col}\" = ?");

            let mut values: Vec<String> = columns
                .iter()
                .filter(|(k, _)| **k != pk_col)
                .map(|(_, v)| v.clone())
                .collect();
            values.push(pk_value.clone());

            let refs: Vec<&dyn rusqlite::types::ToSql> = values
                .iter()
                .map(|v| v as &dyn rusqlite::types::ToSql)
                .collect();
            conn.execute(&sql, refs.as_slice())
                .map_err(|e| format!("Update row: {e}"))?;
        }
    } else {
        // Row doesn't exist → INSERT
        let mut all_cols: Vec<String> = columns.keys().map(|c| format!("\"{c}\"")).collect();
        all_cols.push("\"_is_new\"".to_string());
        all_cols.push("\"_is_modified\"".to_string());

        let mut placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
        let new_val = if is_new { "1" } else { "0" };
        let mod_val = if is_new { "0" } else { "1" };
        placeholders.push(new_val.to_string());
        placeholders.push(mod_val.to_string());

        let sql = format!(
            "INSERT INTO \"{}\" ({}) VALUES ({})",
            table,
            all_cols.join(", "),
            placeholders.join(", ")
        );

        let values: Vec<&str> = columns.values().map(|v| v.as_str()).collect();
        let refs: Vec<&dyn rusqlite::types::ToSql> = values
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        conn.execute(&sql, refs.as_slice())
            .map_err(|e| format!("Insert row: {e}"))?;
    }

    // Undo: record change
    if journal_active {
        let new_row_json = capture_row_as_json(conn, table, &pk_col, &pk_value)?;
        staging_record_change(
            conn,
            "upsert",
            table,
            &pk_value,
            old_row_json.as_deref(),
            new_row_json.as_deref(),
        )?;
    }

    Ok(UpsertResult {
        pk_value,
        was_insert,
    })
}

/// Soft-delete a row. If the row has `_is_new=1`, hard-delete it instead.
/// Returns `true` if a row was affected, `false` if not found.
pub fn staging_mark_deleted(conn: &Connection, table: &str, pk: &str) -> Result<bool, String> {
    let resolved = resolve_staging_table(conn, table)?;
    let table = resolved.as_str();
    let pk_col = get_pk_column(conn, table)?;

    // Undo: capture before state
    let journal_active = undo_journal_exists(conn);
    let old_row_json = if journal_active {
        capture_row_as_json(conn, table, &pk_col, pk)?
    } else {
        None
    };

    let is_new: Option<bool> = {
        let sql = format!("SELECT \"_is_new\" FROM \"{table}\" WHERE \"{pk_col}\" = ?1");
        let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
        stmt.query_row([pk], |row| {
            let v: i32 = row.get(0)?;
            Ok(v == 1)
        })
        .optional()
        .map_err(|e| format!("Check: {e}"))?
    };

    match is_new {
        None => Ok(false),
        Some(true) => {
            // _is_new=1 → hard delete (entry only existed in staging)
            let sql = format!("DELETE FROM \"{table}\" WHERE \"{pk_col}\" = ?1");
            conn.execute(&sql, [pk])
                .map_err(|e| format!("Hard delete: {e}"))?;
            // Undo: record hard delete (new_row = None since row is gone)
            if journal_active {
                staging_record_change(conn, "delete", table, pk, old_row_json.as_deref(), None)?;
            }
            Ok(true)
        }
        Some(false) => {
            // Existing row → soft delete
            let sql = format!("UPDATE \"{table}\" SET \"_is_deleted\" = 1 WHERE \"{pk_col}\" = ?1");
            conn.execute(&sql, [pk])
                .map_err(|e| format!("Soft delete: {e}"))?;
            // Undo: record soft delete
            if journal_active {
                let new_row_json = capture_row_as_json(conn, table, &pk_col, pk)?;
                staging_record_change(
                    conn,
                    "mark_deleted",
                    table,
                    pk,
                    old_row_json.as_deref(),
                    new_row_json.as_deref(),
                )?;
            }
            Ok(true)
        }
    }
}

/// Unmark a soft-deleted row. Returns `true` if a row was affected.
pub fn staging_unmark_deleted(conn: &Connection, table: &str, pk: &str) -> Result<bool, String> {
    let resolved = resolve_staging_table(conn, table)?;
    let table = resolved.as_str();
    let pk_col = get_pk_column(conn, table)?;

    // Undo: capture before state
    let journal_active = undo_journal_exists(conn);
    let old_row_json = if journal_active {
        capture_row_as_json(conn, table, &pk_col, pk)?
    } else {
        None
    };

    let sql = format!("UPDATE \"{table}\" SET \"_is_deleted\" = 0 WHERE \"{pk_col}\" = ?1");
    let affected = conn
        .execute(&sql, [pk])
        .map_err(|e| format!("Undelete: {e}"))?;
    let result = affected > 0;

    // Undo: record state change
    if journal_active && result {
        let new_row_json = capture_row_as_json(conn, table, &pk_col, pk)?;
        staging_record_change(
            conn,
            "unmark_deleted",
            table,
            pk,
            old_row_json.as_deref(),
            new_row_json.as_deref(),
        )?;
    }

    Ok(result)
}

/// A single operation in a batch write.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "op")]
pub enum StagingOperation {
    Upsert {
        table: String,
        columns: HashMap<String, String>,
        is_new: bool,
    },
    MarkDeleted {
        table: String,
        pk: String,
    },
    UnmarkDeleted {
        table: String,
        pk: String,
    },
}

/// Result of a batch write.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StagingBatchResult {
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub errors: Vec<String>,
}

/// Execute a batch of staging operations atomically.
///
/// On any individual failure the entire batch is rolled back.
pub fn staging_batch_write(
    conn: &Connection,
    operations: &[StagingOperation],
) -> Result<StagingBatchResult, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Begin batch transaction: {e}"))?;

    let total = operations.len();

    for (i, op) in operations.iter().enumerate() {
        let result = match op {
            StagingOperation::Upsert {
                table,
                columns,
                is_new,
            } => staging_upsert_row(&tx, table, columns, *is_new).map(|_| ()),
            StagingOperation::MarkDeleted { table, pk } => {
                staging_mark_deleted(&tx, table, pk).map(|_| ())
            }
            StagingOperation::UnmarkDeleted { table, pk } => {
                staging_unmark_deleted(&tx, table, pk).map(|_| ())
            }
        };

        if let Err(e) = result {
            // Explicit rollback — drop would do the same but this is clearer
            let _ = tx.rollback();
            return Ok(StagingBatchResult {
                total,
                succeeded: i,
                failed: total - i,
                errors: vec![format!("Operation {}: {}", i, e)],
            });
        }
    }

    tx.commit().map_err(|e| format!("Commit batch: {e}"))?;

    Ok(StagingBatchResult {
        total,
        succeeded: total,
        failed: 0,
        errors: Vec::new(),
    })
}

/// A row with tracked changes.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StagingChange {
    pub table: String,
    pub pk_value: String,
    pub change_type: String,
    pub columns: HashMap<String, serde_json::Value>,
}

/// Query rows with changes (new, modified, or deleted).
pub fn staging_query_changes(
    conn: &Connection,
    table_filter: Option<&str>,
) -> Result<Vec<StagingChange>, String> {
    let schema = load_embedded_schema(conn)?;
    let mut changes = Vec::new();

    let tables: Vec<(&String, &TableSchema)> = if let Some(filter) = table_filter {
        let resolved = resolve_staging_table(conn, filter)?;
        schema
            .tables
            .iter()
            .filter(|(name, _)| name.as_str() == resolved)
            .collect()
    } else {
        schema.tables.iter().collect()
    };

    for (table_name, ts) in &tables {
        let pk_col = ts.pk_strategy.pk_column();
        let sql = format!(
            "SELECT * FROM \"{table_name}\" WHERE \"_is_new\" = 1 OR \"_is_modified\" = 1 OR \"_is_deleted\" = 1"
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("Query changes in {table_name}: {e}"))?;
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap().to_string())
            .collect();

        let rows = stmt
            .query_map([], |row| {
                let mut columns = HashMap::new();
                let mut is_new = false;
                let mut is_modified = false;
                let mut is_deleted = false;
                let mut pk_val = String::new();

                for (i, name) in col_names.iter().enumerate() {
                    match name.as_str() {
                        "_is_new" => {
                            is_new = row.get::<_, i32>(i)? == 1;
                        }
                        "_is_modified" => {
                            is_modified = row.get::<_, i32>(i)? == 1;
                        }
                        "_is_deleted" => {
                            is_deleted = row.get::<_, i32>(i)? == 1;
                        }
                        _ => {
                            let val: rusqlite::types::Value = row.get(i)?;
                            let json_val = sqlite_value_to_json(val);
                            if name == pk_col {
                                pk_val = match &json_val {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                            }
                            columns.insert(name.clone(), json_val);
                        }
                    }
                }

                let change_type = if is_deleted {
                    "deleted"
                } else if is_new {
                    "new"
                } else if is_modified {
                    "modified"
                } else {
                    "unknown"
                };

                Ok(StagingChange {
                    table: table_name.to_string(),
                    pk_value: pk_val,
                    change_type: change_type.to_string(),
                    columns,
                })
            })
            .map_err(|e| format!("Map changes in {table_name}: {e}"))?;

        for row in rows {
            changes.push(row.map_err(|e| format!("Row error in {table_name}: {e}"))?);
        }
    }

    Ok(changes)
}

/// Summary of a staging section (table).
#[derive(Debug, Clone, serde::Serialize)]
pub struct StagingSectionSummary {
    pub table_name: String,
    pub region_id: Option<String>,
    pub node_id: Option<String>,
    pub source_type: String,
    pub total_rows: usize,
    pub active_rows: usize,
    pub new_rows: usize,
    pub modified_rows: usize,
    pub deleted_rows: usize,
}

/// List all staging sections with row count summaries.
pub fn staging_list_sections(conn: &Connection) -> Result<Vec<StagingSectionSummary>, String> {
    let schema = load_embedded_schema(conn)?;
    let mut sections = Vec::new();

    for (table_name, ts) in &schema.tables {
        let sql = format!(
            "SELECT \
                COUNT(*), \
                SUM(CASE WHEN \"_is_deleted\" = 0 THEN 1 ELSE 0 END), \
                SUM(CASE WHEN \"_is_new\" = 1 THEN 1 ELSE 0 END), \
                SUM(CASE WHEN \"_is_modified\" = 1 THEN 1 ELSE 0 END), \
                SUM(CASE WHEN \"_is_deleted\" = 1 THEN 1 ELSE 0 END) \
             FROM \"{table_name}\""
        );

        let counts = conn
            .query_row(&sql, [], |row| {
                Ok((
                    row.get::<_, i64>(0).unwrap_or(0) as usize,
                    row.get::<_, Option<i64>>(1).unwrap_or(Some(0)).unwrap_or(0) as usize,
                    row.get::<_, Option<i64>>(2).unwrap_or(Some(0)).unwrap_or(0) as usize,
                    row.get::<_, Option<i64>>(3).unwrap_or(Some(0)).unwrap_or(0) as usize,
                    row.get::<_, Option<i64>>(4).unwrap_or(Some(0)).unwrap_or(0) as usize,
                ))
            })
            .map_err(|e| format!("Count rows in {table_name}: {e}"))?;

        sections.push(StagingSectionSummary {
            table_name: table_name.clone(),
            region_id: ts.region_id.clone(),
            node_id: ts.node_id.clone(),
            source_type: ts.source_type.clone(),
            total_rows: counts.0,
            active_rows: counts.1,
            new_rows: counts.2,
            modified_rows: counts.3,
            deleted_rows: counts.4,
        });
    }

    Ok(sections)
}

/// Query all rows for a staging section.
pub fn staging_query_section(
    conn: &Connection,
    table: &str,
    include_deleted: bool,
) -> Result<Vec<HashMap<String, serde_json::Value>>, String> {
    let resolved = resolve_staging_table(conn, table)?;

    let sql = if include_deleted {
        format!("SELECT * FROM \"{resolved}\"")
    } else {
        format!("SELECT * FROM \"{resolved}\" WHERE \"_is_deleted\" = 0")
    };

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Query section {table}: {e}"))?;
    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();

    let rows = stmt
        .query_map([], |row| {
            let mut map = HashMap::new();
            for (i, name) in col_names.iter().enumerate() {
                let val: rusqlite::types::Value = row.get(i)?;
                let json_val = sqlite_value_to_json(val);
                map.insert(name.clone(), json_val);
            }
            Ok(map)
        })
        .map_err(|e| format!("Map rows in {table}: {e}"))?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| format!("Row error: {e}"))?);
    }
    Ok(result)
}

/// Get a single row by PK.
pub fn staging_get_row(
    conn: &Connection,
    table: &str,
    pk: &str,
) -> Result<Option<HashMap<String, serde_json::Value>>, String> {
    let resolved = resolve_staging_table(conn, table)?;
    let pk_col = get_pk_column(conn, &resolved)?;

    let sql = format!("SELECT * FROM \"{resolved}\" WHERE \"{pk_col}\" = ?1");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Prepare get_row: {e}"))?;
    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();

    let result = stmt
        .query_row([pk], |row| {
            let mut map = HashMap::new();
            for (i, name) in col_names.iter().enumerate() {
                let val: rusqlite::types::Value = row.get(i)?;
                let json_val = sqlite_value_to_json(val);
                map.insert(name.clone(), json_val);
            }
            Ok(map)
        })
        .optional()
        .map_err(|e| format!("Get row: {e}"))?;

    Ok(result)
}

/// Ensure `_staging_authoring` meta table exists with all columns.
pub fn ensure_staging_authoring_table(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _staging_authoring (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            _is_new INTEGER NOT NULL DEFAULT 0,
            _is_modified INTEGER NOT NULL DEFAULT 0,
            _is_deleted INTEGER NOT NULL DEFAULT 0,
            _original_hash TEXT
        ) WITHOUT ROWID;",
    )
    .map_err(|e| format!("Create _staging_authoring: {e}"))?;

    // For existing tables that might lack new columns, attempt to add them.
    for (col, default) in &[
        ("_is_new", "INTEGER NOT NULL DEFAULT 0"),
        ("_is_modified", "INTEGER NOT NULL DEFAULT 0"),
        ("_is_deleted", "INTEGER NOT NULL DEFAULT 0"),
        ("_original_hash", "TEXT"),
    ] {
        let has_col = conn
            .prepare(&format!("SELECT \"{col}\" FROM _staging_authoring LIMIT 0"))
            .is_ok();
        if !has_col {
            let _ = conn.execute_batch(&format!(
                "ALTER TABLE _staging_authoring ADD COLUMN \"{col}\" {default}"
            ));
        }
    }

    Ok(())
}

/// Get a meta value from `_staging_authoring`.
pub fn staging_get_meta(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    let result = conn
        .query_row(
            "SELECT value FROM _staging_authoring WHERE key = ?1 AND \"_is_deleted\" = 0",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Get meta '{key}': {e}"))?;
    Ok(result)
}

/// Set a meta value in `_staging_authoring`.
pub fn staging_set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO _staging_authoring (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )
    .map_err(|e| format!("Set meta '{key}': {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a SQLite value to a serde_json::Value.
fn sqlite_value_to_json(val: rusqlite::types::Value) -> serde_json::Value {
    match val {
        rusqlite::types::Value::Null => serde_json::Value::Null,
        rusqlite::types::Value::Integer(n) => serde_json::json!(n),
        rusqlite::types::Value::Real(f) => serde_json::json!(f),
        rusqlite::types::Value::Text(s) => serde_json::Value::String(s),
        rusqlite::types::Value::Blob(b) => {
            serde_json::Value::String(format!("[blob:{} bytes]", b.len()))
        }
    }
}

/// Validate that a table exists in the staging schema.
/// If the exact name isn't found, try resolving via `_table_meta.region_id`
/// (handles multi-node regions where `lsx__Region` doesn't exist but
/// `lsx__Region__Node` does).
/// Resolve a frontend table name to the actual staging table name.
/// Returns the input unchanged if it matches directly, or resolves
/// via `_table_meta.region_id` for multi-node regions.
fn resolve_staging_table(conn: &Connection, table: &str) -> Result<String, String> {
    let schema = load_embedded_schema(conn)?;
    if schema.tables.contains_key(table) {
        return Ok(table.to_string());
    }
    // Fallback: check _table_meta for a matching region_id
    if let Some(resolved) = resolve_table_by_region(conn, table)? {
        if schema.tables.contains_key(&resolved) {
            return Ok(resolved);
        }
    }
    Err(format!("Table '{table}' not found in staging schema"))
}

/// Resolve a frontend table name (e.g. `lsx__Progressions`) to the actual
/// table in _table_meta by matching region_id.  Prefers data tables (where
/// node_id != 'root') over wrapper root tables, since root tables use
/// _row_id PK and don't hold the actual mod-editable entries.
fn resolve_table_by_region(conn: &Connection, table: &str) -> Result<Option<String>, String> {
    // Extract the region and implied source type from "lsx__RegionName" or "stats__RegionName"
    let (source_type, region) = if let Some(r) = table.strip_prefix("lsx__") {
        (Some("lsx"), r)
    } else if let Some(r) = table.strip_prefix("stats__") {
        (Some("stats"), r)
    } else {
        (None, table)
    };

    // Try with source_type filter first, preferring data tables (node_id != 'root')
    if let Some(st) = source_type {
        // First: data table (non-root) for this region + source type
        let data_result: Option<String> = conn
            .query_row(
                "SELECT table_name FROM _table_meta WHERE region_id = ?1 AND source_type = ?2 AND node_id != 'root' LIMIT 1",
                rusqlite::params![region, st],
                |row| row.get(0),
            )
            .ok();
        if data_result.is_some() {
            return Ok(data_result);
        }
        // Fallback: any table for this region + source type (including root)
        let any_result: Option<String> = conn
            .query_row(
                "SELECT table_name FROM _table_meta WHERE region_id = ?1 AND source_type = ?2 LIMIT 1",
                rusqlite::params![region, st],
                |row| row.get(0),
            )
            .ok();
        if any_result.is_some() {
            return Ok(any_result);
        }
    }

    // Fallback: match by region_id alone, prefer data tables
    let data_result: Option<String> = conn
        .query_row(
            "SELECT table_name FROM _table_meta WHERE region_id = ?1 AND node_id != 'root' LIMIT 1",
            [region],
            |row| row.get(0),
        )
        .ok();
    if data_result.is_some() {
        return Ok(data_result);
    }

    let any_result: Option<String> = conn
        .query_row(
            "SELECT table_name FROM _table_meta WHERE region_id = ?1 LIMIT 1",
            [region],
            |row| row.get(0),
        )
        .ok();
    Ok(any_result)
}

/// Get PK column name for a staging table.
fn get_pk_column(conn: &Connection, table: &str) -> Result<String, String> {
    let schema = load_embedded_schema(conn)?;
    let ts = schema
        .tables
        .get(table)
        .ok_or_else(|| format!("Table '{table}' not in schema"))?;
    Ok(ts.pk_strategy.pk_column().to_string())
}

/// Load the embedded schema from the staging DB.
fn load_embedded_schema(conn: &Connection) -> Result<DiscoveredSchema, String> {
    let blob: Vec<u8> = conn
        .query_row(
            "SELECT value FROM _embedded_schema WHERE key = 'schema'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("Load embedded schema: {e}"))?;

    rmp_serde::from_slice(&blob).map_err(|e| format!("Deserialize schema: {e}"))
}

// ---------------------------------------------------------------------------
// Undo Journal System
// ---------------------------------------------------------------------------

/// Entry returned after an undo/redo replay, for frontend cache invalidation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UndoReplayEntry {
    pub table_name: String,
    pub pk_value: String,
    pub action: String,
}

/// Ensure the `_staging_undo_journal` table exists (lazy creation).
pub fn ensure_undo_journal_table(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _staging_undo_journal (
            id INTEGER PRIMARY KEY,
            label TEXT NOT NULL,
            table_name TEXT NOT NULL,
            pk_value TEXT NOT NULL,
            old_row TEXT,
            new_row TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )
    .map_err(|e| format!("Create undo journal: {e}"))
}

/// Record a change in the undo journal. Called by staging write functions.
pub fn staging_record_change(
    conn: &Connection,
    label: &str,
    table_name: &str,
    pk_value: &str,
    old_row: Option<&str>,
    new_row: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO _staging_undo_journal (label, table_name, pk_value, old_row, new_row) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![label, table_name, pk_value, old_row, new_row],
    )
    .map_err(|e| format!("Record undo change: {e}"))?;
    Ok(())
}

/// Create a boundary marker (snapshot) in the undo journal.
///
/// Call this between mutation groups. If the undo pointer is behind the latest
/// journal entry (i.e. user undid then edited), redo history is pruned.
pub fn staging_snapshot(conn: &Connection, label: &str) -> Result<i64, String> {
    ensure_undo_journal_table(conn)?;
    ensure_staging_authoring_table(conn)?;

    let pointer = get_undo_pointer(conn)?;

    // Prune redo entries only if there are boundary markers after the pointer
    // (indicates undone groups whose redo history should be discarded).
    // Mutation entries between the pointer and here belong to the current
    // group and must be preserved.
    if let Some(ptr) = pointer {
        let redo_boundary_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _staging_undo_journal \
                 WHERE id > ?1 AND table_name = '__boundary__'",
                [ptr],
                |row| row.get(0),
            )
            .map_err(|e| format!("Check redo boundaries: {e}"))?;

        if redo_boundary_count > 0 {
            conn.execute("DELETE FROM _staging_undo_journal WHERE id > ?1", [ptr])
                .map_err(|e| format!("Prune redo: {e}"))?;
        }
    }

    // Insert boundary marker
    conn.execute(
        "INSERT INTO _staging_undo_journal (label, table_name, pk_value, old_row, new_row) \
         VALUES (?1, '__boundary__', '', NULL, NULL)",
        [label],
    )
    .map_err(|e| format!("Insert boundary: {e}"))?;

    let boundary_id = conn.last_insert_rowid();
    staging_set_meta(conn, "undo_pointer", &boundary_id.to_string())?;

    Ok(boundary_id)
}

/// Undo the last mutation group.
///
/// Finds all journal entries between the current boundary (undo_pointer) and
/// the previous boundary, replays them in reverse order (applying `old_row`),
/// and moves the pointer back.
pub fn staging_undo(conn: &Connection) -> Result<Vec<UndoReplayEntry>, String> {
    ensure_undo_journal_table(conn)?;
    ensure_staging_authoring_table(conn)?;

    let pointer = match get_undo_pointer(conn)? {
        Some(p) => p,
        None => return Ok(vec![]),
    };

    // Find previous boundary
    let prev_boundary: Option<i64> = conn
        .query_row(
            "SELECT MAX(id) FROM _staging_undo_journal \
             WHERE id < ?1 AND table_name = '__boundary__'",
            [pointer],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Find prev boundary: {e}"))?
        .flatten();

    let lower = prev_boundary.unwrap_or(0);

    // Get entries between previous boundary and current pointer (the group)
    let entries: Vec<(String, String, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT table_name, pk_value, old_row, new_row \
                 FROM _staging_undo_journal \
                 WHERE id > ?1 AND id < ?2 AND table_name != '__boundary__' \
                 ORDER BY id DESC",
            )
            .map_err(|e| format!("Query undo entries: {e}"))?;

        let result = stmt
            .query_map(rusqlite::params![lower, pointer], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| format!("Map undo entries: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Collect undo entries: {e}"))?;
        result
    };

    if entries.is_empty() {
        // Empty group — still move pointer so further undos can reach earlier groups
        match prev_boundary {
            Some(b) => staging_set_meta(conn, "undo_pointer", &b.to_string())?,
            None => {
                conn.execute(
                    "DELETE FROM _staging_authoring WHERE key = 'undo_pointer'",
                    [],
                )
                .map_err(|e| format!("Clear undo pointer: {e}"))?;
            }
        }
        return Ok(vec![]);
    }

    let schema = load_embedded_schema(conn)?;

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Begin undo transaction: {e}"))?;

    let mut replayed = Vec::new();
    for (table_name, pk_value, old_row, new_row) in &entries {
        let action = replay_undo_entry(
            &tx,
            &schema,
            table_name,
            pk_value,
            old_row.as_deref(),
            new_row.as_deref(),
        )?;
        replayed.push(UndoReplayEntry {
            table_name: table_name.clone(),
            pk_value: pk_value.clone(),
            action,
        });
    }

    // Update pointer inside transaction for atomicity
    match prev_boundary {
        Some(b) => staging_set_meta(&tx, "undo_pointer", &b.to_string())?,
        None => {
            tx.execute(
                "DELETE FROM _staging_authoring WHERE key = 'undo_pointer'",
                [],
            )
            .map_err(|e| format!("Clear undo pointer: {e}"))?;
        }
    }

    tx.commit().map_err(|e| format!("Commit undo: {e}"))?;

    Ok(replayed)
}

/// Redo the next mutation group.
///
/// Finds the next boundary after the current pointer, gets entries between
/// the pointer and that boundary, replays them forward (applying `new_row`).
pub fn staging_redo(conn: &Connection) -> Result<Vec<UndoReplayEntry>, String> {
    ensure_undo_journal_table(conn)?;
    ensure_staging_authoring_table(conn)?;

    let pointer_val = get_undo_pointer(conn)?.unwrap_or(0);

    // Find next boundary after current pointer
    let next_boundary: Option<i64> = conn
        .query_row(
            "SELECT MIN(id) FROM _staging_undo_journal \
             WHERE id > ?1 AND table_name = '__boundary__'",
            [pointer_val],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("Find next boundary: {e}"))?
        .flatten();

    let next_boundary = match next_boundary {
        Some(b) => b,
        None => return Ok(vec![]),
    };

    // Entries between current pointer and next boundary
    let entries: Vec<(String, String, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT table_name, pk_value, old_row, new_row \
                 FROM _staging_undo_journal \
                 WHERE id > ?1 AND id < ?2 AND table_name != '__boundary__' \
                 ORDER BY id ASC",
            )
            .map_err(|e| format!("Query redo entries: {e}"))?;

        let result = stmt
            .query_map(rusqlite::params![pointer_val, next_boundary], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| format!("Map redo entries: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Collect redo entries: {e}"))?;
        result
    };

    if entries.is_empty() {
        // Empty group — move pointer forward anyway
        staging_set_meta(conn, "undo_pointer", &next_boundary.to_string())?;
        return Ok(vec![]);
    }

    let schema = load_embedded_schema(conn)?;

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Begin redo transaction: {e}"))?;

    let mut replayed = Vec::new();
    for (table_name, pk_value, old_row, new_row) in &entries {
        let action = replay_redo_entry(
            &tx,
            &schema,
            table_name,
            pk_value,
            old_row.as_deref(),
            new_row.as_deref(),
        )?;
        replayed.push(UndoReplayEntry {
            table_name: table_name.clone(),
            pk_value: pk_value.clone(),
            action,
        });
    }

    staging_set_meta(&tx, "undo_pointer", &next_boundary.to_string())?;
    tx.commit().map_err(|e| format!("Commit redo: {e}"))?;

    Ok(replayed)
}

/// Replace all rows in a staging table with the given baseline rows and
/// clear the undo journal for that table.  Used by "Reset Section" to
/// restore a section to its state when the project was first opened.
///
/// `rows` must have the same shape as `staging_query_section` output
/// (all data columns + `_is_new`, `_is_modified`, `_is_deleted`).
/// Returns the number of rows restored.
pub fn staging_replace_section(
    conn: &Connection,
    table: &str,
    rows: &[HashMap<String, serde_json::Value>],
) -> Result<usize, String> {
    let resolved = resolve_staging_table(conn, table)?;
    let table = resolved.as_str();

    // Get schema columns so we know which columns belong in this table
    let schema = load_embedded_schema(conn)?;
    let ts = schema
        .tables
        .get(table)
        .ok_or_else(|| format!("Table '{table}' not in embedded schema"))?;
    let schema_cols: Vec<&str> = ts.columns.iter().map(|c| c.name.as_str()).collect();

    // Tracking columns that also need to be restored
    let tracking_cols = ["_is_new", "_is_modified", "_is_deleted"];

    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("Begin replace transaction: {e}"))?;

    // 1. Delete all existing rows
    tx.execute(&format!("DELETE FROM \"{table}\""), [])
        .map_err(|e| format!("Clear table '{table}': {e}"))?;

    // 2. Insert each baseline row
    if !rows.is_empty() {
        // Determine columns to insert from the first row, filtered to known columns
        let insert_cols: Vec<String> = {
            let mut cols: Vec<String> = schema_cols
                .iter()
                .filter(|c| rows[0].contains_key(**c))
                .map(|c| c.to_string())
                .collect();
            for tc in &tracking_cols {
                if rows[0].contains_key(*tc) && !cols.contains(&tc.to_string()) {
                    cols.push(tc.to_string());
                }
            }
            cols
        };

        let col_list = insert_cols
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders = (1..=insert_cols.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO \"{table}\" ({col_list}) VALUES ({placeholders})");

        let mut stmt = tx
            .prepare(&sql)
            .map_err(|e| format!("Prepare insert: {e}"))?;

        for row in rows {
            let values: Vec<Box<dyn rusqlite::types::ToSql>> = insert_cols
                .iter()
                .map(|col| -> Box<dyn rusqlite::types::ToSql> {
                    match row.get(col.as_str()) {
                        Some(serde_json::Value::String(s)) => Box::new(s.clone()),
                        Some(serde_json::Value::Number(n)) => {
                            if let Some(i) = n.as_i64() {
                                Box::new(i)
                            } else if let Some(f) = n.as_f64() {
                                Box::new(f)
                            } else {
                                Box::new(n.to_string())
                            }
                        }
                        Some(serde_json::Value::Bool(b)) => Box::new(*b as i32),
                        Some(serde_json::Value::Null) | None => Box::new(rusqlite::types::Null),
                        Some(other) => Box::new(other.to_string()),
                    }
                })
                .collect();

            let refs: Vec<&dyn rusqlite::types::ToSql> =
                values.iter().map(|v| v.as_ref()).collect();
            stmt.execute(refs.as_slice())
                .map_err(|e| format!("Insert baseline row: {e}"))?;
        }
    }

    // 3. Clear undo journal entries for this table
    if undo_journal_exists(&tx) {
        tx.execute(
            "DELETE FROM _staging_undo_journal WHERE table_name = ?1",
            [table],
        )
        .map_err(|e| format!("Clear undo journal for '{table}': {e}"))?;

        // Reset undo pointer to the latest remaining boundary (if any)
        let max_boundary: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM _staging_undo_journal WHERE table_name = '__boundary__'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("Query max boundary: {e}"))?;

        if max_boundary > 0 {
            staging_set_meta(&tx, "undo_pointer", &max_boundary.to_string())?;
        } else {
            // No boundaries left — clear pointer
            tx.execute(
                "DELETE FROM _staging_authoring WHERE key = 'undo_pointer'",
                [],
            )
            .map_err(|e| format!("Clear undo pointer: {e}"))?;
        }
    }

    tx.commit().map_err(|e| format!("Commit replace: {e}"))?;

    Ok(rows.len())
}

// ---------------------------------------------------------------------------
// Undo journal internal helpers
// ---------------------------------------------------------------------------

/// Check if the undo journal table exists.
fn undo_journal_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='_staging_undo_journal'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// Get the current undo pointer from meta.
fn get_undo_pointer(conn: &Connection) -> Result<Option<i64>, String> {
    let val = staging_get_meta(conn, "undo_pointer")?;
    match val {
        Some(s) => {
            let id: i64 = s
                .parse()
                .map_err(|_| format!("Invalid undo_pointer: {s}"))?;
            Ok(Some(id))
        }
        None => {
            // Default to max boundary id
            let max_boundary: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(id), 0) FROM _staging_undo_journal \
                     WHERE table_name = '__boundary__'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| format!("Query max boundary: {e}"))?;

            if max_boundary == 0 {
                Ok(None)
            } else {
                Ok(Some(max_boundary))
            }
        }
    }
}

/// Capture a row's current state as a JSON string. Returns None if not found.
fn capture_row_as_json(
    conn: &Connection,
    table: &str,
    pk_col: &str,
    pk_value: &str,
) -> Result<Option<String>, String> {
    let sql = format!("SELECT * FROM \"{table}\" WHERE \"{pk_col}\" = ?1");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("Capture row prep: {e}"))?;
    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();

    let result = stmt
        .query_row([pk_value], |row| {
            let mut map = serde_json::Map::new();
            for (i, name) in col_names.iter().enumerate() {
                let val: rusqlite::types::Value = row.get(i)?;
                map.insert(name.clone(), sqlite_value_to_json(val));
            }
            Ok(map)
        })
        .optional()
        .map_err(|e| format!("Capture row query: {e}"))?;

    match result {
        Some(map) => {
            let json = serde_json::to_string(&serde_json::Value::Object(map))
                .map_err(|e| format!("Serialize captured row: {e}"))?;
            Ok(Some(json))
        }
        None => Ok(None),
    }
}

/// Replay an undo entry: apply old_row state to reverse the change.
fn replay_undo_entry(
    conn: &Connection,
    schema: &DiscoveredSchema,
    table_name: &str,
    pk_value: &str,
    old_row: Option<&str>,
    new_row: Option<&str>,
) -> Result<String, String> {
    let pk_col = schema
        .tables
        .get(table_name)
        .map(|ts| ts.pk_strategy.pk_column().to_string())
        .unwrap_or_else(|| "UUID".to_string());

    match (old_row, new_row) {
        (None, Some(_)) => {
            // Was an INSERT → undo by DELETE
            let sql = format!("DELETE FROM \"{table_name}\" WHERE \"{pk_col}\" = ?1");
            conn.execute(&sql, [pk_value])
                .map_err(|e| format!("Undo delete {table_name}.{pk_value}: {e}"))?;
            Ok("delete".to_string())
        }
        (Some(old), None) => {
            // Was a DELETE → undo by INSERT old_row back
            insert_row_from_json(conn, table_name, old)?;
            Ok("insert".to_string())
        }
        (Some(old), Some(_)) => {
            // Was an UPDATE → undo by restoring old_row
            update_row_from_json(conn, table_name, &pk_col, pk_value, old)?;
            Ok("update".to_string())
        }
        (None, None) => Err(format!(
            "Invalid journal entry: both old_row and new_row are NULL for {table_name}.{pk_value}"
        )),
    }
}

/// Replay a redo entry: apply new_row state to re-apply the change.
fn replay_redo_entry(
    conn: &Connection,
    schema: &DiscoveredSchema,
    table_name: &str,
    pk_value: &str,
    old_row: Option<&str>,
    new_row: Option<&str>,
) -> Result<String, String> {
    let pk_col = schema
        .tables
        .get(table_name)
        .map(|ts| ts.pk_strategy.pk_column().to_string())
        .unwrap_or_else(|| "UUID".to_string());

    match (old_row, new_row) {
        (None, Some(new)) => {
            // Was an INSERT → redo by INSERT
            insert_row_from_json(conn, table_name, new)?;
            Ok("insert".to_string())
        }
        (Some(_), None) => {
            // Was a DELETE → redo by DELETE
            let sql = format!("DELETE FROM \"{table_name}\" WHERE \"{pk_col}\" = ?1");
            conn.execute(&sql, [pk_value])
                .map_err(|e| format!("Redo delete {table_name}.{pk_value}: {e}"))?;
            Ok("delete".to_string())
        }
        (Some(_), Some(new)) => {
            // Was an UPDATE → redo by applying new_row
            update_row_from_json(conn, table_name, &pk_col, pk_value, new)?;
            Ok("update".to_string())
        }
        (None, None) => Err(format!(
            "Invalid journal entry: both old_row and new_row are NULL for {table_name}.{pk_value}"
        )),
    }
}

/// Insert a row from a JSON string.
fn insert_row_from_json(conn: &Connection, table: &str, json: &str) -> Result<(), String> {
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|e| format!("Parse row JSON for insert: {e}"))?;

    let cols: Vec<String> = map.keys().map(|k| format!("\"{k}\"")).collect();
    let placeholders: Vec<String> = (1..=map.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "INSERT OR REPLACE INTO \"{}\" ({}) VALUES ({})",
        table,
        cols.join(", "),
        placeholders.join(", "),
    );

    let values: Vec<Box<dyn rusqlite::types::ToSql>> =
        map.values().map(json_value_to_sql).collect();
    let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();

    conn.execute(&sql, refs.as_slice())
        .map_err(|e| format!("Insert from JSON into {table}: {e}"))?;
    Ok(())
}

/// Update a row from a JSON string. Sets all columns except the PK.
fn update_row_from_json(
    conn: &Connection,
    table: &str,
    pk_col: &str,
    pk_value: &str,
    json: &str,
) -> Result<(), String> {
    let map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|e| format!("Parse row JSON for update: {e}"))?;

    let set_clauses: Vec<String> = map
        .keys()
        .filter(|k| k.as_str() != pk_col)
        .map(|k| format!("\"{k}\" = ?"))
        .collect();

    if set_clauses.is_empty() {
        return Ok(());
    }

    let sql = format!(
        "UPDATE \"{}\" SET {} WHERE \"{}\" = ?",
        table,
        set_clauses.join(", "),
        pk_col,
    );

    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = map
        .iter()
        .filter(|(k, _)| k.as_str() != pk_col)
        .map(|(_, v)| json_value_to_sql(v))
        .collect();
    values.push(Box::new(pk_value.to_string()));

    let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();

    conn.execute(&sql, refs.as_slice())
        .map_err(|e| format!("Update from JSON in {table}: {e}"))?;
    Ok(())
}

/// Convert a serde_json::Value to a boxed ToSql parameter.
fn json_value_to_sql(val: &serde_json::Value) -> Box<dyn rusqlite::types::ToSql> {
    match val {
        serde_json::Value::Null => Box::new(Option::<String>::None),
        serde_json::Value::Bool(b) => Box::new(if *b { 1i64 } else { 0i64 }),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Box::new(i)
            } else if let Some(f) = n.as_f64() {
                Box::new(f)
            } else {
                Box::new(n.to_string())
            }
        }
        serde_json::Value::String(s) => Box::new(s.clone()),
        other => Box::new(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_staging_data_table_ddl_has_tracking_cols() {
        // Verify that staging DDL includes the three tracking columns
        let conn = Connection::open_in_memory().unwrap();
        create_staging_meta_tables(&conn).unwrap();

        let ts = TableSchema {
            table_name: "test_table".to_string(),
            pk_strategy: PkStrategy::Uuid,
            source_type: "lsx".to_string(),
            region_id: None,
            node_id: None,
            columns: vec![ColumnDef {
                name: "Name".to_string(),
                bg3_type: "FixedString".to_string(),
                sqlite_type: "TEXT".to_string(),
            }],
            fk_constraints: vec![],
            has_file_id: true,
            parent_tables: std::collections::HashSet::new(),
        };

        create_staging_data_table(&conn, &ts).unwrap();

        // Check columns exist
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(test_table)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(cols.contains(&"UUID".to_string()), "Missing UUID PK");
        assert!(cols.contains(&"Name".to_string()), "Missing Name column");
        assert!(
            cols.contains(&"_is_modified".to_string()),
            "Missing _is_modified"
        );
        assert!(cols.contains(&"_is_new".to_string()), "Missing _is_new");
        assert!(
            cols.contains(&"_is_deleted".to_string()),
            "Missing _is_deleted"
        );
        assert!(cols.contains(&"_file_id".to_string()), "Missing _file_id");
        assert!(cols.contains(&"_SourceID".to_string()), "Missing _SourceID");
    }

    #[test]
    fn test_staging_junction_has_tracking_cols() {
        let conn = Connection::open_in_memory().unwrap();

        // Staging junctions should have tracking columns
        conn.execute_batch(
            "CREATE TABLE jct_test (\n\
             \x20   parent_id TEXT NOT NULL,\n\
             \x20   child_id TEXT NOT NULL,\n\
             \x20   \"_is_modified\" INTEGER NOT NULL DEFAULT 0,\n\
             \x20   \"_is_new\" INTEGER NOT NULL DEFAULT 0,\n\
             \x20   \"_is_deleted\" INTEGER NOT NULL DEFAULT 0,\n\
             \x20   PRIMARY KEY (parent_id, child_id)\n\
             ) WITHOUT ROWID",
        )
        .unwrap();

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(jct_test)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(cols.len(), 5);
        assert!(cols.contains(&"parent_id".to_string()));
        assert!(cols.contains(&"child_id".to_string()));
        assert!(cols.contains(&"_is_modified".to_string()));
        assert!(cols.contains(&"_is_new".to_string()));
        assert!(cols.contains(&"_is_deleted".to_string()));
    }

    #[test]
    fn test_staging_rowid_table_no_sourceid() {
        let conn = Connection::open_in_memory().unwrap();
        create_staging_meta_tables(&conn).unwrap();

        let ts = TableSchema {
            table_name: "child_table".to_string(),
            pk_strategy: PkStrategy::Rowid,
            source_type: "lsx".to_string(),
            region_id: None,
            node_id: None,
            columns: vec![ColumnDef {
                name: "Value".to_string(),
                bg3_type: "int32".to_string(),
                sqlite_type: "INTEGER".to_string(),
            }],
            fk_constraints: vec![],
            has_file_id: false,
            parent_tables: std::collections::HashSet::new(),
        };

        create_staging_data_table(&conn, &ts).unwrap();

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(child_table)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        // Rowid tables should NOT have _SourceID (children inherit parent's source)
        assert!(
            !cols.contains(&"_SourceID".to_string()),
            "_SourceID should not exist on Rowid table"
        );
        // But SHOULD have tracking columns
        assert!(cols.contains(&"_is_modified".to_string()));
        assert!(cols.contains(&"_is_new".to_string()));
        assert!(cols.contains(&"_is_deleted".to_string()));
    }

    #[test]
    fn staging_meta_tables_created_successfully() {
        let conn = Connection::open_in_memory().unwrap();
        create_staging_meta_tables(&conn).unwrap();

        // Verify all five meta tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"_sources".to_string()), "Missing _sources");
        assert!(
            tables.contains(&"_source_files".to_string()),
            "Missing _source_files"
        );
        assert!(
            tables.contains(&"_column_types".to_string()),
            "Missing _column_types"
        );
        assert!(
            tables.contains(&"_table_meta".to_string()),
            "Missing _table_meta"
        );
        assert!(
            tables.contains(&"_staging_authoring".to_string()),
            "Missing _staging_authoring"
        );
    }

    #[test]
    fn staging_uuid_pk_table_has_uuid_as_pk() {
        // When PK is UUID, the PK already ensures uniqueness
        let conn = Connection::open_in_memory().unwrap();
        create_staging_meta_tables(&conn).unwrap();

        let ts = TableSchema {
            table_name: "uuid_pk_table".to_string(),
            pk_strategy: PkStrategy::Uuid,
            source_type: "lsx".to_string(),
            region_id: None,
            node_id: None,
            columns: vec![ColumnDef {
                name: "Name".to_string(),
                bg3_type: "FixedString".to_string(),
                sqlite_type: "TEXT".to_string(),
            }],
            fk_constraints: vec![],
            has_file_id: true,
            parent_tables: std::collections::HashSet::new(),
        };

        create_staging_data_table(&conn, &ts).unwrap();

        // UUID is the PK — verify it
        let pk_cols: Vec<(String, i32)> = conn
            .prepare("PRAGMA table_info(uuid_pk_table)")
            .unwrap()
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let pk: i32 = row.get(5)?;
                Ok((name, pk))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        let uuid_entry = pk_cols.iter().find(|(name, _)| name == "UUID");
        assert!(uuid_entry.is_some(), "UUID column should exist");
        assert!(uuid_entry.unwrap().1 != 0, "UUID should be the PK");
    }

    #[test]
    fn staging_summary_serializes_correctly() {
        let summary = StagingSummary {
            db_path: "/tmp/staging.sqlite".to_string(),
            total_tables: 42,
            junction_tables: 5,
            elapsed_secs: 1.234,
            db_size_mb: 0.5,
        };

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"total_tables\":42"));
        assert!(json.contains("\"junction_tables\":5"));
        assert!(json.contains("\"db_path\":\"/tmp/staging.sqlite\""));
    }

    // -------------------------------------------------------------------
    // F1–F6 unit tests
    // -------------------------------------------------------------------

    /// Set up an in-memory staging DB with a test table and embedded schema.
    fn setup_test_staging_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_staging_meta_tables(&conn).unwrap();

        // Data table with UUID PK
        conn.execute_batch(
            "CREATE TABLE \"test_entries\" (
                \"UUID\" TEXT PRIMARY KEY,
                \"_file_id\" INTEGER,
                \"_SourceID\" INTEGER,
                \"Name\" TEXT,
                \"Description\" TEXT,
                \"_is_modified\" INTEGER NOT NULL DEFAULT 0,
                \"_is_new\" INTEGER NOT NULL DEFAULT 0,
                \"_is_deleted\" INTEGER NOT NULL DEFAULT 0
            )",
        )
        .unwrap();

        // Embedded schema storage
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _embedded_schema (
                key   TEXT PRIMARY KEY,
                value BLOB NOT NULL
            ) WITHOUT ROWID",
        )
        .unwrap();

        // Build a DiscoveredSchema with the test table
        let mut tables = HashMap::new();
        tables.insert(
            "test_entries".to_string(),
            TableSchema {
                table_name: "test_entries".to_string(),
                pk_strategy: PkStrategy::Uuid,
                source_type: "lsx".to_string(),
                region_id: Some("TestRegion".to_string()),
                node_id: Some("TestNode".to_string()),
                columns: vec![
                    ColumnDef {
                        name: "Name".to_string(),
                        bg3_type: "FixedString".to_string(),
                        sqlite_type: "TEXT".to_string(),
                    },
                    ColumnDef {
                        name: "Description".to_string(),
                        bg3_type: "LSString".to_string(),
                        sqlite_type: "TEXT".to_string(),
                    },
                ],
                fk_constraints: vec![],
                has_file_id: true,
                parent_tables: std::collections::HashSet::new(),
            },
        );

        let schema = DiscoveredSchema {
            tables,
            renames: HashMap::new(),
            region_node_ids: HashMap::new(),
            junction_tables: vec![],
            junction_lookup: HashMap::new(),
        };

        let blob = rmp_serde::to_vec(&schema).unwrap();
        conn.execute(
            "INSERT INTO _embedded_schema (key, value) VALUES ('schema', ?1)",
            rusqlite::params![blob],
        )
        .unwrap();

        conn
    }

    #[test]
    fn upsert_inserts_new_row_with_is_new() {
        let conn = setup_test_staging_db();
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "test-uuid-1".to_string());
        cols.insert("Name".to_string(), "TestEntry".to_string());

        let result = staging_upsert_row(&conn, "test_entries", &cols, true).unwrap();
        assert!(result.was_insert);
        assert_eq!(result.pk_value, "test-uuid-1");

        // Verify tracking flags
        let (is_new, is_mod): (i32, i32) = conn
            .query_row(
                "SELECT \"_is_new\", \"_is_modified\" FROM test_entries WHERE \"UUID\" = ?1",
                ["test-uuid-1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(is_new, 1);
        assert_eq!(is_mod, 0);
    }

    #[test]
    fn upsert_existing_row_sets_is_modified() {
        let conn = setup_test_staging_db();

        // Insert a vanilla row (not new)
        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('uuid-2', 'Original')",
            [],
        )
        .unwrap();

        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "uuid-2".to_string());
        cols.insert("Name".to_string(), "Modified".to_string());

        let result = staging_upsert_row(&conn, "test_entries", &cols, false).unwrap();
        assert!(!result.was_insert);

        let (is_new, is_mod): (i32, i32) = conn
            .query_row(
                "SELECT \"_is_new\", \"_is_modified\" FROM test_entries WHERE \"UUID\" = ?1",
                ["uuid-2"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(is_new, 0);
        assert_eq!(is_mod, 1);

        // Verify data updated
        let name: String = conn
            .query_row(
                "SELECT \"Name\" FROM test_entries WHERE \"UUID\" = ?1",
                ["uuid-2"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Modified");
    }

    #[test]
    fn upsert_preserves_is_new_on_re_upsert() {
        let conn = setup_test_staging_db();

        // Insert as new
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "uuid-3".to_string());
        cols.insert("Name".to_string(), "First".to_string());
        staging_upsert_row(&conn, "test_entries", &cols, true).unwrap();

        // Re-upsert — should keep _is_new=1
        cols.insert("Name".to_string(), "Second".to_string());
        let result = staging_upsert_row(&conn, "test_entries", &cols, false).unwrap();
        assert!(!result.was_insert);

        let is_new: i32 = conn
            .query_row(
                "SELECT \"_is_new\" FROM test_entries WHERE \"UUID\" = ?1",
                ["uuid-3"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_new, 1, "_is_new should be preserved on re-upsert");
    }

    #[test]
    fn soft_delete_sets_is_deleted() {
        let conn = setup_test_staging_db();

        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('uuid-4', 'ToDelete')",
            [],
        )
        .unwrap();

        let result = staging_mark_deleted(&conn, "test_entries", "uuid-4").unwrap();
        assert!(result);

        let is_deleted: i32 = conn
            .query_row(
                "SELECT \"_is_deleted\" FROM test_entries WHERE \"UUID\" = ?1",
                ["uuid-4"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_deleted, 1);
    }

    #[test]
    fn soft_delete_on_new_row_hard_deletes() {
        let conn = setup_test_staging_db();

        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_new\") VALUES ('uuid-5', 'New', 1)",
            [],
        ).unwrap();

        let result = staging_mark_deleted(&conn, "test_entries", "uuid-5").unwrap();
        assert!(result);

        // Row should be gone entirely
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = ?1",
                ["uuid-5"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "New row should be hard-deleted");
    }

    #[test]
    fn undelete_clears_is_deleted() {
        let conn = setup_test_staging_db();

        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_deleted\") VALUES ('uuid-6', 'Deleted', 1)",
            [],
        ).unwrap();

        let result = staging_unmark_deleted(&conn, "test_entries", "uuid-6").unwrap();
        assert!(result);

        let is_deleted: i32 = conn
            .query_row(
                "SELECT \"_is_deleted\" FROM test_entries WHERE \"UUID\" = ?1",
                ["uuid-6"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_deleted, 0);
    }

    #[test]
    fn batch_write_rolls_back_on_failure() {
        let conn = setup_test_staging_db();

        let mut good_cols = HashMap::new();
        good_cols.insert("UUID".to_string(), "batch-1".to_string());
        good_cols.insert("Name".to_string(), "Good".to_string());

        let ops = vec![
            StagingOperation::Upsert {
                table: "test_entries".to_string(),
                columns: good_cols,
                is_new: true,
            },
            // This should fail — nonexistent table
            StagingOperation::MarkDeleted {
                table: "nonexistent_table".to_string(),
                pk: "x".to_string(),
            },
        ];

        let result = staging_batch_write(&conn, &ops).unwrap();
        assert_eq!(result.succeeded, 1);
        assert_eq!(result.failed, 1);
        assert!(!result.errors.is_empty());

        // The first op should have been rolled back
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'batch-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "Rolled-back row should not exist");
    }

    #[test]
    fn query_changes_returns_correct_types() {
        let conn = setup_test_staging_db();

        conn.execute_batch(
            "INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_new\") VALUES ('new-1', 'New', 1);
             INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_modified\") VALUES ('mod-1', 'Mod', 1);
             INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_deleted\") VALUES ('del-1', 'Del', 1);
             INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('clean-1', 'Clean');"
        ).unwrap();

        let changes = staging_query_changes(&conn, Some("test_entries")).unwrap();
        assert_eq!(
            changes.len(),
            3,
            "Should return 3 changed rows, not the clean one"
        );

        let types: Vec<&str> = changes.iter().map(|c| c.change_type.as_str()).collect();
        assert!(types.contains(&"new"));
        assert!(types.contains(&"modified"));
        assert!(types.contains(&"deleted"));
    }

    #[test]
    fn section_list_returns_accurate_counts() {
        let conn = setup_test_staging_db();

        conn.execute_batch(
            "INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_new\") VALUES ('a', 'A', 1);
             INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_modified\") VALUES ('b', 'B', 1);
             INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_deleted\") VALUES ('c', 'C', 1);
             INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('d', 'D');",
        )
        .unwrap();

        let sections = staging_list_sections(&conn).unwrap();
        assert_eq!(sections.len(), 1);

        let s = &sections[0];
        assert_eq!(s.table_name, "test_entries");
        assert_eq!(s.total_rows, 4);
        assert_eq!(s.active_rows, 3); // all except deleted
        assert_eq!(s.new_rows, 1);
        assert_eq!(s.modified_rows, 1);
        assert_eq!(s.deleted_rows, 1);
    }

    #[test]
    fn staging_authoring_table_has_correct_columns() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_staging_authoring_table(&conn).unwrap();

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(_staging_authoring)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(cols.contains(&"key".to_string()));
        assert!(cols.contains(&"value".to_string()));
        assert!(cols.contains(&"_is_new".to_string()));
        assert!(cols.contains(&"_is_modified".to_string()));
        assert!(cols.contains(&"_is_deleted".to_string()));
        assert!(cols.contains(&"_original_hash".to_string()));
    }

    #[test]
    fn get_meta_returns_none_for_missing_key() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_staging_authoring_table(&conn).unwrap();

        let result = staging_get_meta(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn set_meta_creates_and_replaces_values() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_staging_authoring_table(&conn).unwrap();

        staging_set_meta(&conn, "author", "TestUser").unwrap();
        assert_eq!(
            staging_get_meta(&conn, "author").unwrap(),
            Some("TestUser".to_string())
        );

        // Replace
        staging_set_meta(&conn, "author", "NewUser").unwrap();
        assert_eq!(
            staging_get_meta(&conn, "author").unwrap(),
            Some("NewUser".to_string())
        );
    }

    #[test]
    fn get_row_returns_none_for_missing() {
        let conn = setup_test_staging_db();
        let result = staging_get_row(&conn, "test_entries", "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_row_returns_data_for_existing() {
        let conn = setup_test_staging_db();
        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('row-1', 'Hello')",
            [],
        )
        .unwrap();

        let result = staging_get_row(&conn, "test_entries", "row-1").unwrap();
        assert!(result.is_some());
        let map = result.unwrap();
        assert_eq!(map.get("Name").unwrap(), &serde_json::json!("Hello"));
    }

    #[test]
    fn query_section_excludes_deleted_by_default() {
        let conn = setup_test_staging_db();
        conn.execute_batch(
            "INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('s1', 'Active');
             INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_deleted\") VALUES ('s2', 'Gone', 1);"
        ).unwrap();

        let rows = staging_query_section(&conn, "test_entries", false).unwrap();
        assert_eq!(rows.len(), 1);

        let rows_all = staging_query_section(&conn, "test_entries", true).unwrap();
        assert_eq!(rows_all.len(), 2);
    }

    #[test]
    fn delete_nonexistent_row_returns_false() {
        let conn = setup_test_staging_db();
        let result = staging_mark_deleted(&conn, "test_entries", "no-such-uuid").unwrap();
        assert!(!result);
    }

    #[test]
    fn validate_rejects_unknown_table() {
        let conn = setup_test_staging_db();
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "x".to_string());

        let err = staging_upsert_row(&conn, "bogus_table", &cols, true);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("not found in staging schema"));
    }

    // -------------------------------------------------------------------
    // Undo journal tests
    // -------------------------------------------------------------------

    #[test]
    fn undo_journal_table_created() {
        let conn = setup_test_staging_db();
        ensure_undo_journal_table(&conn).unwrap();

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='_staging_undo_journal'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "Journal table should exist");
    }

    #[test]
    fn snapshot_creates_boundary_marker() {
        let conn = setup_test_staging_db();
        let id = staging_snapshot(&conn, "test snapshot").unwrap();
        assert!(id > 0);

        let (table_name, pk_value, label): (String, String, String) = conn
            .query_row(
                "SELECT table_name, pk_value, label FROM _staging_undo_journal WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(table_name, "__boundary__");
        assert_eq!(pk_value, "");
        assert_eq!(label, "test snapshot");
    }

    #[test]
    fn record_change_stores_journal_entry() {
        let conn = setup_test_staging_db();
        ensure_undo_journal_table(&conn).unwrap();

        staging_record_change(
            &conn,
            "upsert",
            "test_entries",
            "uuid-1",
            None,
            Some("{\"UUID\":\"uuid-1\"}"),
        )
        .unwrap();

        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM _staging_undo_journal WHERE table_name != '__boundary__'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn undo_reverts_insert() {
        let conn = setup_test_staging_db();

        // Create journal and first snapshot
        staging_snapshot(&conn, "before insert").unwrap();

        // Insert a row (auto-records journal entry since journal table exists)
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "undo-test-1".to_string());
        cols.insert("Name".to_string(), "TestName".to_string());
        staging_upsert_row(&conn, "test_entries", &cols, true).unwrap();

        // Close the group
        staging_snapshot(&conn, "after insert").unwrap();

        // Verify row exists
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'undo-test-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "Row should exist before undo");

        // Undo
        let replayed = staging_undo(&conn).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].table_name, "test_entries");
        assert_eq!(replayed[0].action, "delete");

        // Verify row is gone
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'undo-test-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "Row should be gone after undo");
    }

    #[test]
    fn redo_restores_insert() {
        let conn = setup_test_staging_db();

        staging_snapshot(&conn, "before insert").unwrap();
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "redo-test-1".to_string());
        cols.insert("Name".to_string(), "RedoName".to_string());
        staging_upsert_row(&conn, "test_entries", &cols, true).unwrap();
        staging_snapshot(&conn, "after insert").unwrap();

        // Undo
        staging_undo(&conn).unwrap();
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'redo-test-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        // Redo
        let replayed = staging_redo(&conn).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].action, "insert");

        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'redo-test-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "Row should be restored after redo");
    }

    #[test]
    fn undo_reverts_update() {
        let conn = setup_test_staging_db();

        // Pre-populate a row
        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('upd-1', 'Original')",
            [],
        )
        .unwrap();

        staging_snapshot(&conn, "before update").unwrap();

        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "upd-1".to_string());
        cols.insert("Name".to_string(), "Modified".to_string());
        staging_upsert_row(&conn, "test_entries", &cols, false).unwrap();

        staging_snapshot(&conn, "after update").unwrap();

        // Verify updated
        let name: String = conn
            .query_row(
                "SELECT \"Name\" FROM test_entries WHERE \"UUID\" = 'upd-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Modified");

        // Undo
        let replayed = staging_undo(&conn).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(replayed[0].action, "update");

        // Verify restored
        let name: String = conn
            .query_row(
                "SELECT \"Name\" FROM test_entries WHERE \"UUID\" = 'upd-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Original");
    }

    #[test]
    fn undo_restores_soft_deleted_row() {
        let conn = setup_test_staging_db();

        // Insert a vanilla row (not new)
        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_new\") VALUES ('del-1', 'ToDelete', 0)",
            [],
        )
        .unwrap();

        staging_snapshot(&conn, "before delete").unwrap();
        staging_mark_deleted(&conn, "test_entries", "del-1").unwrap();
        staging_snapshot(&conn, "after delete").unwrap();

        // Verify soft deleted
        let is_deleted: i32 = conn
            .query_row(
                "SELECT \"_is_deleted\" FROM test_entries WHERE \"UUID\" = 'del-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_deleted, 1);

        // Undo
        staging_undo(&conn).unwrap();

        let is_deleted: i32 = conn
            .query_row(
                "SELECT \"_is_deleted\" FROM test_entries WHERE \"UUID\" = 'del-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_deleted, 0, "Row should be undeleted after undo");
    }

    #[test]
    fn undo_after_hard_delete_restores_row() {
        let conn = setup_test_staging_db();

        // Insert a new row (will be hard-deleted)
        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\", \"_is_new\") VALUES ('hdel-1', 'NewRow', 1)",
            [],
        )
        .unwrap();

        staging_snapshot(&conn, "before hard delete").unwrap();
        staging_mark_deleted(&conn, "test_entries", "hdel-1").unwrap();
        staging_snapshot(&conn, "after hard delete").unwrap();

        // Verify gone
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'hdel-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        // Undo
        staging_undo(&conn).unwrap();

        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'hdel-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "Row should be restored after undo of hard delete");
    }

    #[test]
    fn multiple_sequential_undos() {
        let conn = setup_test_staging_db();

        // Group 1: insert row A
        staging_snapshot(&conn, "start 1").unwrap();
        let mut cols_a = HashMap::new();
        cols_a.insert("UUID".to_string(), "seq-a".to_string());
        cols_a.insert("Name".to_string(), "A".to_string());
        staging_upsert_row(&conn, "test_entries", &cols_a, true).unwrap();
        staging_snapshot(&conn, "end 1").unwrap();

        // Group 2: insert row B
        let mut cols_b = HashMap::new();
        cols_b.insert("UUID".to_string(), "seq-b".to_string());
        cols_b.insert("Name".to_string(), "B".to_string());
        staging_upsert_row(&conn, "test_entries", &cols_b, true).unwrap();
        staging_snapshot(&conn, "end 2").unwrap();

        // Both rows exist
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM test_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Undo group 2
        let r1 = staging_undo(&conn).unwrap();
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].pk_value, "seq-b");

        // Only row A remains
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM test_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Undo group 1
        let r2 = staging_undo(&conn).unwrap();
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].pk_value, "seq-a");

        // No rows
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM test_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn snapshot_after_undo_prunes_redo_history() {
        let conn = setup_test_staging_db();

        // Group 1: insert row
        staging_snapshot(&conn, "start 1").unwrap();
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "prune-1".to_string());
        cols.insert("Name".to_string(), "First".to_string());
        staging_upsert_row(&conn, "test_entries", &cols, true).unwrap();
        staging_snapshot(&conn, "end 1").unwrap();

        // Undo group 1
        staging_undo(&conn).unwrap();

        // Now create a new group (should prune redo history)
        staging_snapshot(&conn, "new start").unwrap();
        cols.insert("UUID".to_string(), "prune-2".to_string());
        cols.insert("Name".to_string(), "Second".to_string());
        staging_upsert_row(&conn, "test_entries", &cols, true).unwrap();
        staging_snapshot(&conn, "new end").unwrap();

        // Redo should return empty (redo history was pruned)
        let redo_result = staging_redo(&conn).unwrap();
        assert!(redo_result.is_empty(), "Redo should be empty after prune");
    }

    #[test]
    fn undo_with_no_journal_returns_empty() {
        let conn = setup_test_staging_db();
        ensure_undo_journal_table(&conn).unwrap();

        let result = staging_undo(&conn).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn redo_with_nothing_to_redo_returns_empty() {
        let conn = setup_test_staging_db();
        ensure_undo_journal_table(&conn).unwrap();

        let result = staging_redo(&conn).unwrap();
        assert!(result.is_empty());
    }

    // -------------------------------------------------------------------
    // S-ERRTEST §9: SQL injection via table name
    // -------------------------------------------------------------------
    // `resolve_staging_table` acts as a whitelist — only table names present
    // in the embedded schema are accepted.  Table names with SQL
    // metacharacters must be rejected before they ever reach a format!() SQL
    // string.

    #[test]
    fn upsert_rejects_sql_injection_table_names() {
        let conn = setup_test_staging_db();
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "test-uuid".to_string());

        let injection_names = [
            "entries; DROP TABLE entries;",
            "test\"; --",
            "test' OR '1'='1",
            "Robert'); DROP TABLE Students;--",
            "test\"; DELETE FROM test_entries WHERE \"1\"=\"1",
            "__boundary__", // undo journal sentinel — must not be resolvable
        ];

        for name in &injection_names {
            let result = staging_upsert_row(&conn, name, &cols, true);
            assert!(result.is_err(), "Should reject table name: {name}");
            let err = result.unwrap_err();
            assert!(
                err.contains("not found in staging schema") || err.contains("not in schema"),
                "Expected schema-not-found error for '{name}', got: {err}",
            );
        }

        // Verify the real table is unharmed
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM test_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "test_entries should be untouched after injection attempts"
        );
    }

    #[test]
    fn mark_deleted_rejects_sql_injection_table_names() {
        let conn = setup_test_staging_db();

        // Seed a row to verify it survives
        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('safe-row', 'Safe')",
            [],
        )
        .unwrap();

        for name in &["entries; DROP TABLE entries;", "test\"; --"] {
            let result = staging_mark_deleted(&conn, name, "safe-row");
            assert!(result.is_err(), "Should reject table name: {name}");
        }

        // Row survives
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM test_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "Row should survive injection attempts");
    }

    #[test]
    fn query_section_rejects_sql_injection_table_names() {
        let conn = setup_test_staging_db();

        let result = staging_query_section(&conn, "test\"; DROP TABLE test_entries; --", false);
        assert!(result.is_err());

        // Table survives
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='test_entries'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "test_entries must still exist");
    }

    #[test]
    fn get_row_rejects_sql_injection_table_names() {
        let conn = setup_test_staging_db();

        let result = staging_get_row(&conn, "test\"; DROP TABLE test_entries; --", "any-pk");
        assert!(result.is_err());
    }

    // -------------------------------------------------------------------
    // S-ERRTEST §10: batch_write — mixed valid/invalid → atomic rollback
    // -------------------------------------------------------------------

    #[test]
    fn batch_write_atomic_rollback_on_missing_pk() {
        let conn = setup_test_staging_db();

        let mut good_cols = HashMap::new();
        good_cols.insert("UUID".to_string(), "batch-ok-1".to_string());
        good_cols.insert("Name".to_string(), "Good".to_string());

        // Second upsert is missing the PK column — should fail
        let mut bad_cols = HashMap::new();
        bad_cols.insert("Name".to_string(), "NoPK".to_string());

        let ops = vec![
            StagingOperation::Upsert {
                table: "test_entries".to_string(),
                columns: good_cols,
                is_new: true,
            },
            StagingOperation::Upsert {
                table: "test_entries".to_string(),
                columns: bad_cols,
                is_new: true,
            },
        ];

        let result = staging_batch_write(&conn, &ops).unwrap();
        assert!(
            result.failed > 0,
            "Second op should fail: {:?}",
            result.errors
        );
        assert_eq!(result.succeeded, 1, "Only first op ran before failure");

        // Atomicity check: the successful first op must be rolled back
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'batch-ok-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "Rolled-back row from first op should not exist");
    }

    #[test]
    fn batch_write_succeeds_when_all_ops_valid() {
        let conn = setup_test_staging_db();

        let mut cols_a = HashMap::new();
        cols_a.insert("UUID".to_string(), "batch-a".to_string());
        cols_a.insert("Name".to_string(), "A".to_string());

        let mut cols_b = HashMap::new();
        cols_b.insert("UUID".to_string(), "batch-b".to_string());
        cols_b.insert("Name".to_string(), "B".to_string());

        let ops = vec![
            StagingOperation::Upsert {
                table: "test_entries".to_string(),
                columns: cols_a,
                is_new: true,
            },
            StagingOperation::Upsert {
                table: "test_entries".to_string(),
                columns: cols_b,
                is_new: true,
            },
        ];

        let result = staging_batch_write(&conn, &ops).unwrap();
        assert_eq!(result.succeeded, 2);
        assert_eq!(result.failed, 0);
        assert!(result.errors.is_empty());

        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM test_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "Both rows should be committed");
    }

    #[test]
    fn batch_write_rollback_preserves_pre_existing_data() {
        let conn = setup_test_staging_db();

        // Pre-existing row that must survive a failed batch
        conn.execute(
            "INSERT INTO test_entries (\"UUID\", \"Name\") VALUES ('existing', 'Existing')",
            [],
        )
        .unwrap();

        let mut good_cols = HashMap::new();
        good_cols.insert("UUID".to_string(), "batch-new".to_string());
        good_cols.insert("Name".to_string(), "New".to_string());

        let ops = vec![
            StagingOperation::Upsert {
                table: "test_entries".to_string(),
                columns: good_cols,
                is_new: true,
            },
            StagingOperation::MarkDeleted {
                table: "nonexistent_table".to_string(),
                pk: "x".to_string(),
            },
        ];

        let result = staging_batch_write(&conn, &ops).unwrap();
        assert!(result.failed > 0);

        // Pre-existing data must be intact
        let name: String = conn
            .query_row(
                "SELECT \"Name\" FROM test_entries WHERE \"UUID\" = 'existing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Existing", "Pre-existing row must be untouched");

        // New row must NOT exist (rolled back)
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM test_entries WHERE \"UUID\" = 'batch-new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn batch_write_error_report_identifies_failing_op() {
        let conn = setup_test_staging_db();

        let mut good_cols = HashMap::new();
        good_cols.insert("UUID".to_string(), "rpt-1".to_string());
        good_cols.insert("Name".to_string(), "Good".to_string());

        let ops = vec![
            StagingOperation::Upsert {
                table: "test_entries".to_string(),
                columns: good_cols,
                is_new: true,
            },
            StagingOperation::MarkDeleted {
                table: "nonexistent_table".to_string(),
                pk: "x".to_string(),
            },
        ];

        let result = staging_batch_write(&conn, &ops).unwrap();
        assert_eq!(result.errors.len(), 1);
        // Error message should reference operation index 1
        assert!(
            result.errors[0].contains("Operation 1"),
            "Error should reference failing op index, got: {}",
            result.errors[0],
        );
    }

    // -------------------------------------------------------------------
    // S-ERRTEST §12: DB-not-found / missing schema patterns
    // -------------------------------------------------------------------
    // Note: The staging CRUD functions (upsert, get_row, etc.) take a
    // `&Connection`, not a path.  Path validation happens at the command
    // layer in `commands/db.rs` (checks `db.is_file()` before opening).
    // Here we test the library-level functions with:
    //   (a) `create_staging_db` given a nonexistent schema source
    //   (b) CRUD functions on a connection missing the embedded schema

    #[test]
    fn create_staging_db_fails_on_nonexistent_schema() {
        let bad_schema = std::path::Path::new("/tmp/nonexistent_dir_xyz/schema.sqlite");
        let staging_out = std::path::Path::new("/tmp/nonexistent_dir_xyz/staging.sqlite");

        let result = create_staging_db(bad_schema, staging_out);
        assert!(result.is_err(), "Should fail when schema DB doesn't exist");
    }

    #[test]
    fn upsert_fails_on_connection_without_schema() {
        // Simulates opening a DB that isn't actually a staging DB
        let conn = Connection::open_in_memory().unwrap();
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "x".to_string());

        let result = staging_upsert_row(&conn, "test_entries", &cols, true);
        assert!(result.is_err(), "Should fail without embedded schema");
    }

    #[test]
    fn get_row_fails_on_connection_without_schema() {
        let conn = Connection::open_in_memory().unwrap();

        let result = staging_get_row(&conn, "test_entries", "any-pk");
        assert!(result.is_err(), "Should fail without embedded schema");
    }

    #[test]
    fn query_section_fails_on_connection_without_schema() {
        let conn = Connection::open_in_memory().unwrap();

        let result = staging_query_section(&conn, "test_entries", false);
        assert!(result.is_err(), "Should fail without embedded schema");
    }

    #[test]
    fn mark_deleted_fails_on_connection_without_schema() {
        let conn = Connection::open_in_memory().unwrap();

        let result = staging_mark_deleted(&conn, "test_entries", "any-pk");
        assert!(result.is_err(), "Should fail without embedded schema");
    }

    #[test]
    fn staging_list_sections_fails_on_connection_without_schema() {
        let conn = Connection::open_in_memory().unwrap();

        let result = staging_list_sections(&conn);
        assert!(result.is_err(), "Should fail without embedded schema");
    }

    // -------------------------------------------------------------------
    // S-ERRTEST §13: undo/redo on empty journal — graceful handling
    // -------------------------------------------------------------------
    // The functions call ensure_undo_journal_table + ensure_staging_authoring_table
    // internally, so even a brand-new DB with no journal table should work.

    #[test]
    fn undo_on_fresh_staging_db_without_journal_table_returns_ok_empty() {
        let conn = setup_test_staging_db();
        // Deliberately do NOT call ensure_undo_journal_table — undo creates it

        let result = staging_undo(&conn);
        assert!(result.is_ok(), "Undo should not error: {:?}", result.err());
        assert!(
            result.unwrap().is_empty(),
            "Should return empty replay list"
        );
    }

    #[test]
    fn redo_on_fresh_staging_db_without_journal_table_returns_ok_empty() {
        let conn = setup_test_staging_db();

        let result = staging_redo(&conn);
        assert!(result.is_ok(), "Redo should not error: {:?}", result.err());
        assert!(
            result.unwrap().is_empty(),
            "Should return empty replay list"
        );
    }

    #[test]
    fn undo_then_redo_on_fresh_db_both_graceful() {
        let conn = setup_test_staging_db();

        // Undo on empty journal
        let undo_result = staging_undo(&conn).unwrap();
        assert!(undo_result.is_empty());

        // Redo immediately after (still nothing to redo)
        let redo_result = staging_redo(&conn).unwrap();
        assert!(redo_result.is_empty());

        // A second round should also be fine
        let undo_result2 = staging_undo(&conn).unwrap();
        assert!(undo_result2.is_empty());
    }

    #[test]
    fn redo_without_prior_undo_returns_empty() {
        let conn = setup_test_staging_db();

        // Create a snapshot and do some work
        staging_snapshot(&conn, "initial").unwrap();
        let mut cols = HashMap::new();
        cols.insert("UUID".to_string(), "redo-no-undo".to_string());
        cols.insert("Name".to_string(), "Test".to_string());
        staging_upsert_row(&conn, "test_entries", &cols, true).unwrap();
        staging_snapshot(&conn, "after insert").unwrap();

        // Try to redo without undoing first — pointer is already at the latest boundary
        let result = staging_redo(&conn).unwrap();
        assert!(
            result.is_empty(),
            "Redo without prior undo should return empty"
        );
    }
}
