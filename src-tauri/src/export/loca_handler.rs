use std::collections::HashSet;
use std::path::PathBuf;

use super::{
    all_rows_deleted, get_meta_value, has_tracked_changes, ExportContext, ExportUnit, FileAction,
    FileTypeHandler,
};
use crate::error::AppError;

/// File-definition entry parsed from the `loca_file_definitions` JSON meta value.
#[derive(serde::Deserialize)]
struct LocaFileDef {
    label: String,
    handles: Vec<String>,
}

/// Row fetched from the `loca__english` staging table.
struct LocaRow {
    contentuid: String,
    version: String,
    text: String,
}

pub struct LocaHandler;

impl FileTypeHandler for LocaHandler {
    fn name(&self) -> &str {
        "loca"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec!["loca__"]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec!["loca_file_definitions"]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let table = "loca__english";

        if !has_tracked_changes(&ctx.staging_conn, table)? {
            return Ok(Vec::new());
        }

        if all_rows_deleted(&ctx.staging_conn, table)? {
            return Ok(vec![ExportUnit {
                handler_name: self.name().to_string(),
                output_path: loca_output_path(&ctx.mod_folder, "english"),
                action: FileAction::Delete,
                entry_count: 0,
                content: None,
            }]);
        }

        // Count non-deleted rows
        let total_count: i64 = ctx
            .staging_conn
            .query_row(
                "SELECT COUNT(*) FROM \"loca__english\" WHERE \"_is_deleted\" = 0",
                [],
                |row| row.get(0),
            )
            .map_err(|e| AppError::internal(format!("Count loca rows: {e}")))?;

        if total_count == 0 {
            return Ok(Vec::new());
        }
        let total_count = total_count as usize;

        let file_defs = read_file_definitions(ctx)?;
        let mut units = Vec::new();

        if file_defs.is_empty() {
            // Single-file mode: all entries → english.xml
            units.push(ExportUnit {
                handler_name: self.name().to_string(),
                output_path: loca_output_path(&ctx.mod_folder, "english"),
                action: FileAction::Create,
                entry_count: total_count,
                content: None,
            });
        } else {
            // User-defined file splitting
            let mut assigned_handles: HashSet<String> = HashSet::new();

            for def in &file_defs {
                if def.handles.is_empty() {
                    continue;
                }

                let placeholders: String = def
                    .handles
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT COUNT(*) FROM \"loca__english\" \
                     WHERE \"_is_deleted\" = 0 AND contentuid IN ({placeholders})"
                );
                let mut stmt = ctx
                    .staging_conn
                    .prepare(&sql)
                    .map_err(|e| AppError::internal(format!("Prepare loca count: {e}")))?;

                let count: i64 = stmt
                    .query_row(rusqlite::params_from_iter(def.handles.iter()), |row| {
                        row.get(0)
                    })
                    .map_err(|e| {
                        AppError::internal(format!("Count loca def '{}': {e}", def.label))
                    })?;
                let count = count as usize;

                if count == 0 {
                    continue;
                }

                for h in &def.handles {
                    assigned_handles.insert(h.clone());
                }

                units.push(ExportUnit {
                    handler_name: self.name().to_string(),
                    output_path: loca_output_path(&ctx.mod_folder, &def.label),
                    action: FileAction::Create,
                    entry_count: count,
                    content: None,
                });
            }

            // Fallback: unassigned entries → english.xml
            let fallback_count = if assigned_handles.is_empty() {
                total_count
            } else {
                let placeholders: String = assigned_handles
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT COUNT(*) FROM \"loca__english\" \
                     WHERE \"_is_deleted\" = 0 AND contentuid NOT IN ({placeholders})"
                );
                let mut stmt = ctx
                    .staging_conn
                    .prepare(&sql)
                    .map_err(|e| AppError::internal(format!("Prepare loca fallback count: {e}")))?;

                stmt.query_row(rusqlite::params_from_iter(assigned_handles.iter()), |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|e| AppError::internal(format!("Count loca fallback: {e}")))?
                    as usize
            };

            if fallback_count > 0 {
                units.push(ExportUnit {
                    handler_name: self.name().to_string(),
                    output_path: loca_output_path(&ctx.mod_folder, "english"),
                    action: FileAction::Create,
                    entry_count: fallback_count,
                    content: None,
                });
            }
        }

        Ok(units)
    }

    fn render(&self, unit: &ExportUnit, ctx: &ExportContext) -> Result<Vec<u8>, AppError> {
        // Derive which entries belong to this unit from the output path stem
        let file_stem = unit
            .output_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("english");

        let file_defs = read_file_definitions(ctx)?;

        let rows = if file_stem == "english" && !file_defs.is_empty() {
            // Fallback file: entries not assigned to any definition
            let assigned: HashSet<String> = file_defs
                .iter()
                .flat_map(|d| d.handles.iter().cloned())
                .collect();
            let all_rows = query_loca_rows(&ctx.staging_conn)?;
            all_rows
                .into_iter()
                .filter(|r| !assigned.contains(&r.contentuid))
                .collect()
        } else if let Some(def) = file_defs.iter().find(|d| d.label == file_stem) {
            // Named definition: only entries whose contentuid is in the handles list
            let handle_set: HashSet<&str> = def.handles.iter().map(|s| s.as_str()).collect();
            let all_rows = query_loca_rows(&ctx.staging_conn)?;
            all_rows
                .into_iter()
                .filter(|r| handle_set.contains(r.contentuid.as_str()))
                .collect()
        } else {
            // No file definitions — all entries
            query_loca_rows(&ctx.staging_conn)?
        };

        Ok(render_content_list(&rows))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the relative output path: `Mods/{folder}/Localization/English/{name}.xml`
fn loca_output_path(mod_folder: &str, name: &str) -> PathBuf {
    PathBuf::from("Mods")
        .join(mod_folder)
        .join("Localization")
        .join("English")
        .join(format!("{name}.xml"))
}

/// Read and parse `loca_file_definitions` from `_staging_authoring`.
fn read_file_definitions(ctx: &ExportContext) -> Result<Vec<LocaFileDef>, AppError> {
    let raw = get_meta_value(&ctx.staging_conn, "loca_file_definitions")?;
    match raw {
        Some(json) if !json.is_empty() => {
            let defs: Vec<LocaFileDef> = serde_json::from_str(&json).map_err(|e| {
                AppError::parse_error(format!("Invalid loca_file_definitions JSON: {e}"))
            })?;
            Ok(defs)
        }
        _ => Ok(Vec::new()),
    }
}

/// Query all non-deleted rows from `loca__english`, sorted by contentuid.
fn query_loca_rows(conn: &rusqlite::Connection) -> Result<Vec<LocaRow>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT contentuid, version, text FROM \"loca__english\" \
             WHERE \"_is_deleted\" = 0 ORDER BY contentuid",
        )
        .map_err(|e| AppError::internal(format!("Prepare loca query: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok(LocaRow {
                contentuid: row.get(0)?,
                version: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                text: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })
        .map_err(|e| AppError::internal(format!("Query loca rows: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("Read loca rows: {e}")))?;

    Ok(rows)
}

/// Render loca rows into BG3 ContentList XML format (UTF-8).
fn render_content_list(rows: &[LocaRow]) -> Vec<u8> {
    let mut out = String::with_capacity(rows.len() * 120 + 80);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<contentList>\n");

    for row in rows {
        let version = if row.version.is_empty() {
            "1"
        } else {
            &row.version
        };
        out.push_str("  <content contentuid=\"");
        xml_escape_into(&mut out, &row.contentuid);
        out.push_str("\" version=\"");
        xml_escape_into(&mut out, version);
        out.push_str("\">");
        xml_escape_into(&mut out, &row.text);
        out.push_str("</content>\n");
    }

    out.push_str("</contentList>\n");
    out.into_bytes()
}

/// Escape XML special characters into an existing `String`.
fn xml_escape_into(buf: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => buf.push_str("&amp;"),
            '<' => buf.push_str("&lt;"),
            '>' => buf.push_str("&gt;"),
            '"' => buf.push_str("&quot;"),
            '\'' => buf.push_str("&apos;"),
            _ => buf.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_special_chars() {
        let mut buf = String::new();
        xml_escape_into(&mut buf, "Tom & Jerry <\"friends\"> it's");
        assert_eq!(buf, "Tom &amp; Jerry &lt;&quot;friends&quot;&gt; it&apos;s");
    }

    #[test]
    fn render_content_list_basic() {
        let rows = vec![
            LocaRow {
                contentuid: "h0001".into(),
                version: "1".into(),
                text: "Hello".into(),
            },
            LocaRow {
                contentuid: "h0002".into(),
                version: "".into(),
                text: "A & B".into(),
            },
        ];
        let bytes = render_content_list(&rows);
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(xml.contains("<content contentuid=\"h0001\" version=\"1\">Hello</content>"));
        assert!(xml.contains("<content contentuid=\"h0002\" version=\"1\">A &amp; B</content>"));
        assert!(xml.ends_with("</contentList>\n"));
    }

    #[test]
    fn render_content_list_empty() {
        let bytes = render_content_list(&[]);
        let xml = String::from_utf8(bytes).unwrap();
        assert!(xml.contains("<contentList>\n</contentList>"));
    }

    #[test]
    fn loca_output_path_default() {
        let p = loca_output_path("MyMod", "english");
        assert_eq!(
            p,
            PathBuf::from("Mods/MyMod/Localization/English/english.xml")
        );
    }

    #[test]
    fn loca_output_path_custom() {
        let p = loca_output_path("MyMod", "spells_loca");
        assert_eq!(
            p,
            PathBuf::from("Mods/MyMod/Localization/English/spells_loca.xml")
        );
    }
}
