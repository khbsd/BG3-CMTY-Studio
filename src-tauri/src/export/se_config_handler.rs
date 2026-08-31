//! ScriptExtender `Config.json` export handler.
//!
//! Implements [`FileTypeHandler`] for exporting the SE Config.json file
//! from `_staging_authoring` entries keyed like
//! `Mods/{folder}/ScriptExtender/Config.json`.

use std::path::PathBuf;

use crate::error::AppError;
use crate::validation::se_config::{validate_se_config, DiagnosticSeverity};

use super::{
    ExportContext, ExportUnit, FileAction, FileTypeHandler, HandlerWarning, WarningSeverity,
};

pub struct SeConfigHandler;

impl FileTypeHandler for SeConfigHandler {
    fn name(&self) -> &str {
        "SeConfigHandler"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec![]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec!["se_config"]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let rows = query_se_config_rows(&ctx.staging_conn)?;

        let mut units = Vec::new();
        for (key, _value) in rows {
            let rel_path = PathBuf::from(&key);
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
                content: None,
            });
        }

        Ok(units)
    }

    fn render(&self, unit: &ExportUnit, ctx: &ExportContext) -> Result<Vec<u8>, AppError> {
        let key = unit.output_path.to_string_lossy().replace('\\', "/");
        let value = ctx
            .staging_conn
            .query_row(
                "SELECT value FROM _staging_authoring WHERE key = ?1 AND \"_is_deleted\" = 0",
                [&key],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| {
                AppError::internal(format!("SeConfigHandler render read key '{key}': {e}"))
            })?;

        Ok(value.into_bytes())
    }

    fn validate(&self, ctx: &ExportContext) -> Result<Vec<HandlerWarning>, AppError> {
        let rows = query_se_config_rows(&ctx.staging_conn)?;
        let mut warnings = Vec::new();

        for (key, content) in rows {
            let diagnostics = validate_se_config(&content);
            for diag in diagnostics {
                let severity = match diag.severity {
                    DiagnosticSeverity::Error => WarningSeverity::Error,
                    DiagnosticSeverity::Warning => WarningSeverity::Warning,
                    DiagnosticSeverity::Info => continue,
                };
                warnings.push(HandlerWarning {
                    handler_name: self.name().to_string(),
                    message: format!(
                        "[{key}:{line}:{col}] {msg}",
                        line = diag.line,
                        col = diag.col,
                        msg = diag.message,
                    ),
                    severity,
                });
            }
        }

        Ok(warnings)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Query all non-deleted, changed Config.json entries from `_staging_authoring`.
fn query_se_config_rows(conn: &rusqlite::Connection) -> Result<Vec<(String, String)>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key, value FROM _staging_authoring \
             WHERE key LIKE '%ScriptExtender/Config.json' \
             AND _is_deleted = 0 \
             AND (_is_new = 1 OR _is_modified = 1)",
        )
        .map_err(|e| AppError::internal(format!("SeConfigHandler prepare: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .map_err(|e| AppError::internal(format!("SeConfigHandler query: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("SeConfigHandler rows: {e}")))?;

    Ok(rows)
}
