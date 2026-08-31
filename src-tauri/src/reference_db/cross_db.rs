//! Cross-database reference resolution via ATTACH.
//!
//! SQLite FK constraints only enforce within a single database file.
//! In our multi-DB architecture, the honor, mods, and staging databases
//! all reference vanilla data that lives in `ref_base.sqlite`.
//!
//! This module provides:
//!   - `RefDbConn`: a connection wrapper that ATTACHes reference DBs read-only
//!   - `validate_cross_db_fks()`: checks FK references across attached DBs
//!   - `cross_db_union_sql()`: generates UNION ALL queries for lookups
//!
//! Attachment aliases:
//!   - `main`  — the primary DB being operated on (implicit)
//!   - `base`  — ref_base.sqlite (vanilla data)
//!   - `honor` — ref_honor.sqlite (honour-mode overrides)
//!   - `mods`  — ref_mods.sqlite (ingested mod data)

use rusqlite::{params, Connection};
use std::path::Path;

/// Well-known schema aliases for ATTACH.
pub const ALIAS_BASE: &str = "base";
pub const ALIAS_HONOR: &str = "honor";
pub const ALIAS_MODS: &str = "mods";

/// Read-only connection wrapper for a reference DB plus attached fallback DBs.
pub struct RefDbConn {
    conn: Connection,
    attached_schemas: Vec<String>,
}

impl RefDbConn {
    /// Open a reference DB with no attachments.
    pub fn open(db_path: &Path) -> Result<Self, String> {
        let conn = Connection::open(db_path)
            .map_err(|e| format!("Open DB {}: {}", db_path.display(), e))?;
        Ok(Self {
            conn,
            attached_schemas: Vec::new(),
        })
    }

    /// Open `ref_honor.sqlite` and attach `ref_base.sqlite` as the first fallback.
    pub fn open_honor_with_base(honor_db_path: &Path, base_db_path: &Path) -> Result<Self, String> {
        let mut db = Self::open(honor_db_path)?;
        attach_readonly(&db.conn, base_db_path, ALIAS_BASE)?;
        db.attached_schemas.push(ALIAS_BASE.to_string());
        Ok(db)
    }

    /// Borrow the underlying rusqlite connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Attached schema aliases in lookup order.
    pub fn attached_schemas(&self) -> &[String] {
        &self.attached_schemas
    }
}

/// A single FK violation found during cross-DB validation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FkViolation {
    /// Table containing the dangling reference.
    pub table: String,
    /// Rowid of the row with the violation.
    pub rowid: i64,
    /// FK column that holds the dangling value.
    pub from_column: String,
    /// The actual dangling value.
    pub value: String,
    /// The target table the FK should reference.
    pub target_table: String,
    /// The target column the FK should reference.
    pub target_column: String,
}

/// Summary returned by `validate_cross_db_fks`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CrossDbFkReport {
    /// Violations that are completely unresolved — not found in main or any attached DB.
    pub unresolved: Vec<FkViolation>,
    /// Count of references that were missing from main but resolved in an attached DB.
    pub cross_resolved: usize,
    /// Total FK references checked.
    pub total_checked: usize,
    /// Attached schema aliases that were searched.
    pub attached_schemas: Vec<String>,
}

/// Attach a database file as a read-only schema alias.
///
/// Uses a plain path for compatibility — SQLITE_OPEN_URI is not guaranteed
/// on the parent connection.  The attached DB is typically only read from.
pub fn attach_readonly(conn: &Connection, db_path: &Path, alias: &str) -> Result<(), String> {
    // Validate alias is alphanumeric to prevent SQL injection
    if !alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("Invalid alias: {alias}"));
    }
    let path_str = db_path.to_string_lossy().replace('\\', "/");
    let sql = format!("ATTACH DATABASE '{path_str}' AS \"{alias}\"");
    conn.execute_batch(&sql)
        .map_err(|e| format!("ATTACH {} ({}): {}", alias, db_path.display(), e))
}

/// Detach a previously attached database.
pub fn detach(conn: &Connection, alias: &str) -> Result<(), String> {
    if !alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!("Invalid alias: {alias}"));
    }
    let sql = format!("DETACH DATABASE \"{alias}\"");
    conn.execute_batch(&sql)
        .map_err(|e| format!("DETACH {alias}: {e}"))
}

/// List all currently attached database aliases (excluding "main" and "temp").
pub fn list_attached(conn: &Connection) -> Result<Vec<(String, String)>, String> {
    let mut stmt = conn
        .prepare("PRAGMA database_list")
        .map_err(|e| format!("database_list: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .map_err(|e| format!("database_list query: {e}"))?
        .filter_map(|r| r.ok())
        .filter(|(name, _)| name != "main" && name != "temp")
        .collect();
    Ok(rows)
}

/// Validate FK references in `main`, resolving against attached schemas.
///
/// For each table in `main` with FK constraints, this function:
///   1. Finds rows where the FK column has a non-NULL value
///   2. Checks if that value exists as a PK in `main.<target_table>`
///   3. If not, checks each attached schema's `<alias>.<target_table>`
///   4. If found in an attached schema → counted as `cross_resolved`
///   5. If not found anywhere → added to `unresolved`
///
/// This replaces `PRAGMA foreign_key_check` for cross-DB scenarios.
pub fn validate_cross_db_fks(
    conn: &Connection,
    attached_aliases: &[&str],
) -> Result<CrossDbFkReport, String> {
    let mut report = CrossDbFkReport {
        unresolved: Vec::new(),
        cross_resolved: 0,
        total_checked: 0,
        attached_schemas: attached_aliases.iter().map(|s| s.to_string()).collect(),
    };

    // Get all user tables in main
    let table_names: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM main.sqlite_master WHERE type = 'table' AND name NOT LIKE '\\_%' ESCAPE '\\'")
            .map_err(|e| format!("List tables: {e}"))?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| format!("Query tables: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    for table_name in &table_names {
        // Get FK constraints for this table
        let fk_sql = format!("PRAGMA main.foreign_key_list(\"{table_name}\")");
        let fks: Vec<(String, String, String)> = {
            let mut stmt = conn
                .prepare(&fk_sql)
                .map_err(|e| format!("FK list {table_name}: {e}"))?;
            // foreign_key_list columns: id, seq, table, from, to, on_update, on_delete, match
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(2)?, // target table
                        row.get::<_, String>(3)?, // source column
                        row.get::<_, String>(4)?, // target column
                    ))
                })
                .map_err(|e| format!("FK list query {table_name}: {e}"))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };

        if fks.is_empty() {
            continue;
        }

        for (target_table, from_col, to_col) in &fks {
            // Check if the target table exists in main — if not, can't validate
            let target_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM main.sqlite_master WHERE type='table' AND name=?1",
                    params![target_table],
                    |row| row.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(false);

            if !target_exists {
                continue;
            }

            // Find rows with non-NULL FK values that don't exist in main
            let check_sql = format!(
                "SELECT rowid, \"{from_col}\" FROM main.\"{table_name}\" \
                 WHERE \"{from_col}\" IS NOT NULL \
                 AND \"{from_col}\" NOT IN (SELECT \"{to_col}\" FROM main.\"{target_table}\")",
            );

            let dangling: Vec<(i64, String)> = match conn.prepare(&check_sql) {
                Ok(mut stmt) => stmt
                    .query_map([], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|e| format!("FK check {table_name}.{from_col}: {e}"))?
                    .filter_map(|r| r.ok())
                    .collect(),
                Err(_) => continue, // Column type mismatch etc — skip
            };

            report.total_checked += 1;

            if dangling.is_empty() {
                continue;
            }

            // For each dangling reference, check attached schemas
            for (rowid, value) in &dangling {
                let mut resolved = false;

                for alias in attached_aliases {
                    // Check if target table exists in this attached schema
                    let exists_sql = format!(
                        "SELECT COUNT(*) FROM \"{alias}\".sqlite_master WHERE type='table' AND name=?1"
                    );
                    let has_table = conn
                        .query_row(&exists_sql, params![target_table], |row| {
                            row.get::<_, i64>(0)
                        })
                        .map(|n| n > 0)
                        .unwrap_or(false);

                    if !has_table {
                        continue;
                    }

                    let lookup_sql = format!(
                        "SELECT 1 FROM \"{alias}\".\"{target_table}\" WHERE \"{to_col}\" = ?1 LIMIT 1"
                    );
                    let found = conn
                        .query_row(&lookup_sql, params![value], |_| Ok(()))
                        .is_ok();

                    if found {
                        resolved = true;
                        report.cross_resolved += 1;
                        break;
                    }
                }

                if !resolved {
                    report.unresolved.push(FkViolation {
                        table: table_name.clone(),
                        rowid: *rowid,
                        from_column: from_col.clone(),
                        value: value.clone(),
                        target_table: target_table.clone(),
                        target_column: to_col.clone(),
                    });
                }
            }
        }
    }

    Ok(report)
}

/// Generate a SQL expression that looks up a PK across main + attached schemas.
///
/// Returns e.g.:
/// ```sql
/// SELECT "UUID" FROM main."GameObjects" WHERE "UUID" = ?1
/// UNION ALL
/// SELECT "UUID" FROM base."GameObjects" WHERE "UUID" = ?1
/// LIMIT 1
/// ```
///
/// Useful for autocomplete, dropdown population, and dependency resolution.
pub fn cross_db_exists_sql(table: &str, pk_column: &str, attached_aliases: &[&str]) -> String {
    let mut parts = vec![format!(
        "SELECT \"{}\" FROM main.\"{}\" WHERE \"{}\" = ?1",
        pk_column, table, pk_column
    )];
    for alias in attached_aliases {
        parts.push(format!(
            "SELECT \"{pk_column}\" FROM \"{alias}\".\"{table}\" WHERE \"{pk_column}\" = ?1"
        ));
    }
    format!("{} LIMIT 1", parts.join(" UNION ALL "))
}

/// Generate a SELECT that unions all rows from a table across main + attached schemas.
///
/// Returns e.g.:
/// ```sql
/// SELECT *, 'main' AS _db FROM main."GameObjects"
/// UNION ALL
/// SELECT *, 'base' AS _db FROM base."GameObjects"
/// ```
///
/// The `_db` column identifies which database each row came from.
/// Callers can add WHERE / ORDER BY / LIMIT on top.
pub fn cross_db_union_sql(table: &str, attached_aliases: &[&str]) -> String {
    let mut parts = vec![format!("SELECT *, 'main' AS _db FROM main.\"{}\"", table)];
    for alias in attached_aliases {
        parts.push(format!(
            "SELECT *, '{alias}' AS _db FROM \"{alias}\".\"{table}\""
        ));
    }
    parts.join(" UNION ALL ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cross_db_exists_sql() {
        let sql = cross_db_exists_sql("GameObjects", "UUID", &["base", "mods"]);
        assert!(sql.contains("main.\"GameObjects\""));
        assert!(sql.contains("\"base\".\"GameObjects\""));
        assert!(sql.contains("\"mods\".\"GameObjects\""));
        assert!(sql.ends_with("LIMIT 1"));
    }

    #[test]
    fn test_cross_db_union_sql() {
        let sql = cross_db_union_sql("PassiveData", &["base"]);
        assert!(sql.contains("'main' AS _db FROM main.\"PassiveData\""));
        assert!(sql.contains("'base' AS _db FROM \"base\".\"PassiveData\""));
    }

    #[test]
    fn test_attach_validates_alias() {
        let conn = Connection::open_in_memory().unwrap();
        let result = attach_readonly(&conn, Path::new("/tmp/test.db"), "bad;alias");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid alias"));
    }

    #[test]
    fn ref_db_conn_opens_honor_with_base_attached() {
        let tmp = tempdir().unwrap();
        let honor_path = tmp.path().join("ref_honor.sqlite");
        let base_path = tmp.path().join("ref_base.sqlite");

        Connection::open(&honor_path).unwrap().close().unwrap();
        Connection::open(&base_path).unwrap().close().unwrap();

        let db = RefDbConn::open_honor_with_base(&honor_path, &base_path).unwrap();
        let attached = list_attached(db.conn()).unwrap();

        assert_eq!(db.attached_schemas(), &[ALIAS_BASE.to_string()]);
        assert!(attached.iter().any(|(alias, _)| alias == ALIAS_BASE));
    }
}
