//! DB-backed query functions for vanilla data lookups.
//!
//! These replace the VanillaCache YAML-reading path.  Each function opens
//! `ref_base.sqlite` (or another schema DB), runs a SQL query, and returns the
//! same types the frontend already expects.

use crate::models::Section;
use crate::schema::infer::{AttrSchema, ChildSchema, NodeSchema};
use crate::{
    ChildTableInfo, ListItemsInfo, LocaEntry, SectionInfo, StatEntryInfo, VanillaEntryInfo,
};
use rusqlite::Connection;
use std::path::Path;

/// SQL ORDER BY fragment that prefers root tables (parent_tables IS NULL),
/// then direct children of root, then deeper children, then highest row_count.
/// Use this everywhere we pick "the main table" for a region.
const ROOT_TABLE_ORDER: &str = "\
    ORDER BY \
      CASE \
        WHEN parent_tables IS NULL THEN 0 \
        WHEN parent_tables LIKE '%\\_\\_root' ESCAPE '\\' THEN 1 \
        ELSE 2 \
      END, \
      row_count DESC \
    LIMIT 1";

/// Query vanilla LSX entries from the reference DB for a given section.
///
/// Maps the `Section` enum to a DB table via `_table_meta`, fetches rows, and
/// applies the same display-name formatting as the legacy VanillaCache path.
pub fn query_vanilla_entries(
    db_path: &Path,
    section: &Section,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<VanillaEntryInfo>, String> {
    let conn = open_ro(db_path)?;

    let region_id = section.region_id();

    // Find the table name(s) for this section via _table_meta
    let table_name: String = conn
        .query_row(
            &format!(
                "SELECT table_name FROM _table_meta \
                 WHERE region_id = ?1 AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
            ),
            [region_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("No table for region '{region_id}': {e}"))?;

    validate_table_name(&table_name)?;

    // Discover available columns
    let columns = table_columns(&conn, &table_name)?;

    // Build a generic SELECT with the columns we need
    let pk_col = columns
        .iter()
        .find(|c| c.name == "UUID")
        .or_else(|| columns.iter().find(|c| c.name == "MapKey"))
        .or_else(|| columns.iter().find(|c| c.name == "_row_id"))
        .map(|c| c.name.as_str())
        .unwrap_or("rowid");

    let has = |name: &str| columns.iter().any(|c| c.name == name);

    // Fetch the node_id from _table_meta
    let node_id: String = conn
        .query_row(
            "SELECT COALESCE(node_id, '') FROM _table_meta WHERE table_name = ?1",
            [&table_name],
            |row| row.get(0),
        )
        .unwrap_or_default();

    // Rowid-strategy columns are INTEGER; cast to TEXT for uniform handling
    let pk_is_integer = pk_col == "_row_id" || pk_col == "rowid";
    let pk_select_expr = if pk_is_integer {
        format!("CAST(\"{pk_col}\" AS TEXT)")
    } else {
        format!("\"{pk_col}\"")
    };

    // Build the query — select PK + display-relevant columns
    let mut select_cols = vec![pk_select_expr];
    for col in &[
        "Name",
        "Level",
        "Comment",
        "DisplayName",
        "ParentGuid",
        "Color",
    ] {
        if has(col) {
            select_cols.push(format!("\"{col}\""));
        }
    }

    let sql = format!(
        "SELECT {} FROM \"{}\" ORDER BY \"{}\" LIMIT ?1 OFFSET ?2",
        select_cols.join(", "),
        table_name,
        pk_col,
    );

    let lim = limit.unwrap_or(0) as i64;
    let off = offset.unwrap_or(0) as i64;
    let effective_limit = if lim == 0 { -1i64 } else { lim }; // -1 = unlimited in SQLite

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let rows = stmt
        .query_map([effective_limit, off], |row| {
            let uuid: String = row.get(0)?;

            // Extract optional columns by name
            let get_opt = |name: &str| -> Option<String> {
                let idx = select_cols
                    .iter()
                    .position(|c| c == &format!("\"{name}\""))?;
                row.get::<_, Option<String>>(idx).ok().flatten()
            };

            let name_val = get_opt("Name").unwrap_or_default();
            let level_val = get_opt("Level");
            let comment_val = get_opt("Comment");
            let display_name_val = get_opt("DisplayName");
            let parent_guid_val = get_opt("ParentGuid");
            let color_val = get_opt("Color");

            Ok((
                uuid,
                name_val,
                level_val,
                comment_val,
                display_name_val,
                parent_guid_val,
                color_val,
            ))
        })
        .map_err(|e| format!("Query: {e}"))?;

    let mut entries = Vec::new();
    for row in rows {
        let (uuid, name, level, comment, display_name_attr, parent_guid, color) =
            row.map_err(|e| format!("Row: {e}"))?;

        let display_name = format_display_name(
            section,
            &name,
            level.as_deref(),
            comment.as_deref(),
            display_name_attr.as_deref(),
        );

        entries.push(VanillaEntryInfo {
            uuid,
            display_name,
            node_id: node_id.clone(),
            color,
            parent_guid: match section {
                Section::Races => parent_guid.filter(|v| !v.is_empty()),
                _ => None,
            },
            text_handle: None,
        });
    }

    Ok(entries)
}

/// Check whether the reference DB has populated data for at least one LSX
/// section. Returns `false` if the DB is missing, empty, or has no LSX tables.
pub fn db_has_vanilla_data(db_path: &Path) -> bool {
    if !db_path.is_file() {
        return false;
    }
    let Ok(conn) = open_ro(db_path) else {
        return false;
    };
    conn.query_row(
        "SELECT COUNT(*) FROM _table_meta WHERE source_type = 'lsx' AND row_count > 0",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

// ── Helpers ─────────────────────────────────────────────────────────────────

struct ColInfo {
    name: String,
}

fn table_columns(conn: &Connection, table_name: &str) -> Result<Vec<ColInfo>, String> {
    validate_table_name(table_name)?;
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table_name}\")"))
        .map_err(|e| format!("table_info: {e}"))?;
    let cols = stmt
        .query_map([], |row| {
            Ok(ColInfo {
                name: row.get::<_, String>(1)?,
            })
        })
        .map_err(|e| format!("table_info query: {e}"))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(cols)
}

/// Apply section-specific display name formatting (mirrors lib.rs logic).
fn format_display_name(
    section: &Section,
    name: &str,
    level: Option<&str>,
    comment: Option<&str>,
    display_name_attr: Option<&str>,
) -> String {
    match section {
        Section::Progressions => {
            let lvl = level.unwrap_or("?");
            format!("{name} Lv{lvl}")
        }
        Section::Lists => comment.unwrap_or("").to_string(),
        Section::Backgrounds | Section::Origins => display_name_attr
            .filter(|v| !v.is_empty())
            .or(Some(name))
            .unwrap_or("")
            .to_string(),
        _ => name.to_string(),
    }
}

/// Open a read-only connection to the reference DB.
fn open_ro(db_path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("Open ref DB: {e}"))
}

/// Validate that a table name contains only safe characters for SQL interpolation.
///
/// All valid table names in the reference DB consist solely of ASCII letters,
/// digits, and underscores (e.g. `lsx__Races__Race`, `stats__SpellData`,
/// `valuelist_entries`).  This rejects any name containing characters that
/// could enable SQL injection.
fn validate_table_name(table_name: &str) -> Result<(), String> {
    if table_name.is_empty() {
        return Err("Table name must not be empty".to_string());
    }
    if !table_name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(format!(
            "Invalid table name '{table_name}': only ASCII alphanumeric characters and underscores are allowed"
        ));
    }
    Ok(())
}

// ── Stats queries ───────────────────────────────────────────────────────────

/// Check whether the reference DB has populated stats data.
pub fn db_has_stats_data(db_path: &Path) -> bool {
    if !db_path.is_file() {
        return false;
    }
    let Ok(conn) = open_ro(db_path) else {
        return false;
    };
    conn.query_row(
        "SELECT COUNT(*) FROM _table_meta WHERE source_type = 'stats' AND row_count > 0",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Check whether the reference DB has populated localization data.
pub fn db_has_loca_data(db_path: &Path) -> bool {
    if !db_path.is_file() {
        return false;
    }
    let Ok(conn) = open_ro(db_path) else {
        return false;
    };
    conn.query_row(
        "SELECT COUNT(*) FROM _table_meta WHERE source_type = 'loca' AND row_count > 0",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Query stat entries from `stats__*` tables.
///
/// If `entry_type` is empty, queries ALL stats tables.  Otherwise queries only
/// `stats__<entry_type>`.  Returns `_entry_name` as name and the type derived
/// from the table name.
pub fn query_stat_entries(db_path: &Path, entry_type: &str) -> Result<Vec<StatEntryInfo>, String> {
    let conn = open_ro(db_path)?;

    // Collect target tables
    let tables: Vec<(String, String)> = if entry_type.is_empty() {
        let mut stmt = conn
            .prepare(
                "SELECT table_name FROM _table_meta \
                 WHERE source_type = 'stats' AND row_count > 0 \
                 ORDER BY table_name",
            )
            .map_err(|e| format!("Prepare: {e}"))?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| {
                let tn: String = row.get(0)?;
                let etype = tn.strip_prefix("stats__").unwrap_or(&tn).to_string();
                Ok((tn, etype))
            })
            .map_err(|e| format!("Query: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    } else {
        let tn = format!("stats__{entry_type}");
        vec![(tn, entry_type.to_string())]
    };

    for (tn, _) in &tables {
        validate_table_name(tn)?;
    }

    let mut results = Vec::new();
    for (table_name, etype) in &tables {
        let sql = format!("SELECT \"_entry_name\" FROM \"{table_name}\" ORDER BY \"_entry_name\"");
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue, // table may not exist
        };
        let rows = stmt
            .query_map([], |row| {
                let name: String = row.get(0)?;
                Ok(StatEntryInfo {
                    name,
                    entry_type: etype.clone(),
                })
            })
            .map_err(|e| format!("Query {table_name}: {e}"))?;
        for entry in rows.flatten() {
            results.push(entry);
        }
    }

    results.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(results)
}

/// Query unique stat field names (column names) from `stats__*` tables.
///
/// Internal columns (`_entry_name`, `_file_id`, `_type`, `_using`) are excluded.
pub fn query_stat_field_names(db_path: &Path, entry_type: &str) -> Result<Vec<String>, String> {
    let conn = open_ro(db_path)?;

    let tables: Vec<String> = if entry_type.is_empty() {
        let mut stmt = conn
            .prepare(
                "SELECT table_name FROM _table_meta \
                 WHERE source_type = 'stats' AND row_count > 0",
            )
            .map_err(|e| format!("Prepare: {e}"))?;
        let rows: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| format!("Query: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    } else {
        vec![format!("stats__{}", entry_type)]
    };

    for tn in &tables {
        validate_table_name(tn)?;
    }

    let internal = ["_entry_name", "_file_id", "_type", "_using"];
    let mut names = std::collections::BTreeSet::new();
    for tn in &tables {
        if let Ok(cols) = table_columns(&conn, tn) {
            for c in cols {
                if !internal.contains(&c.name.as_str()) && !c.name.ends_with("_version") {
                    names.insert(c.name);
                }
            }
        }
    }

    Ok(names.into_iter().collect())
}

pub fn query_stat_entry_fields(
    db_path: &Path,
    entry_name: &str,
    entry_type: &str,
) -> Result<std::collections::HashMap<String, String>, String> {
    let conn = open_ro(db_path)?;
    let table_name = format!("stats__{entry_type}");
    validate_table_name(&table_name)?;

    let columns = table_columns(&conn, &table_name)?;
    if columns.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let select_expr = columns
        .iter()
        .map(|c| format!("\"{}\"", c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let sql =
        format!("SELECT {select_expr} FROM \"{table_name}\" WHERE \"_entry_name\" = ?1 LIMIT 1");

    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(_) => return Ok(std::collections::HashMap::new()),
    };

    let result = stmt.query_row([entry_name], |row| {
        let mut fields = std::collections::HashMap::new();

        for (index, column) in columns.iter().enumerate() {
            if matches!(
                column.name.as_str(),
                "_entry_name" | "_file_id" | "_type" | "_using"
            ) || column.name.ends_with("_version")
            {
                continue;
            }

            let value = match row.get_ref(index)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                rusqlite::types::ValueRef::Text(s) => {
                    Some(std::str::from_utf8(s).unwrap_or("").to_string())
                }
                rusqlite::types::ValueRef::Blob(_) => None,
            };

            if let Some(value) = value {
                fields.insert(column.name.clone(), value);
            }
        }

        Ok(fields)
    });

    match result {
        Ok(fields) => Ok(fields),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(std::collections::HashMap::new()),
        Err(e) => Err(format!("Query {table_name}: {e}")),
    }
}

// ── List items query ────────────────────────────────────────────────────────

/// Query list items by UUID from the Lists table.
pub fn query_list_items(db_path: &Path, uuids: &[String]) -> Result<Vec<ListItemsInfo>, String> {
    if uuids.is_empty() {
        return Ok(Vec::new());
    }

    let conn = open_ro(db_path)?;

    // Find the Lists table
    let table_name: String = conn
        .query_row(
            &format!(
                "SELECT table_name FROM _table_meta \
                 WHERE region_id = 'Lists' AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("No Lists table: {e}"))?;

    validate_table_name(&table_name)?;

    let node_id: String = conn
        .query_row(
            "SELECT COALESCE(node_id, '') FROM _table_meta WHERE table_name = ?1",
            [&table_name],
            |row| row.get(0),
        )
        .unwrap_or_default();

    let columns = table_columns(&conn, &table_name)?;
    let has = |name: &str| columns.iter().any(|c| c.name == name);

    let item_fields: Vec<&str> = ["Spells", "Passives", "Skills", "Abilities", "Equipment"]
        .iter()
        .copied()
        .filter(|f| has(f))
        .collect();

    // Build column list
    let mut select_cols = vec!["\"UUID\"".to_string()];
    if has("Comment") {
        select_cols.push("\"Comment\"".to_string());
    }
    for f in &item_fields {
        select_cols.push(format!("\"{f}\""));
    }

    // Build WHERE IN clause with positional params
    let placeholders: Vec<String> = (1..=uuids.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT {} FROM \"{}\" WHERE \"UUID\" IN ({})",
        select_cols.join(", "),
        table_name,
        placeholders.join(", "),
    );

    let params: Vec<&dyn rusqlite::types::ToSql> = uuids
        .iter()
        .map(|u| u as &dyn rusqlite::types::ToSql)
        .collect();

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let rows = stmt
        .query_map(params.as_slice(), |row| {
            let uuid: String = row.get(0)?;

            let get_opt = |col_name: &str| -> Option<String> {
                let idx = select_cols
                    .iter()
                    .position(|c| c == &format!("\"{col_name}\""))?;
                row.get::<_, Option<String>>(idx).ok().flatten()
            };

            let comment = get_opt("Comment").unwrap_or_default();

            // Find the first item field with data
            let mut items = Vec::new();
            let mut item_key = String::new();
            for f in &item_fields {
                if let Some(val) = get_opt(f) {
                    if !val.is_empty() {
                        items = val
                            .split(';')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        item_key = f.to_string();
                        break;
                    }
                }
            }

            Ok(ListItemsInfo {
                uuid,
                node_id: node_id.clone(),
                display_name: comment,
                items,
                item_key,
            })
        })
        .map_err(|e| format!("Query: {e}"))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| format!("Row: {e}"))?);
    }
    Ok(results)
}

/// Query ActionResource definitions and groups for stats cost field editing.
pub fn query_cost_resources(db_path: &Path) -> Result<Vec<crate::CostResourceInfo>, String> {
    let conn = open_ro(db_path)?;
    let mut results = Vec::new();

    let mut push_region =
        |region_id: &str, kind: &str, include_max_level: bool| -> Result<(), String> {
            let table_name: String = match conn.query_row(
                &format!(
                    "SELECT table_name FROM _table_meta \
                 WHERE region_id = ?1 AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
                ),
                [region_id],
                |row| row.get(0),
            ) {
                Ok(tn) => tn,
                Err(_) => return Ok(()),
            };

            validate_table_name(&table_name)?;
            let columns = table_columns(&conn, &table_name)?;
            let has = |name: &str| columns.iter().any(|c| c.name == name);
            if !has("Name") {
                return Ok(());
            }

            let mut select_cols = vec!["\"Name\"".to_string()];
            if include_max_level && has("MaxLevel") {
                select_cols.push("\"MaxLevel\"".to_string());
            }
            if has("DisplayName") {
                select_cols.push("\"DisplayName\"".to_string());
            }
            if has("Comment") {
                select_cols.push("\"Comment\"".to_string());
            }

            let sql = format!(
                "SELECT {} FROM \"{}\" ORDER BY \"Name\"",
                select_cols.join(", "),
                table_name,
            );

            let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
            let rows = stmt
                .query_map([], |row| {
                    let name: String = row.get(0)?;

                    let get_opt = |col_name: &str| -> Option<String> {
                        let idx = select_cols
                            .iter()
                            .position(|c| c == &format!("\"{col_name}\""))?;
                        row.get::<_, Option<String>>(idx).ok().flatten()
                    };

                    let max_level = get_opt("MaxLevel")
                        .and_then(|v| v.parse::<i32>().ok())
                        .unwrap_or(0);
                    let display_name = get_opt("DisplayName")
                        .filter(|v| !v.is_empty())
                        .or_else(|| get_opt("Comment").filter(|v| !v.is_empty()))
                        .unwrap_or_else(|| name.clone());

                    Ok(crate::CostResourceInfo {
                        name,
                        display_name,
                        max_level,
                        kind: kind.to_string(),
                    })
                })
                .map_err(|e| format!("Query: {e}"))?;

            for row in rows {
                results.push(row.map_err(|e| format!("Row: {e}"))?);
            }

            Ok(())
        };

    push_region("ActionResourceDefinitions", "resource", true)?;
    push_region("ActionResourceGroupDefinitions", "group", false)?;

    results.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| {
                a.display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase())
            })
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(results)
}

// ── Progression table UUIDs ─────────────────────────────────────────────────

/// Collect unique ProgressionTableUUIDs from Progressions, ClassDescriptions,
/// and Races tables.
pub fn query_progression_table_uuids(db_path: &Path) -> Result<Vec<VanillaEntryInfo>, String> {
    let conn = open_ro(db_path)?;

    let targets: &[(&str, &str, &str)] = &[
        ("Progressions", "TableUUID", ""),
        ("ClassDescriptions", "ProgressionTableUUID", ""),
        ("Races", "ProgressionTableUUID", " (Race)"),
    ];

    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    for &(region_id, attr_key, suffix) in targets {
        let table_name: String = match conn.query_row(
            &format!(
                "SELECT table_name FROM _table_meta \
                 WHERE region_id = ?1 AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
            ),
            [region_id],
            |row| row.get(0),
        ) {
            Ok(tn) => tn,
            Err(_) => continue,
        };

        validate_table_name(&table_name)?;

        let columns = table_columns(&conn, &table_name)?;
        let has = |name: &str| columns.iter().any(|c| c.name == name);
        if !has(attr_key) {
            continue;
        }

        let has_name = has("Name");
        let sql = if has_name {
            format!(
                "SELECT \"{attr_key}\", \"Name\" FROM \"{table_name}\" WHERE \"{attr_key}\" IS NOT NULL AND \"{attr_key}\" != ''"
            )
        } else {
            format!(
                "SELECT \"{attr_key}\" FROM \"{table_name}\" WHERE \"{attr_key}\" IS NOT NULL AND \"{attr_key}\" != ''"
            )
        };

        let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                let uuid: String = row.get(0)?;
                let name: String = if has_name {
                    row.get::<_, Option<String>>(1)?.unwrap_or_default()
                } else {
                    String::new()
                };
                Ok((uuid, name))
            })
            .map_err(|e| format!("Query: {e}"))?;

        for row in rows {
            let (uuid, name) = row.map_err(|e| format!("Row: {e}"))?;
            if !uuid.is_empty() && seen.insert(uuid.clone()) {
                let display_name = if name.is_empty() {
                    uuid.clone()
                } else if suffix.is_empty() {
                    name
                } else {
                    format!("{name}{suffix}")
                };
                results.push(VanillaEntryInfo {
                    uuid,
                    display_name,
                    node_id: "ProgressionTable".to_string(),
                    color: None,
                    parent_guid: None,
                    text_handle: None,
                });
            }
        }
    }

    results.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(results)
}

// ── Voice table UUIDs ───────────────────────────────────────────────────────

/// Collect unique VoiceTable UUIDs from the Voices table.
pub fn query_voice_table_uuids(db_path: &Path) -> Result<Vec<VanillaEntryInfo>, String> {
    let conn = open_ro(db_path)?;

    let table_name: String = conn
        .query_row(
            &format!(
                "SELECT table_name FROM _table_meta \
                 WHERE region_id = 'Voices' AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("No Voices table: {e}"))?;

    validate_table_name(&table_name)?;

    let columns = table_columns(&conn, &table_name)?;
    let has = |name: &str| columns.iter().any(|c| c.name == name);

    let has_table_uuid = has("TableUUID");
    let has_name = has("Name");
    let has_display_name = has("DisplayName");

    let mut select_cols = vec!["\"UUID\"".to_string()];
    if has_table_uuid {
        select_cols.push("\"TableUUID\"".to_string());
    }
    if has_name {
        select_cols.push("\"Name\"".to_string());
    }
    if has_display_name {
        select_cols.push("\"DisplayName\"".to_string());
    }

    let sql = format!("SELECT {} FROM \"{}\"", select_cols.join(", "), table_name,);

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    let rows = stmt
        .query_map([], |row| {
            let uuid: String = row.get(0)?;
            let get_opt = |col_name: &str| -> Option<String> {
                let idx = select_cols
                    .iter()
                    .position(|c| c == &format!("\"{col_name}\""))?;
                row.get::<_, Option<String>>(idx).ok().flatten()
            };

            let table_uuid = get_opt("TableUUID")
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| uuid.clone());
            let name = get_opt("Name")
                .or_else(|| get_opt("DisplayName"))
                .unwrap_or_default();

            Ok((table_uuid, name))
        })
        .map_err(|e| format!("Query: {e}"))?;

    for row in rows {
        let (table_uuid, name) = row.map_err(|e| format!("Row: {e}"))?;
        if !table_uuid.is_empty() && seen.insert(table_uuid.clone()) {
            results.push(VanillaEntryInfo {
                uuid: table_uuid,
                display_name: name,
                node_id: "VoiceTable".to_string(),
                color: None,
                parent_guid: None,
                text_handle: None,
            });
        }
    }

    results.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(results)
}

// ── Entries by folder / section ─────────────────────────────────────────────

/// Query entries for a given section (folder), including UIColor/Color for
/// display swatches.
pub fn query_entries_by_folder(
    db_path: &Path,
    section: &Section,
) -> Result<Vec<VanillaEntryInfo>, String> {
    let conn = open_ro(db_path)?;
    let region_id = section.region_id();

    let table_name: String = conn
        .query_row(
            &format!(
                "SELECT table_name FROM _table_meta \
                 WHERE region_id = ?1 AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
            ),
            [region_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("No table for region '{region_id}': {e}"))?;

    validate_table_name(&table_name)?;

    let node_id: String = conn
        .query_row(
            "SELECT COALESCE(node_id, '') FROM _table_meta WHERE table_name = ?1",
            [&table_name],
            |row| row.get(0),
        )
        .unwrap_or_default();

    let columns = table_columns(&conn, &table_name)?;
    let has = |name: &str| columns.iter().any(|c| c.name == name);

    let pk_col = columns
        .iter()
        .find(|c| c.name == "UUID")
        .or_else(|| columns.iter().find(|c| c.name == "MapKey"))
        .or_else(|| columns.iter().find(|c| c.name == "_row_id"))
        .map(|c| c.name.as_str())
        .unwrap_or("rowid");

    // Rowid-strategy columns are INTEGER; cast to TEXT for uniform handling
    let pk_is_integer = pk_col == "_row_id" || pk_col == "rowid";
    let pk_select_expr = if pk_is_integer {
        format!("CAST(\"{pk_col}\" AS TEXT)")
    } else {
        format!("\"{pk_col}\"")
    };

    let mut select_cols = vec![pk_select_expr];
    for col in &["Name", "DisplayName", "UIColor", "Color"] {
        if has(col) {
            select_cols.push(format!("\"{col}\""));
        }
    }

    let sql = format!(
        "SELECT {} FROM \"{}\" ORDER BY \"{}\"",
        select_cols.join(", "),
        table_name,
        pk_col,
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    let rows = stmt
        .query_map([], |row| {
            let uuid: String = row.get(0)?;
            let get_opt = |col_name: &str| -> Option<String> {
                let idx = select_cols
                    .iter()
                    .position(|c| c == &format!("\"{col_name}\""))?;
                row.get::<_, Option<String>>(idx).ok().flatten()
            };

            let name = get_opt("Name")
                .or_else(|| get_opt("DisplayName"))
                .unwrap_or_default();
            let color = get_opt("UIColor").or_else(|| get_opt("Color"));

            Ok((uuid, name, color))
        })
        .map_err(|e| format!("Query: {e}"))?;

    for row in rows {
        let (uuid, name, color_raw) = row.map_err(|e| format!("Row: {e}"))?;
        if !uuid.is_empty() && seen.insert(uuid.clone()) {
            let color = color_raw.and_then(|v| crate::parse_bgra_color(&v));
            results.push(VanillaEntryInfo {
                uuid,
                display_name: name,
                node_id: node_id.clone(),
                color,
                parent_guid: None,
                text_handle: None,
            });
        }
    }

    Ok(results)
}

// ── Localization map ────────────────────────────────────────────────────────

/// Query localization entries from `loca__english`.
pub fn query_localization_map(db_path: &Path) -> Result<Vec<LocaEntry>, String> {
    let conn = open_ro(db_path)?;

    // Find the loca table
    let table_name: String = conn
        .query_row(
            "SELECT table_name FROM _table_meta \
             WHERE source_type = 'loca' AND row_count > 0 \
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("No loca table: {e}"))?;

    validate_table_name(&table_name)?;

    let sql =
        format!("SELECT \"contentuid\", \"text\" FROM \"{table_name}\" ORDER BY \"contentuid\"",);

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok(LocaEntry {
                handle: row.get(0)?,
                text: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            })
        })
        .map_err(|e| format!("Query: {e}"))?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row.map_err(|e| format!("Row: {e}"))?);
    }
    Ok(entries)
}

// ── Selector IDs ────────────────────────────────────────────────────────────

/// Query unique SelectorId values from the Progressions table.
/// Returns (id, source) pairs where source is always "Vanilla".
pub fn query_selector_ids(db_path: &Path) -> Result<Vec<(String, String)>, String> {
    let conn = open_ro(db_path)?;

    let table_name: String = conn
        .query_row(
            &format!(
                "SELECT table_name FROM _table_meta \
                 WHERE region_id = 'Progressions' AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("No Progressions table: {e}"))?;

    validate_table_name(&table_name)?;

    let columns = table_columns(&conn, &table_name)?;
    if !columns.iter().any(|c| c.name == "SelectorId") {
        return Ok(Vec::new());
    }

    let sql = format!(
        "SELECT DISTINCT \"SelectorId\" FROM \"{table_name}\" \
         WHERE \"SelectorId\" IS NOT NULL AND \"SelectorId\" != '' \
         ORDER BY \"SelectorId\"",
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            Ok((id, "Vanilla".to_string()))
        })
        .map_err(|e| format!("Query: {e}"))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| format!("Row: {e}"))?);
    }
    Ok(results)
}

/// Check whether the DB has equipment data.
pub fn db_has_equipment_data(db_path: &Path) -> bool {
    let conn = match open_ro(db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    conn.query_row(
        "SELECT COUNT(*) FROM _table_meta WHERE table_name = 'stats__equipment' AND row_count > 0",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Query equipment names from the reference DB.
/// Query all icon MapKey names from IconUVList tables in the reference DB.
///
/// Iterates every `IconUVList` table discovered during schema build, collects
/// `MapKey` values, de-duplicates, and returns them sorted.  Used to populate
/// the `iconName:` combobox descriptor for the Icon field on stat entries.
pub fn query_icon_names(db_path: &Path) -> Result<Vec<String>, String> {
    let conn = open_ro(db_path)?;

    // Find all IconUVList tables (one per Icons_*.lsx file)
    let mut stmt = conn
        .prepare(
            "SELECT table_name FROM _table_meta \
             WHERE region_id = 'IconUVList' AND source_type = 'lsx' AND row_count > 0 \
             ORDER BY table_name",
        )
        .map_err(|e| format!("Prepare icon table lookup: {e}"))?;

    let table_names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Query icon tables: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    if table_names.is_empty() {
        return Ok(Vec::new());
    }

    for tn in &table_names {
        validate_table_name(tn)?;
    }

    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();

    for table_name in &table_names {
        let columns = table_columns(&conn, table_name)?;
        // IconUV tables use MapKey as primary key
        if !columns.iter().any(|c| c.name == "MapKey") {
            continue;
        }

        let sql = format!(
            "SELECT \"MapKey\" FROM \"{table_name}\" \
             WHERE \"MapKey\" IS NOT NULL AND \"MapKey\" != '' \
             ORDER BY \"MapKey\""
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("Prepare {table_name}: {e}"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("Query {table_name}: {e}"))?;

        for row in rows {
            let mk = row.map_err(|e| format!("Row: {e}"))?;
            if seen.insert(mk.clone()) {
                results.push(mk);
            }
        }
    }

    results.sort();
    Ok(results)
}

/// Per-icon atlas data: MapKey + atlas DDS path + UV coordinates (0..1 normalised).
#[derive(serde::Serialize)]
pub struct IconAtlasEntry {
    pub map_key: String,
    /// Path attr from TextureAtlasPath node, e.g. "Assets/Textures/Icons/Icons_Skills.dds"
    pub atlas_path: String,
    pub u1: f64,
    pub v1: f64,
    pub u2: f64,
    pub v2: f64,
}

/// Query atlas UV data for all icons in the reference DB.
///
/// Joins every `IconUVList` table (MapKey, U1, V1, U2, V2, _file_id) with the
/// `TextureAtlasInfo` table that carries the `Path` attribute, matched via the
/// shared `_file_id` column (both regions come from the same source `.lsx` file).
pub fn query_icon_atlas_data(db_path: &Path) -> Result<Vec<IconAtlasEntry>, String> {
    let conn = open_ro(db_path)?;

    // 1. Collect all IconUVList tables.
    let mut stmt = conn
        .prepare(
            "SELECT table_name FROM _table_meta \
             WHERE region_id = 'IconUVList' AND source_type = 'lsx' AND row_count > 0 \
             ORDER BY table_name",
        )
        .map_err(|e| format!("Prepare icon table lookup: {e}"))?;
    let uv_tables: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Query icon tables: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    if uv_tables.is_empty() {
        return Ok(Vec::new());
    }

    // 2. Find TextureAtlasInfo tables that have both _file_id and Path columns
    //    (these are the TextureAtlasPath node tables).
    let mut stmt2 = conn
        .prepare(
            "SELECT table_name FROM _table_meta \
             WHERE region_id = 'TextureAtlasInfo' AND source_type = 'lsx' AND row_count > 0 \
             ORDER BY table_name",
        )
        .map_err(|e| format!("Prepare atlas info table lookup: {e}"))?;
    let info_tables: Vec<String> = stmt2
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Query atlas info tables: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    // Build: _file_id → atlas_path
    let mut file_to_path: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for tn in &info_tables {
        if validate_table_name(tn).is_err() {
            continue;
        }
        let cols = table_columns(&conn, tn).unwrap_or_default();
        // Only tables that have both the _file_id tracking col and a Path column are useful.
        let has_file_id = cols.iter().any(|c| c.name == "_file_id");
        let has_path = cols.iter().any(|c| c.name == "Path");
        if !has_file_id || !has_path {
            continue;
        }
        let sql = format!(
            "SELECT _file_id, \"Path\" FROM \"{tn}\" \
             WHERE \"Path\" IS NOT NULL AND \"Path\" != '' AND _file_id IS NOT NULL"
        );
        if let Ok(mut s) = conn.prepare(&sql) {
            if let Ok(rows) = s.query_map([], |row| {
                let fid: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                Ok((fid, path))
            }) {
                for row in rows.flatten() {
                    file_to_path.entry(row.0).or_insert(row.1);
                }
            }
        }
    }

    if file_to_path.is_empty() {
        return Ok(Vec::new());
    }

    // 3. For each IconUVList table, collect MapKey + UV + atlas path.
    let mut seen = std::collections::HashSet::new();
    let mut results: Vec<IconAtlasEntry> = Vec::new();

    for table_name in &uv_tables {
        if validate_table_name(table_name).is_err() {
            continue;
        }
        let cols = match table_columns(&conn, table_name) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let has = |name: &str| cols.iter().any(|c| c.name == name);
        // Need all UV columns and _file_id to join with atlas path table.
        if !has("MapKey")
            || !has("U1")
            || !has("V1")
            || !has("U2")
            || !has("V2")
            || !has("_file_id")
        {
            continue;
        }

        let sql = format!(
            "SELECT \"MapKey\", \"U1\", \"V1\", \"U2\", \"V2\", _file_id \
             FROM \"{table_name}\" \
             WHERE \"MapKey\" IS NOT NULL AND \"MapKey\" != ''"
        );
        let mut stmt = match conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let rows = match stmt.query_map([], |row| {
            let map_key: String = row.get(0)?;
            // UV values are stored as REAL; fall back to 0.0 on null/type mismatch.
            let u1: f64 = row.get::<_, f64>(1).unwrap_or(0.0);
            let v1: f64 = row.get::<_, f64>(2).unwrap_or(0.0);
            let u2: f64 = row.get::<_, f64>(3).unwrap_or(0.0);
            let v2: f64 = row.get::<_, f64>(4).unwrap_or(0.0);
            let file_id: i64 = row.get::<_, i64>(5).unwrap_or(0);
            Ok((map_key, u1, v1, u2, v2, file_id))
        }) {
            Ok(r) => r,
            Err(_) => continue,
        };

        for row in rows.flatten() {
            let (map_key, u1, v1, u2, v2, file_id) = row;
            if map_key.is_empty() || !seen.insert(map_key.clone()) {
                continue;
            }
            let atlas_path = match file_to_path.get(&file_id) {
                Some(p) => p.clone(),
                None => continue, // No atlas path found for this file — skip.
            };
            results.push(IconAtlasEntry {
                map_key,
                atlas_path,
                u1,
                v1,
                u2,
                v2,
            });
        }
    }

    results.sort_by(|a, b| a.map_key.cmp(&b.map_key));
    Ok(results)
}

pub fn query_equipment_names(db_path: &Path) -> Result<Vec<String>, String> {
    let conn = open_ro(db_path)?;

    let mut stmt = conn
        .prepare("SELECT \"_entry_name\" FROM \"stats__equipment\" ORDER BY \"_entry_name\"")
        .map_err(|e| format!("Prepare equipment query: {e}"))?;

    let rows: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| format!("Query equipment: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(rows)
}

/// Check whether the DB has value list data.
pub fn db_has_valuelist_data(db_path: &Path) -> bool {
    let conn = match open_ro(db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    conn.query_row(
        "SELECT COUNT(*) FROM _table_meta WHERE table_name = 'valuelist_entries' AND row_count > 0",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Query value lists from the reference DB.
///
/// If `list_key` is non-empty, only return the matching list.
/// Returns `Vec<(key, values)>` matching the `ValueListInfo` shape.
pub fn query_value_lists(
    db_path: &Path,
    list_key: &str,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let conn = open_ro(db_path)?;

    let (sql, params_vec): (String, Vec<String>) = if list_key.is_empty() {
        (
            "SELECT list_key, value_name FROM valuelist_entries ORDER BY list_key, value_index"
                .to_string(),
            Vec::new(),
        )
    } else {
        (
            "SELECT list_key, value_name FROM valuelist_entries WHERE list_key = ?1 ORDER BY value_index"
                .to_string(),
            vec![list_key.to_string()],
        )
    };

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();

    let rows: Vec<(String, String)> = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| format!("Query: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    // Group by list_key, preserving order
    let mut result: Vec<(String, Vec<String>)> = Vec::new();
    let mut current_key: Option<String> = None;
    for (key, value) in rows {
        if current_key.as_ref() != Some(&key) {
            result.push((key.clone(), Vec::new()));
            current_key = Some(key);
        }
        if let Some(last) = result.last_mut() {
            last.1.push(value);
        }
    }

    Ok(result)
}

// ── Dynamic section discovery ───────────────────────────────────────────────

/// Query all populated sections from _table_meta, including junction-derived
/// parent→child relationships. This drives the dynamic section list on the
/// frontend — no hardcoded Section enum needed.
pub fn query_available_sections(db_path: &Path) -> Result<Vec<SectionInfo>, String> {
    let conn = open_ro(db_path)?;

    // 1. Fetch all non-junction tables with row data
    let mut stmt = conn
        .prepare(
            "SELECT table_name, source_type, region_id, node_id, row_count \
             FROM _table_meta \
             WHERE source_type != 'junction' AND row_count > 0 \
             ORDER BY source_type, region_id, table_name",
        )
        .map_err(|e| format!("Prepare sections: {e}"))?;

    struct RawRow {
        table_name: String,
        source_type: String,
        region_id: String,
        node_id: String,
        row_count: i64,
    }

    let raw_rows: Vec<RawRow> = stmt
        .query_map([], |row| {
            Ok(RawRow {
                table_name: row.get(0)?,
                source_type: row.get(1)?,
                region_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                node_id: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row_count: row.get(4)?,
            })
        })
        .map_err(|e| format!("Query sections: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    // 2. Fetch junction table info for parent→child relationships
    let mut jct_stmt = conn
        .prepare(
            "SELECT table_name, parent_tables, row_count \
             FROM _table_meta \
             WHERE source_type = 'junction' AND row_count > 0",
        )
        .map_err(|e| format!("Prepare junctions: {e}"))?;

    struct JctRow {
        junction_table: String,
        parent_table: String,
        child_table: String,
        _row_count: i64,
    }

    let jct_rows: Vec<JctRow> = jct_stmt
        .query_map([], |row| {
            let table_name: String = row.get(0)?;
            let parent_tables: String = row.get::<_, Option<String>>(1)?.unwrap_or_default();
            let row_count: i64 = row.get(2)?;
            Ok((table_name, parent_tables, row_count))
        })
        .map_err(|e| format!("Query junctions: {e}"))?
        .filter_map(|r| r.ok())
        .filter_map(|(jt_name, parent_tables, rc)| {
            // Junction parent_tables format: "parent_table,child_table"
            let parts: Vec<&str> = parent_tables.splitn(2, ',').collect();
            if parts.len() == 2 {
                Some(JctRow {
                    junction_table: jt_name,
                    parent_table: parts[0].to_string(),
                    child_table: parts[1].to_string(),
                    _row_count: rc,
                })
            } else {
                None
            }
        })
        .collect();

    // 3. Build a lookup: table_name → (node_id, row_count)
    let table_info: std::collections::HashMap<&str, (&str, i64)> = raw_rows
        .iter()
        .map(|r| (r.table_name.as_str(), (r.node_id.as_str(), r.row_count)))
        .collect();

    // 4. Group junctions by parent table
    let mut children_by_parent: std::collections::HashMap<&str, Vec<ChildTableInfo>> =
        std::collections::HashMap::new();
    for jct in &jct_rows {
        let (child_node_id, child_row_count) = table_info
            .get(jct.child_table.as_str())
            .copied()
            .unwrap_or(("", 0));

        children_by_parent
            .entry(jct.parent_table.as_str())
            .or_default()
            .push(ChildTableInfo {
                junction_table: jct.junction_table.clone(),
                child_table: jct.child_table.clone(),
                child_node_id: child_node_id.to_string(),
                child_row_count: child_row_count as i32,
            });
    }

    // 5. Collect all child tables so we can exclude them from the top-level list
    let child_tables: std::collections::HashSet<&str> =
        jct_rows.iter().map(|j| j.child_table.as_str()).collect();

    // 6. Build SectionInfo entries: only top-level tables (not children)
    let mut sections = Vec::new();
    for raw in &raw_rows {
        // Skip tables that are children of another table — they'll appear
        // in the parent's `children` vec instead.
        if child_tables.contains(raw.table_name.as_str()) {
            continue;
        }

        // Determine PK column from PRAGMA
        let pk_col = detect_pk_column(&conn, &raw.table_name);

        let children = children_by_parent
            .remove(raw.table_name.as_str())
            .unwrap_or_default();

        sections.push(SectionInfo {
            region_id: raw.region_id.clone(),
            table_name: raw.table_name.clone(),
            node_id: raw.node_id.clone(),
            source_type: raw.source_type.clone(),
            row_count: raw.row_count as i32,
            pk_column: pk_col,
            children,
        });
    }

    Ok(sections)
}

/// Build a region_id → Section parse_name lookup from the Section enum.
/// For regions that map to a Section variant (e.g. "ConditionErrors" → "ErrorDescriptions"),
/// we return the variant name. For regions without a Section variant, returns None.
fn region_to_section_name() -> HashMap<&'static str, String> {
    let mut map = HashMap::new();
    for section in Section::all_ordered() {
        // region_id() → Section variant Debug name (= parse_name key = ts-rs string)
        for rid in section.region_ids() {
            map.insert(*rid, format!("{section:?}"));
        }
    }
    map
}

/// Query complete node schemas directly from DB metadata (`_table_meta` + `_column_types`).
///
/// This replaces the slow `infer_schemas` path that had to scan actual row data.
/// Returns one `NodeSchema` per (region, node_id) pair in the database, with
/// attribute types drawn from `_column_types` and child relationships from junction tables.
pub fn query_db_schemas(db_path: &Path) -> Result<Vec<NodeSchema>, String> {
    let conn = open_ro(db_path)?;
    let region_map = region_to_section_name();

    // 1. Fetch all root-level LSX tables (non-junction, with data)
    let mut meta_stmt = conn
        .prepare(
            "SELECT table_name, region_id, node_id, row_count \
             FROM _table_meta \
             WHERE source_type = 'lsx' AND row_count > 0 \
             ORDER BY region_id, row_count DESC",
        )
        .map_err(|e| format!("Prepare meta: {e}"))?;

    struct MetaRow {
        table_name: String,
        region_id: String,
        node_id: String,
        row_count: i64,
    }

    let meta_rows: Vec<MetaRow> = meta_stmt
        .query_map([], |row| {
            Ok(MetaRow {
                table_name: row.get(0)?,
                region_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                node_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row_count: row.get(3)?,
            })
        })
        .map_err(|e| format!("Query meta: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    // 2. Identify junction tables and build parent→children map
    let mut jct_stmt = conn
        .prepare(
            "SELECT table_name, region_id, node_id, row_count, parent_tables \
             FROM _table_meta \
             WHERE table_name LIKE '%\\_\\_to\\_\\_%' ESCAPE '\\' AND row_count > 0",
        )
        .map_err(|e| format!("Prepare junctions: {e}"))?;

    // junction parent_table → Vec<(group_id, child_node_id, child_row_count)>
    let mut children_by_parent: HashMap<String, Vec<(String, String, i64)>> = HashMap::new();

    let jct_rows = jct_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,         // table_name
                row.get::<_, Option<String>>(2)?, // node_id
                row.get::<_, i64>(3)?,            // row_count
            ))
        })
        .map_err(|e| format!("Query junctions: {e}"))?;

    for jct_row in jct_rows.flatten() {
        let (jt_name, child_node_id, child_rc) = jct_row;
        // Parse junction table name: lsx__Region__ParentNode__to__ChildNode
        if let Some(to_pos) = jt_name.find("__to__") {
            let parent_part = &jt_name[..to_pos]; // lsx__Region__ParentNode
            let child_part = &jt_name[to_pos + 6..]; // ChildNode
            let group_id = child_part.to_string();
            let child_nid = child_node_id.unwrap_or_else(|| child_part.to_string());
            children_by_parent
                .entry(parent_part.to_string())
                .or_default()
                .push((group_id, child_nid, child_rc));
        }
    }

    // 3. Identify child tables (tables that appear as children of junction tables)
    //    so we can exclude them from top-level results
    let child_tables: std::collections::HashSet<String> = children_by_parent
        .values()
        .flatten()
        .flat_map(|(_, child_nid, _)| {
            // Child tables follow the pattern: lsx__Region__ChildNodeId
            // We'll collect all table_names that are targets of junction relationships
            meta_rows
                .iter()
                .filter(|m| m.node_id == *child_nid)
                .map(|m| m.table_name.clone())
                .collect::<Vec<_>>()
        })
        .collect();

    // 4. Load all column types in bulk for efficiency
    let mut col_stmt = conn
        .prepare(
            "SELECT table_name, column_name, bg3_type \
             FROM _column_types \
             WHERE table_name LIKE 'lsx%' \
             ORDER BY table_name, column_name",
        )
        .map_err(|e| format!("Prepare columns: {e}"))?;

    let mut columns_by_table: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let col_rows = col_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| format!("Query columns: {e}"))?;

    for col_row in col_rows.flatten() {
        let (tbl, col_name, bg3_type) = col_row;
        // Skip internal columns
        if col_name.starts_with('_') {
            continue;
        }
        columns_by_table.entry(tbl).or_default().push((
            col_name,
            bg3_type.unwrap_or_else(|| "FixedString".to_string()),
        ));
    }

    // 5. Build NodeSchema per (region_id, node_id) — only root-level tables
    //    For each region, pick the table with the most rows whose node_id is not "root"
    //    (some regions like "Color" have a single "root" node — use region_id as node_id)
    let mut seen: HashMap<(String, String), usize> = HashMap::new(); // dedup key
    let mut schemas = Vec::new();

    for meta in &meta_rows {
        if meta.region_id.is_empty() || meta.node_id.is_empty() {
            continue;
        }
        // Skip child tables and junction tables themselves
        if child_tables.contains(&meta.table_name) {
            continue;
        }
        if meta.table_name.contains("__to__") {
            continue;
        }

        // Resolve the section name: Section variant name if exists, else region_id
        let section_name = region_map
            .get(meta.region_id.as_str())
            .cloned()
            .unwrap_or_else(|| meta.region_id.clone());

        // Use node_id as the schema key; treat "root" as the region_id
        let effective_node_id = if meta.node_id == "root" {
            meta.region_id.clone()
        } else {
            meta.node_id.clone()
        };

        // Dedup: keep only the first (highest row_count) table per (section, node_id)
        let dedup_key = (section_name.clone(), effective_node_id.clone());
        if seen.contains_key(&dedup_key) {
            continue;
        }
        seen.insert(dedup_key, schemas.len());

        // Build attributes from column types
        let cols = columns_by_table
            .get(&meta.table_name)
            .cloned()
            .unwrap_or_default();
        let mut attributes: Vec<AttrSchema> = cols
            .into_iter()
            .map(|(name, attr_type)| AttrSchema {
                name,
                attr_type,
                frequency: 1.0,   // DB columns are always present
                examples: vec![], // No examples from metadata alone
            })
            .collect();

        // Sort: UUID/MapKey first, then alphabetically
        attributes.sort_by(|a, b| {
            let a_pk = a.name == "UUID" || a.name == "MapKey";
            let b_pk = b.name == "UUID" || b.name == "MapKey";
            if a_pk && !b_pk {
                return std::cmp::Ordering::Less;
            }
            if b_pk && !a_pk {
                return std::cmp::Ordering::Greater;
            }
            a.name.cmp(&b.name)
        });

        // Build children from junction tables
        let children_raw = children_by_parent
            .get(&meta.table_name)
            .cloned()
            .unwrap_or_default();
        let children: Vec<ChildSchema> = children_raw
            .into_iter()
            .map(|(group_id, child_node_id, _child_rc)| ChildSchema {
                group_id,
                child_node_id,
                frequency: 1.0, // Junction presence = always applicable
            })
            .collect();

        schemas.push(NodeSchema {
            node_id: effective_node_id,
            section: section_name,
            sample_count: meta.row_count as usize,
            attributes,
            children,
        });
    }

    // ── Phase 6: Stats table schemas ──────────────────────────────────
    // Each stats__<type> table gets a NodeSchema with its columns as attributes.
    // Section and node_id both use the entry_type (e.g. "SpellData", "Armor").
    let mut stats_stmt = conn
        .prepare(
            "SELECT table_name, row_count \
             FROM _table_meta \
             WHERE source_type = 'stats' AND row_count > 0 \
             ORDER BY row_count DESC",
        )
        .map_err(|e| format!("Prepare stats meta: {e}"))?;

    let stats_rows: Vec<(String, i64)> = stats_stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|e| format!("Query stats meta: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    // Load stats column types in bulk
    let mut stats_col_stmt = conn
        .prepare(
            "SELECT table_name, column_name, bg3_type \
             FROM _column_types \
             WHERE table_name LIKE 'stats%' \
             ORDER BY table_name, column_name",
        )
        .map_err(|e| format!("Prepare stats col: {e}"))?;

    let mut stats_columns_by_table: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let stats_col_rows = stats_col_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| format!("Query stats cols: {e}"))?;

    let stats_internal = ["_entry_name", "_file_id", "_type", "_using"];
    for (tn, cn, bt) in stats_col_rows.flatten() {
        if stats_internal.contains(&cn.as_str()) || cn.ends_with("_version") {
            continue;
        }
        stats_columns_by_table.entry(tn).or_default().push((cn, bt));
    }

    for (table_name, row_count) in &stats_rows {
        // Entry type from table name: "stats__SpellData" → "SpellData"
        let entry_type = table_name.strip_prefix("stats__").unwrap_or(table_name);

        let cols = stats_columns_by_table
            .get(table_name)
            .cloned()
            .unwrap_or_default();

        let attributes: Vec<AttrSchema> = cols
            .into_iter()
            .map(|(name, attr_type)| AttrSchema {
                name,
                attr_type,
                frequency: 1.0,
                examples: vec![],
            })
            .collect();

        schemas.push(NodeSchema {
            node_id: entry_type.to_string(),
            section: format!("stats:{entry_type}"),
            sample_count: *row_count as usize,
            attributes,
            children: vec![],
        });
    }

    Ok(schemas)
}

/// Query entries from any section by region_id string (unified replacement for
/// both `query_vanilla_entries` and `query_entries_by_folder`).
///
/// Looks up the table via `_table_meta`, discovers columns, and returns entries
/// with intelligent display name formatting.
pub fn query_section_entries(
    db_path: &Path,
    region_id: &str,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<VanillaEntryInfo>, String> {
    let conn = open_ro(db_path)?;

    // Find the main table for this region
    // Prefer root tables (parent_tables IS NULL) and direct children of root
    // over deeper child tables, which may have more rows but are NOT the primary entry table.
    let table_name: String = conn
        .query_row(
            "SELECT table_name FROM _table_meta \
             WHERE region_id = ?1 AND source_type = 'lsx' AND row_count > 0 \
             ORDER BY \
               CASE \
                 WHEN parent_tables IS NULL THEN 0 \
                 WHEN parent_tables LIKE '%\\_\\_root' ESCAPE '\\' THEN 1 \
                 ELSE 2 \
               END, \
               row_count DESC \
             LIMIT 1",
            [region_id],
            |row| row.get(0),
        )
        .map_err(|e| format!("No table for region '{region_id}': {e}"))?;

    validate_table_name(&table_name)?;

    let node_id: String = conn
        .query_row(
            "SELECT COALESCE(node_id, '') FROM _table_meta WHERE table_name = ?1",
            [&table_name],
            |row| row.get(0),
        )
        .unwrap_or_default();

    let columns = table_columns(&conn, &table_name)?;
    let has = |name: &str| columns.iter().any(|c| c.name == name);

    let pk_col = columns
        .iter()
        .find(|c| c.name == "UUID")
        .or_else(|| columns.iter().find(|c| c.name == "MapKey"))
        .or_else(|| columns.iter().find(|c| c.name == "_row_id"))
        .map(|c| c.name.as_str())
        .unwrap_or("rowid");

    // Rowid-strategy columns are INTEGER; cast to TEXT for uniform handling
    let pk_is_integer = pk_col == "_row_id" || pk_col == "rowid";
    let pk_select_expr = if pk_is_integer {
        format!("CAST(\"{pk_col}\" AS TEXT)")
    } else {
        format!("\"{pk_col}\"")
    };

    // Select PK + all display-relevant columns
    let mut select_cols = vec![pk_select_expr];
    for col in &[
        "Name",
        "Level",
        "Comment",
        "DisplayName",
        "ParentGuid",
        "Color",
        "UIColor",
        "Text",
    ] {
        if has(col) {
            select_cols.push(format!("\"{col}\""));
        }
    }

    let sql = format!(
        "SELECT {} FROM \"{}\" ORDER BY \"{}\" LIMIT ?1 OFFSET ?2",
        select_cols.join(", "),
        table_name,
        pk_col,
    );

    let lim = limit.unwrap_or(0) as i64;
    let off = offset.unwrap_or(0) as i64;
    let effective_limit = if lim == 0 { -1i64 } else { lim };

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let rows = stmt
        .query_map([effective_limit, off], |row| {
            let uuid: String = row.get(0)?;

            let get_opt = |name: &str| -> Option<String> {
                let idx = select_cols
                    .iter()
                    .position(|c| c == &format!("\"{name}\""))?;
                row.get::<_, Option<String>>(idx).ok().flatten()
            };

            let name_val = get_opt("Name").unwrap_or_default();
            let level_val = get_opt("Level");
            let comment_val = get_opt("Comment");
            let display_name_val = get_opt("DisplayName");
            let parent_guid_val = get_opt("ParentGuid");
            let color_raw = get_opt("UIColor").or_else(|| get_opt("Color"));
            let text_handle_val = get_opt("Text");

            Ok((
                uuid,
                name_val,
                level_val,
                comment_val,
                display_name_val,
                parent_guid_val,
                color_raw,
                text_handle_val,
            ))
        })
        .map_err(|e| format!("Query: {e}"))?;

    let mut entries = Vec::new();
    for row in rows {
        let (
            uuid,
            name,
            level,
            comment,
            display_name_attr,
            parent_guid,
            color_raw,
            text_handle_raw,
        ) = row.map_err(|e| format!("Row: {e}"))?;

        // Intelligent display name: prefer DisplayName, then "Name LvN", then Comment, then Name, then UUID
        let display_name = display_name_attr
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(String::from)
            .or_else(|| {
                if !name.is_empty() {
                    if let Some(lvl) = level.as_deref().filter(|v| !v.is_empty()) {
                        Some(format!("{name} Lv{lvl}"))
                    } else {
                        Some(name.clone())
                    }
                } else {
                    None
                }
            })
            .or_else(|| comment.filter(|v| !v.is_empty()))
            .unwrap_or_else(|| uuid.clone());

        let color = color_raw.and_then(|v| crate::parse_bgra_color(&v));
        let parent_guid = parent_guid.filter(|v| !v.is_empty());
        // Extract loca handle from Text attribute (TranslatedString format: "handle;version")
        let text_handle = text_handle_raw
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(|v| v.split(';').next().unwrap_or(v).to_string());

        entries.push(VanillaEntryInfo {
            uuid,
            display_name,
            node_id: node_id.clone(),
            color,
            parent_guid,
            text_handle,
        });
    }

    Ok(entries)
}

/// Detect the primary key column for a table by checking PRAGMA table_info
/// for the `pk` flag.
fn detect_pk_column(conn: &Connection, table_name: &str) -> String {
    if validate_table_name(table_name).is_err() {
        return "rowid".to_string();
    }
    let result: Option<String> = conn
        .query_row(
            &format!("SELECT name FROM pragma_table_info('{table_name}') WHERE pk = 1"),
            [],
            |row| row.get(0),
        )
        .ok();
    result.unwrap_or_else(|| "rowid".to_string())
}

// ── Full-fidelity queries for scan/diff pipeline ────────────────────────────

use crate::models::{LsxAttribute, LsxChildEntry, LsxChildGroup, LsxEntry, StatsEntry};
use std::collections::HashMap;

/// Load a column-name → bg3_type map for a table from `_column_types`.
fn column_type_map(conn: &Connection, table_name: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT column_name, bg3_type FROM _column_types WHERE table_name = ?1")
    else {
        return map;
    };
    let Ok(rows) = stmt.query_map([table_name], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        if let Some(bg3) = row.1 {
            map.insert(row.0, bg3);
        }
    }
    map
}

/// Query ALL vanilla LSX entries for a section, returning full `LsxEntry` objects
/// with attributes, types, and children — everything the diff pipeline needs.
pub fn query_vanilla_lsx_for_scan(
    db_path: &Path,
    section: &Section,
) -> Result<HashMap<String, LsxEntry>, String> {
    let region_ids = section.region_ids();
    if region_ids.len() <= 1 {
        // Single-region section: use the fast path
        return query_vanilla_lsx_by_region(db_path, section.region_id());
    }
    // Multi-region section: merge entries from all regions
    let mut merged = HashMap::new();
    for region_id in region_ids {
        if let Ok(entries) = query_vanilla_lsx_by_region(db_path, region_id) {
            merged.extend(entries);
        }
    }
    Ok(merged)
}

/// Query ALL vanilla LSX entries for a given region_id string.
pub fn query_vanilla_lsx_by_region(
    db_path: &Path,
    region_id: &str,
) -> Result<HashMap<String, LsxEntry>, String> {
    let conn = open_ro(db_path)?;

    // Find the main table for this section
    let table_name: String = match conn.query_row(
        &format!(
            "SELECT table_name FROM _table_meta \
             WHERE region_id = ?1 AND source_type = 'lsx' AND row_count > 0 {ROOT_TABLE_ORDER}"
        ),
        [region_id],
        |row| row.get(0),
    ) {
        Ok(tn) => tn,
        Err(_) => return Ok(HashMap::new()), // section not in DB
    };

    validate_table_name(&table_name)?;

    let node_id: String = conn
        .query_row(
            "SELECT COALESCE(node_id, '') FROM _table_meta WHERE table_name = ?1",
            [&table_name],
            |row| row.get(0),
        )
        .unwrap_or_default();

    // Get all user columns (skip _file_id and other internal columns)
    let columns = table_columns(&conn, &table_name)?;
    let data_cols: Vec<&ColInfo> = columns
        .iter()
        .filter(|c| !c.name.starts_with('_'))
        .collect();

    // Load bg3 type map for attribute reconstruction
    let type_map = column_type_map(&conn, &table_name);

    // Determine PK column
    let _pk_col = data_cols
        .iter()
        .find(|c| c.name == "UUID")
        .or_else(|| data_cols.iter().find(|c| c.name == "MapKey"))
        .map(|c| c.name.as_str())
        .unwrap_or("rowid");

    // Build SELECT with all data columns
    let col_names: Vec<&str> = data_cols.iter().map(|c| c.name.as_str()).collect();
    let select_expr = col_names
        .iter()
        .map(|n| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {select_expr} FROM \"{table_name}\"");

    let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let mut attributes = HashMap::new();
            let mut uuid_val = String::new();

            for (i, col_name) in col_names.iter().enumerate() {
                let val: Option<String> = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                    rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                    rusqlite::types::ValueRef::Text(s) => {
                        let s = std::str::from_utf8(s).unwrap_or("");
                        if s.is_empty() {
                            None
                        } else {
                            Some(s.to_string())
                        }
                    }
                    rusqlite::types::ValueRef::Blob(_) => None,
                };
                if let Some(ref v) = val {
                    if !v.is_empty() {
                        let bg3_type = type_map
                            .get(*col_name)
                            .cloned()
                            .unwrap_or_else(|| "LSString".to_string());
                        attributes.insert(
                            col_name.to_string(),
                            LsxAttribute {
                                attr_type: bg3_type,
                                value: v.clone(),
                            },
                        );
                    }
                }
                // Capture UUID/MapKey
                if *col_name == "UUID" || *col_name == "MapKey" {
                    if let Some(ref v) = val {
                        uuid_val = v.clone();
                    }
                }
            }

            Ok((uuid_val, attributes))
        })
        .map_err(|e| format!("Query: {e}"))?;

    // Collect all entries
    let mut entries: HashMap<String, LsxEntry> = HashMap::new();
    for row in rows {
        let (uuid, attributes) = row.map_err(|e| format!("Row: {e}"))?;
        if uuid.is_empty() {
            continue;
        }
        entries.insert(
            uuid.clone(),
            LsxEntry {
                uuid,
                node_id: node_id.clone(),
                attributes,
                children: Vec::new(),
                commented: false,
                region_id: String::new(),
            },
        );
    }

    // ── Load children from junction tables ──
    // Find all junction tables where this table is the parent
    let _junction_sql = "SELECT table_name, child_table FROM _table_meta \
                        WHERE parent_tables LIKE ?1 AND source_type = 'junction'";
    // junction tables don't have source_type='junction' — they're stored differently.
    // Instead, scan _table_meta for tables whose name matches the pattern:
    //   <parent_table>__to__<child_node>
    let jt_prefix = format!("{table_name}__to__");
    let mut jt_stmt = conn
        .prepare("SELECT table_name, node_id FROM _table_meta WHERE table_name LIKE ?1")
        .map_err(|e| format!("JT prepare: {e}"))?;
    let jt_rows = jt_stmt
        .query_map([format!("{jt_prefix}%")], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .map_err(|e| format!("JT query: {e}"))?;

    for jt_row in jt_rows.flatten() {
        let (jt_name, child_node_id) = jt_row;

        validate_table_name(&jt_name)?;

        // Extract group_id from junction table name: parent__to__ChildNode → ChildNode
        let group_id = jt_name
            .strip_prefix(&jt_prefix)
            .unwrap_or(&jt_name)
            .to_string();

        // Read (parent_id, child_id) pairs from the junction table
        let child_sql = format!("SELECT parent_id, child_id FROM \"{jt_name}\" ORDER BY parent_id");
        let Ok(mut child_stmt) = conn.prepare(&child_sql) else {
            continue;
        };
        let Ok(child_rows) = child_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) else {
            continue;
        };

        let child_node = child_node_id.unwrap_or_else(|| group_id.clone());

        for child_row in child_rows.flatten() {
            let (parent_uuid, child_uuid) = child_row;
            if let Some(entry) = entries.get_mut(&parent_uuid) {
                // Find or create the child group
                let group = entry.children.iter_mut().find(|g| g.group_id == group_id);
                if let Some(group) = group {
                    group.entries.push(LsxChildEntry {
                        node_id: child_node.clone(),
                        object_guid: child_uuid,
                    });
                } else {
                    entry.children.push(LsxChildGroup {
                        group_id: group_id.clone(),
                        entries: vec![LsxChildEntry {
                            node_id: child_node.clone(),
                            object_guid: child_uuid,
                        }],
                    });
                }
            }
        }
    }

    Ok(entries)
}

/// Query ALL vanilla stats entries from the DB, returning full `StatsEntry`
/// objects with all fields — everything the diff pipeline needs.
pub fn query_vanilla_stats_for_scan(db_path: &Path) -> Result<HashMap<String, StatsEntry>, String> {
    let conn = open_ro(db_path)?;

    // Find all stats tables
    let mut meta_stmt = conn
        .prepare(
            "SELECT table_name FROM _table_meta \
             WHERE source_type = 'stats' AND row_count > 0 \
             ORDER BY table_name",
        )
        .map_err(|e| format!("Prepare: {e}"))?;
    let table_rows = meta_stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("Query: {e}"))?;

    let mut all_entries: HashMap<String, StatsEntry> = HashMap::new();

    for table_row in table_rows.flatten() {
        let table_name = table_row;
        let entry_type = table_name
            .strip_prefix("stats__")
            .unwrap_or(&table_name)
            .to_string();

        validate_table_name(&table_name)?;

        let columns = table_columns(&conn, &table_name)?;
        let _data_cols: Vec<&str> = columns
            .iter()
            .map(|c| c.name.as_str())
            .filter(|n| !n.starts_with('_'))
            .collect();

        // Internal columns that aren't user data
        let _internal = ["_entry_name", "_type", "_using", "_file_id"];

        let select_expr = columns
            .iter()
            .map(|c| format!("\"{}\"", c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {select_expr} FROM \"{table_name}\"");

        let mut stmt = conn.prepare(&sql).map_err(|e| format!("Prepare: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                let mut name = String::new();
                let mut using: Option<String> = None;
                let mut data = HashMap::new();

                for (i, col) in columns.iter().enumerate() {
                    let val: Option<String> = match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => None,
                        rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                        rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                        rusqlite::types::ValueRef::Text(s) => {
                            let s = std::str::from_utf8(s).unwrap_or("");
                            if s.is_empty() {
                                None
                            } else {
                                Some(s.to_string())
                            }
                        }
                        rusqlite::types::ValueRef::Blob(_) => None,
                    };
                    match col.name.as_str() {
                        "_entry_name" => {
                            name = val.unwrap_or_default();
                        }
                        "_using" => {
                            using = val.filter(|v| !v.is_empty());
                        }
                        n if n.starts_with('_') => {} // skip internal
                        _ => {
                            if let Some(v) = val {
                                if !v.is_empty() {
                                    data.insert(col.name.clone(), v);
                                }
                            }
                        }
                    }
                }

                Ok(StatsEntry {
                    name,
                    entry_type: entry_type.clone(),
                    parent: using,
                    data,
                })
            })
            .map_err(|e| format!("Query: {e}"))?;

        for row in rows {
            let entry = row.map_err(|e| format!("Row: {e}"))?;
            if !entry.name.is_empty() {
                all_entries.insert(entry.name.clone(), entry);
            }
        }
    }

    Ok(all_entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Create a temp SQLite DB file with `_table_meta` and return (NamedTempFile, PathBuf).
    /// Caller keeps `NamedTempFile` alive so the file isn't deleted.
    fn create_test_db() -> (NamedTempFile, std::path::PathBuf) {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE _table_meta (
                table_name TEXT PRIMARY KEY,
                source_type TEXT NOT NULL,
                region_id TEXT,
                node_id TEXT,
                parent_tables TEXT,
                row_count INTEGER DEFAULT 0
            );",
        )
        .unwrap();
        (tmp, path)
    }

    /// Populate a DB with a typical LSX section (Races-like) for testing.
    fn seed_lsx_section(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"lsx__Races__Race\" (
                \"UUID\" TEXT PRIMARY KEY,
                \"Name\" TEXT,
                \"DisplayName\" TEXT,
                \"ParentGuid\" TEXT,
                \"Color\" TEXT
            );
            INSERT INTO \"lsx__Races__Race\" VALUES
                ('uuid-1', 'Human', 'Human', '', ''),
                ('uuid-2', 'Elf', 'Elf', 'uuid-1', ''),
                ('uuid-3', 'Dwarf', '', '', '');
            INSERT INTO _table_meta VALUES
                ('lsx__Races__Race', 'lsx', 'Races', 'Race', NULL, 3);",
        )
        .unwrap();
    }

    /// Populate a DB with a Progressions-like section (has Level column).
    fn seed_progressions_section(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"lsx__Progressions__Progression\" (
                \"UUID\" TEXT PRIMARY KEY,
                \"Name\" TEXT,
                \"Level\" TEXT
            );
            INSERT INTO \"lsx__Progressions__Progression\" VALUES
                ('prog-1', 'Fighter', '5'),
                ('prog-2', 'Rogue', '3');
            INSERT INTO _table_meta VALUES
                ('lsx__Progressions__Progression', 'lsx', 'Progressions', 'Progression', NULL, 2);",
        )
        .unwrap();
    }

    /// Populate a DB with junction-connected parent/child tables.
    fn seed_with_junction(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"lsx__ClassDescriptions__ClassDescription\" (
                \"UUID\" TEXT PRIMARY KEY,
                \"Name\" TEXT
            );
            INSERT INTO \"lsx__ClassDescriptions__ClassDescription\" VALUES
                ('cls-1', 'Fighter'),
                ('cls-2', 'Wizard');

            CREATE TABLE \"lsx__ClassDescriptions__SubClasses\" (
                \"UUID\" TEXT PRIMARY KEY,
                \"Name\" TEXT
            );
            INSERT INTO \"lsx__ClassDescriptions__SubClasses\" VALUES
                ('sub-1', 'Champion'),
                ('sub-2', 'Evocation');

            CREATE TABLE \"lsx__ClassDescriptions__ClassDescription__to__SubClasses\" (
                parent_id TEXT NOT NULL,
                child_id TEXT NOT NULL,
                PRIMARY KEY (parent_id, child_id)
            );
            INSERT INTO \"lsx__ClassDescriptions__ClassDescription__to__SubClasses\" VALUES
                ('cls-1', 'sub-1'),
                ('cls-2', 'sub-2');

            INSERT INTO _table_meta VALUES
                ('lsx__ClassDescriptions__ClassDescription', 'lsx', 'ClassDescriptions', 'ClassDescription', NULL, 2),
                ('lsx__ClassDescriptions__SubClasses', 'lsx', 'ClassDescriptions', 'SubClasses', NULL, 2),
                ('lsx__ClassDescriptions__ClassDescription__to__SubClasses', 'junction', NULL, NULL,
                 'lsx__ClassDescriptions__ClassDescription,lsx__ClassDescriptions__SubClasses', 2);",
        )
        .unwrap();
    }

    /// Populate a stats section.
    fn seed_stats(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"stats__SpellData\" (
                \"_entry_name\" TEXT PRIMARY KEY,
                \"_file_id\" INTEGER,
                \"_type\" TEXT,
                \"_using\" TEXT,
                \"SpellType\" TEXT,
                \"Level\" TEXT,
                \"DisplayName\" TEXT
            );
            INSERT INTO \"stats__SpellData\" VALUES
                ('Fireball', 1, 'SpellData', 'Projectile_MainHandAttack', 'Projectile', '3', 'Fireball'),
                ('MagicMissile', 2, 'SpellData', NULL, 'Projectile', '1', 'Magic Missile');
            INSERT INTO _table_meta VALUES
                ('stats__SpellData', 'stats', NULL, NULL, NULL, 2);",
        )
        .unwrap();
    }

    /// Populate a loca section.
    fn seed_loca(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"loca__english\" (
                \"contentuid\" TEXT PRIMARY KEY,
                \"text\" TEXT
            );
            INSERT INTO \"loca__english\" VALUES
                ('h0001', 'Hello World'),
                ('h0002', 'Goodbye');
            INSERT INTO _table_meta VALUES
                ('loca__english', 'loca', NULL, NULL, NULL, 2);",
        )
        .unwrap();
    }

    // ── Happy path: query_available_sections ────────────────────────────

    #[test]
    fn available_sections_returns_populated_lsx_tables() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);

        let sections = query_available_sections(&path).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].region_id, "Races");
        assert_eq!(sections[0].source_type, "lsx");
        assert_eq!(sections[0].row_count, 3);
        assert_eq!(sections[0].pk_column, "UUID");
        assert!(sections[0].children.is_empty());
    }

    #[test]
    fn available_sections_nests_junction_children() {
        let (_tmp, path) = create_test_db();
        seed_with_junction(&path);

        let sections = query_available_sections(&path).unwrap();
        // Only parent should be top-level; child should be nested
        assert_eq!(
            sections.len(),
            1,
            "Child table should not appear at top level"
        );
        let parent = &sections[0];
        assert_eq!(parent.region_id, "ClassDescriptions");
        assert_eq!(parent.node_id, "ClassDescription");
        assert_eq!(parent.children.len(), 1);

        let child = &parent.children[0];
        assert_eq!(child.child_node_id, "SubClasses");
        assert_eq!(child.child_row_count, 2);
        assert!(child.junction_table.contains("__to__"));
    }

    #[test]
    fn available_sections_includes_stats_and_loca() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);
        seed_loca(&path);

        let sections = query_available_sections(&path).unwrap();
        let types: Vec<&str> = sections.iter().map(|s| s.source_type.as_str()).collect();
        assert!(types.contains(&"stats"), "Should include stats section");
        assert!(types.contains(&"loca"), "Should include loca section");
    }

    #[test]
    fn available_sections_empty_db_returns_empty() {
        let (_tmp, path) = create_test_db();
        // _table_meta exists but has no rows
        let sections = query_available_sections(&path).unwrap();
        assert!(sections.is_empty());
    }

    // ── Happy path: query_section_entries ────────────────────────────────

    #[test]
    fn section_entries_returns_all_rows() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);

        let entries = query_section_entries(&path, "Races", None, None).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].node_id, "Race");
    }

    #[test]
    fn section_entries_display_name_prefers_displayname_attr() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);

        let entries = query_section_entries(&path, "Races", None, None).unwrap();
        // uuid-1 has DisplayName="Human" — should use that
        let human = entries.iter().find(|e| e.uuid == "uuid-1").unwrap();
        assert_eq!(human.display_name, "Human");
    }

    #[test]
    fn section_entries_display_name_falls_back_to_name() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);

        let entries = query_section_entries(&path, "Races", None, None).unwrap();
        // uuid-3 has Name="Dwarf", DisplayName="" → should use Name
        let dwarf = entries.iter().find(|e| e.uuid == "uuid-3").unwrap();
        assert_eq!(dwarf.display_name, "Dwarf");
    }

    #[test]
    fn section_entries_display_name_falls_back_to_uuid() {
        let (_tmp, path) = create_test_db();
        // Create a table with no Name/DisplayName columns at all
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"lsx__Misc__Thing\" (
                \"UUID\" TEXT PRIMARY KEY,
                \"Value\" TEXT
            );
            INSERT INTO \"lsx__Misc__Thing\" VALUES ('bare-uuid', '42');
            INSERT INTO _table_meta VALUES
                ('lsx__Misc__Thing', 'lsx', 'Misc', 'Thing', NULL, 1);",
        )
        .unwrap();

        let entries = query_section_entries(&path, "Misc", None, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].display_name, "bare-uuid");
    }

    #[test]
    fn section_entries_level_appended_when_present() {
        let (_tmp, path) = create_test_db();
        seed_progressions_section(&path);

        let entries = query_section_entries(&path, "Progressions", None, None).unwrap();
        let fighter = entries.iter().find(|e| e.uuid == "prog-1").unwrap();
        assert_eq!(fighter.display_name, "Fighter Lv5");
    }

    #[test]
    fn section_entries_pagination_works() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);

        let page1 = query_section_entries(&path, "Races", Some(2), None).unwrap();
        assert_eq!(page1.len(), 2);

        let page2 = query_section_entries(&path, "Races", Some(2), Some(2)).unwrap();
        assert_eq!(page2.len(), 1);
    }

    #[test]
    fn section_entries_includes_parent_guid() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);

        let entries = query_section_entries(&path, "Races", None, None).unwrap();
        let elf = entries.iter().find(|e| e.uuid == "uuid-2").unwrap();
        assert_eq!(elf.parent_guid.as_deref(), Some("uuid-1"));
    }

    // ── Happy path: existing query functions ────────────────────────────

    #[test]
    fn db_has_vanilla_data_true_when_populated() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);
        assert!(db_has_vanilla_data(&path));
    }

    #[test]
    fn db_has_stats_data_true_when_populated() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);
        assert!(db_has_stats_data(&path));
    }

    #[test]
    fn db_has_loca_data_true_when_populated() {
        let (_tmp, path) = create_test_db();
        seed_loca(&path);
        assert!(db_has_loca_data(&path));
    }

    #[test]
    fn query_stat_entries_returns_sorted() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        let entries = query_stat_entries(&path, "SpellData").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Fireball");
        assert_eq!(entries[1].name, "MagicMissile");
        assert_eq!(entries[0].entry_type, "SpellData");
    }

    #[test]
    fn query_stat_entries_all_types() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        let entries = query_stat_entries(&path, "").unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn query_stat_field_names_excludes_internal() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        let names = query_stat_field_names(&path, "SpellData").unwrap();
        assert!(names.contains(&"SpellType".to_string()));
        assert!(names.contains(&"Level".to_string()));
        assert!(names.contains(&"DisplayName".to_string()));
        assert!(!names.contains(&"_entry_name".to_string()));
    }

    #[test]
    fn query_stat_entry_fields_returns_known_entry_fields() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        let fields = query_stat_entry_fields(&path, "Fireball", "SpellData").unwrap();
        assert_eq!(
            fields.get("SpellType").map(String::as_str),
            Some("Projectile")
        );
        assert_eq!(fields.get("Level").map(String::as_str), Some("3"));
        assert_eq!(
            fields.get("DisplayName").map(String::as_str),
            Some("Fireball")
        );
        assert!(!fields.contains_key("_entry_name"));
        assert!(!fields.contains_key("_file_id"));
        assert!(!fields.contains_key("_type"));
        assert!(!fields.contains_key("_using"));
    }

    #[test]
    fn query_stat_entry_fields_unknown_entry_returns_empty() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        let fields = query_stat_entry_fields(&path, "UnknownSpell", "SpellData").unwrap();
        assert!(fields.is_empty());
    }

    #[test]
    fn query_localization_map_returns_entries() {
        let (_tmp, path) = create_test_db();
        seed_loca(&path);

        let entries = query_localization_map(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].handle, "h0001");
        assert_eq!(entries[0].text, "Hello World");
    }

    // ── Unhappy path: missing / corrupt DB ──────────────────────────────

    #[test]
    fn available_sections_nonexistent_db_returns_error() {
        let path = std::path::PathBuf::from("/nonexistent/path/db.sqlite");
        let result = query_available_sections(&path);
        assert!(result.is_err());
    }

    #[test]
    fn section_entries_nonexistent_region_returns_error() {
        let (_tmp, path) = create_test_db();
        seed_lsx_section(&path);

        let result = query_section_entries(&path, "DoesNotExist", None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No table for region"));
    }

    #[test]
    fn section_entries_nonexistent_db_returns_error() {
        let path = std::path::PathBuf::from("/nonexistent/path/db.sqlite");
        let result = query_section_entries(&path, "Races", None, None);
        assert!(result.is_err());
    }

    #[test]
    fn db_has_vanilla_data_false_for_missing_file() {
        let path = std::path::PathBuf::from("/nonexistent/db.sqlite");
        assert!(!db_has_vanilla_data(&path));
    }

    #[test]
    fn db_has_stats_data_false_for_missing_file() {
        let path = std::path::PathBuf::from("/nonexistent/db.sqlite");
        assert!(!db_has_stats_data(&path));
    }

    #[test]
    fn db_has_loca_data_false_for_missing_file() {
        let path = std::path::PathBuf::from("/nonexistent/db.sqlite");
        assert!(!db_has_loca_data(&path));
    }

    #[test]
    fn db_has_vanilla_data_false_for_empty_db() {
        let (_tmp, path) = create_test_db();
        // _table_meta exists but no rows
        assert!(!db_has_vanilla_data(&path));
    }

    #[test]
    fn db_has_stats_data_false_for_empty_db() {
        let (_tmp, path) = create_test_db();
        assert!(!db_has_stats_data(&path));
    }

    #[test]
    fn query_stat_entries_nonexistent_type_returns_empty() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        // Asking for a stats type that doesn't exist — table missing, should
        // gracefully skip via the `Err(_) => continue` branch
        let entries = query_stat_entries(&path, "FakeType").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn query_localization_map_no_loca_table_returns_error() {
        let (_tmp, path) = create_test_db();
        // No loca table seeded
        let result = query_localization_map(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No loca table"));
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn available_sections_skips_zero_row_count() {
        let (_tmp, path) = create_test_db();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE \"lsx__Empty__Node\" (\"UUID\" TEXT PRIMARY KEY);
            INSERT INTO _table_meta VALUES
                ('lsx__Empty__Node', 'lsx', 'Empty', 'Node', NULL, 0);",
        )
        .unwrap();

        let sections = query_available_sections(&path).unwrap();
        assert!(
            sections.is_empty(),
            "Tables with row_count=0 should be excluded"
        );
    }

    #[test]
    fn detect_pk_column_returns_rowid_when_no_pk() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path();
        let conn = Connection::open(path).unwrap();
        // WITHOUT ROWID can't be used without PK, but a regular table with
        // no explicit PK still has an implicit rowid
        conn.execute_batch("CREATE TABLE no_pk (a TEXT, b TEXT);")
            .unwrap();

        assert_eq!(detect_pk_column(&conn, "no_pk"), "rowid");
    }

    #[test]
    fn detect_pk_column_finds_explicit_pk() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE with_pk (\"MapKey\" TEXT PRIMARY KEY, val TEXT);")
            .unwrap();

        assert_eq!(detect_pk_column(&conn, "with_pk"), "MapKey");
    }

    // ── Table name validation ───────────────────────────────────────────

    #[test]
    fn validate_table_name_accepts_valid_names() {
        assert!(validate_table_name("lsx__Races__Race").is_ok());
        assert!(validate_table_name("stats__SpellData").is_ok());
        assert!(validate_table_name("loca__english").is_ok());
        assert!(validate_table_name("valuelist_entries").is_ok());
        assert!(
            validate_table_name("lsx__ClassDescriptions__ClassDescription__to__SubClasses").is_ok()
        );
        assert!(validate_table_name("_table_meta").is_ok());
    }

    #[test]
    fn validate_table_name_rejects_empty() {
        let result = validate_table_name("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must not be empty"));
    }

    #[test]
    fn validate_table_name_rejects_sql_injection_attempts() {
        // Double-quote escape
        assert!(validate_table_name("foo\" OR 1=1 --").is_err());
        // Semicolon
        assert!(validate_table_name("foo; DROP TABLE _table_meta").is_err());
        // Single quote
        assert!(validate_table_name("foo'bar").is_err());
        // Spaces
        assert!(validate_table_name("foo bar").is_err());
        // Parentheses
        assert!(validate_table_name("foo()").is_err());
        // Dash (comment)
        assert!(validate_table_name("foo--bar").is_err());
    }

    #[test]
    fn validate_table_name_rejects_non_ascii() {
        assert!(validate_table_name("table_naïve").is_err());
        assert!(validate_table_name("表名").is_err());
    }

    #[test]
    fn query_stat_entries_rejects_malicious_entry_type() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        let result = query_stat_entries(&path, "SpellData\" OR 1=1 --");
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("Invalid table name"));
    }

    #[test]
    fn query_stat_field_names_rejects_malicious_entry_type() {
        let (_tmp, path) = create_test_db();
        seed_stats(&path);

        let result = query_stat_field_names(&path, "foo; DROP TABLE x");
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("Invalid table name"));
    }

    #[test]
    fn detect_pk_column_returns_rowid_for_invalid_name() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path();
        let conn = Connection::open(path).unwrap();

        assert_eq!(detect_pk_column(&conn, "bad name!"), "rowid");
    }
}
