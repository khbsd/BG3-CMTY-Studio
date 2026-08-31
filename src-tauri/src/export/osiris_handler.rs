//! Osiris goal-file export handler.
//!
//! Implements [`FileTypeHandler`] for exporting Osiris goal files from two sources:
//! 1. **Auto-generated goals** — from meta keys `osiris_goal_entries` and `osiris_goal_file_uuid`
//! 2. **Authored goals** — from `_staging_authoring` blobs matching `Story/RawFiles/Goals/*.txt`

use std::path::PathBuf;

use crate::error::AppError;
use crate::validation::osiris::{
    cross_validate_parent_edges, validate_osiris_goal, DiagnosticSeverity as OsiDiagSeverity,
};

use super::{
    ExportContext, ExportUnit, FileAction, FileTypeHandler, HandlerWarning, WarningSeverity,
};

pub struct OsirisHandler;

impl FileTypeHandler for OsirisHandler {
    fn name(&self) -> &str {
        "OsirisHandler"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec![]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec![
            "osiris_goal_entries",
            "osiris_goal_file_uuid",
            "osiris_authored",
        ]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let mut units = Vec::new();

        // ── Pathway 1: Auto-generated goals from meta keys ──
        let auto_goals = query_auto_generated_goals(&ctx.staging_conn)?;
        for (goal_name, content) in &auto_goals {
            let rel_path = PathBuf::from("Mods")
                .join(&ctx.mod_folder)
                .join("Story")
                .join("RawFiles")
                .join("Goals")
                .join(format!("{goal_name}.txt"));
            let full_path = ctx.mod_path.join(&rel_path);
            let action = if full_path.exists() {
                FileAction::Update
            } else {
                FileAction::Create
            };

            units.push(ExportUnit {
                handler_name: self.name().to_string(),
                output_path: rel_path,
                action,
                entry_count: 1,
                content: Some(content.as_bytes().to_vec()),
            });
        }

        // ── Pathway 2: Authored goals from _staging_authoring ──
        let authored_keys = query_authored_goal_keys(&ctx.staging_conn)?;
        for key in &authored_keys {
            let rel_path = PathBuf::from(key);
            let full_path = ctx.mod_path.join(&rel_path);
            let action = if full_path.exists() {
                FileAction::Update
            } else {
                FileAction::Create
            };

            units.push(ExportUnit {
                handler_name: self.name().to_string(),
                output_path: rel_path,
                action,
                entry_count: 1,
                content: None, // Will be populated during render
            });
        }

        // ── Check for deleted authored goals ──
        let deleted_keys = query_deleted_goal_keys(&ctx.staging_conn)?;
        for key in &deleted_keys {
            let rel_path = PathBuf::from(key);
            let full_path = ctx.mod_path.join(&rel_path);
            if full_path.exists() {
                units.push(ExportUnit {
                    handler_name: self.name().to_string(),
                    output_path: rel_path,
                    action: FileAction::Delete,
                    entry_count: 0,
                    content: None,
                });
            }
        }

        Ok(units)
    }

    fn render(&self, unit: &ExportUnit, ctx: &ExportContext) -> Result<Vec<u8>, AppError> {
        // If content was pre-populated (auto-generated goals), use it directly
        if let Some(ref content) = unit.content {
            return Ok(content.clone());
        }

        // Otherwise read from staging authoring (authored goals)
        let key = unit.output_path.to_string_lossy().replace('\\', "/");
        let value = ctx
            .staging_conn
            .query_row(
                "SELECT value FROM _staging_authoring WHERE key = ?1 AND \"_is_deleted\" = 0",
                [&key],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| {
                AppError::internal(format!("OsirisHandler render read key '{key}': {e}"))
            })?;

        Ok(value.into_bytes())
    }

    fn validate(&self, ctx: &ExportContext) -> Result<Vec<HandlerWarning>, AppError> {
        let mut warnings = Vec::new();

        // Collect all goals for cross-validation
        let mut all_goals: Vec<(String, String)> = Vec::new();

        // Auto-generated goals
        let auto_goals = query_auto_generated_goals(&ctx.staging_conn)?;
        for (name, content) in &auto_goals {
            // Run structural validation
            let diags = validate_osiris_goal(content);
            for diag in &diags {
                let severity = match diag.severity {
                    OsiDiagSeverity::Error => WarningSeverity::Error,
                    OsiDiagSeverity::Warning => WarningSeverity::Warning,
                    OsiDiagSeverity::Info => WarningSeverity::Info,
                };
                warnings.push(HandlerWarning {
                    handler_name: self.name().to_string(),
                    message: format!("Auto-goal '{}' line {}: {}", name, diag.line, diag.message),
                    severity,
                });
            }
            all_goals.push((name.clone(), content.clone()));
        }

        // Authored goals
        let authored_keys = query_authored_goal_keys(&ctx.staging_conn)?;
        for key in &authored_keys {
            let content = match ctx.staging_conn.query_row(
                "SELECT value FROM _staging_authoring WHERE key = ?1 AND \"_is_deleted\" = 0",
                [key.as_str()],
                |row| row.get::<_, String>(0),
            ) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let goal_name = extract_goal_name(key);
            let diags = validate_osiris_goal(&content);
            for diag in &diags {
                let severity = match diag.severity {
                    OsiDiagSeverity::Error => WarningSeverity::Error,
                    OsiDiagSeverity::Warning => WarningSeverity::Warning,
                    OsiDiagSeverity::Info => WarningSeverity::Info,
                };
                warnings.push(HandlerWarning {
                    handler_name: self.name().to_string(),
                    message: format!("Goal '{}' line {}: {}", goal_name, diag.line, diag.message),
                    severity,
                });
            }
            all_goals.push((goal_name, content));
        }

        // Cross-reference ParentTargetEdge between all goals
        if all_goals.len() > 1 {
            let goal_refs: Vec<(&str, &str)> = all_goals
                .iter()
                .map(|(n, c)| (n.as_str(), c.as_str()))
                .collect();
            let cross_diags = cross_validate_parent_edges(&goal_refs);
            for diag in &cross_diags {
                let severity = match diag.severity {
                    OsiDiagSeverity::Error => WarningSeverity::Error,
                    OsiDiagSeverity::Warning => WarningSeverity::Warning,
                    OsiDiagSeverity::Info => WarningSeverity::Info,
                };
                warnings.push(HandlerWarning {
                    handler_name: self.name().to_string(),
                    message: diag.message.clone(),
                    severity,
                });
            }
        }

        Ok(warnings)
    }
}

/// Extract goal name from a staging key path.
/// e.g., `Mods/MyMod/Story/RawFiles/Goals/MyGoal.txt` → `MyGoal`
fn extract_goal_name(key: &str) -> String {
    key.rsplit('/')
        .next()
        .unwrap_or(key)
        .strip_suffix(".txt")
        .unwrap_or(key)
        .to_string()
}

/// Query auto-generated goal content from meta keys.
///
/// The `osiris_goal_entries` meta key stores goal content as a JSON-encoded
/// map of {goal_name: content_string}.
fn query_auto_generated_goals(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, String)>, AppError> {
    let json_value: Option<String> = conn
        .query_row(
            "SELECT value FROM _staging_authoring WHERE key = 'osiris_goal_entries' AND \"_is_deleted\" = 0",
            [],
            |row| row.get(0),
        )
        .ok();

    let Some(json_str) = json_value else {
        return Ok(vec![]);
    };

    let goals: std::collections::HashMap<String, String> = serde_json::from_str(&json_str)
        .map_err(|e| {
            AppError::parse_error(format!("Failed to parse osiris_goal_entries JSON: {e}"))
        })?;

    Ok(goals.into_iter().collect())
}

/// Query all non-deleted authored goal file keys from `_staging_authoring`.
fn query_authored_goal_keys(conn: &rusqlite::Connection) -> Result<Vec<String>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key FROM _staging_authoring \
             WHERE key LIKE '%Story/RawFiles/Goals/%.txt' \
             AND \"_is_deleted\" = 0 \
             ORDER BY key",
        )
        .map_err(|e| AppError::internal(format!("OsirisHandler query authored keys: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| AppError::internal(format!("OsirisHandler query map: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("OsirisHandler collect: {e}")))?;
    Ok(rows)
}

/// Query deleted goal file keys for cleanup.
fn query_deleted_goal_keys(conn: &rusqlite::Connection) -> Result<Vec<String>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key FROM _staging_authoring \
             WHERE key LIKE '%Story/RawFiles/Goals/%.txt' \
             AND \"_is_deleted\" = 1 \
             ORDER BY key",
        )
        .map_err(|e| AppError::internal(format!("OsirisHandler query deleted keys: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| AppError::internal(format!("OsirisHandler query map: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("OsirisHandler collect: {e}")))?;
    Ok(rows)
}
