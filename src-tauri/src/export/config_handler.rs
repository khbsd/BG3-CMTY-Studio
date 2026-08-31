//! Generic config file export handler (JSON / YAML).
//!
//! Implements [`FileTypeHandler`] for exporting generic JSON and YAML config
//! files from `_staging_authoring` entries. Excludes `ScriptExtender/Config.json`
//! which is owned by [`super::se_config_handler::SeConfigHandler`].
//!
//! MCM blueprint files (filename contains `MCM_blueprint`) are additionally
//! validated via [`crate::validation::mcm_blueprint::validate_mcm_blueprint`].

use std::path::PathBuf;

use crate::error::AppError;
use crate::validation::mcm_blueprint::{
    validate_mcm_blueprint, Diagnostic as McmDiagnostic, DiagnosticSeverity as McmSeverity,
};

use super::{
    ExportContext, ExportUnit, FileAction, FileTypeHandler, HandlerWarning, WarningSeverity,
};

pub struct ConfigFileHandler;

impl FileTypeHandler for ConfigFileHandler {
    fn name(&self) -> &str {
        "ConfigFileHandler"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec!["config__"]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec!["config_file"]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let rows = query_config_keys(&ctx.staging_conn)?;

        let mut units = Vec::new();
        for key in rows {
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
                AppError::internal(format!("ConfigFileHandler render read key '{key}': {e}"))
            })?;

        Ok(value.into_bytes())
    }

    fn validate(&self, ctx: &ExportContext) -> Result<Vec<HandlerWarning>, AppError> {
        let rows = query_all_config_entries(&ctx.staging_conn)?;
        let mut warnings = Vec::new();

        for (key, content) in rows {
            let lower = key.to_lowercase();

            if lower.ends_with(".json") {
                // JSON parse validation
                if let Err(e) = serde_json::from_str::<serde_json::Value>(&content) {
                    warnings.push(HandlerWarning {
                        handler_name: self.name().to_string(),
                        message: format!("[{key}] Invalid JSON: {e}"),
                        severity: WarningSeverity::Error,
                    });
                }

                // MCM blueprint validation (delegate to dedicated validator)
                if is_mcm_blueprint(&key) {
                    let diagnostics = validate_mcm_blueprint(&content);
                    for diag in diagnostics {
                        let severity = mcm_severity_to_warning(&diag);
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
            } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
                // YAML parse validation
                if let Err(e) = serde_saphyr::from_str::<serde_json::Value>(&content) {
                    warnings.push(HandlerWarning {
                        handler_name: self.name().to_string(),
                        message: format!("[{key}] Invalid YAML: {e}"),
                        severity: WarningSeverity::Error,
                    });
                }
            }
        }

        Ok(warnings)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether a staging key represents an MCM blueprint file.
fn is_mcm_blueprint(key: &str) -> bool {
    let file_name = key.rsplit(['/', '\\']).next().unwrap_or(key);
    file_name.contains("MCM_blueprint")
}

/// Returns `true` if the key is the SE `Config.json` (claimed by `SeConfigHandler`).
fn is_se_config(key: &str) -> bool {
    let normalized = key.replace('\\', "/");
    normalized.ends_with("ScriptExtender/Config.json")
}

/// Convert MCM blueprint diagnostic severity to handler warning severity.
fn mcm_severity_to_warning(diag: &McmDiagnostic) -> WarningSeverity {
    match diag.severity {
        McmSeverity::Error => WarningSeverity::Error,
        McmSeverity::Warning => WarningSeverity::Warning,
        McmSeverity::Info => WarningSeverity::Info,
    }
}

/// Query non-deleted, changed config file keys from `_staging_authoring`.
///
/// Returns only `.json` / `.yaml` / `.yml` keys, excluding those already
/// claimed by other handlers (e.g., `ScriptExtender/Config.json`).
fn query_config_keys(conn: &rusqlite::Connection) -> Result<Vec<String>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key FROM _staging_authoring \
             WHERE (key LIKE '%.json' OR key LIKE '%.yaml' OR key LIKE '%.yml') \
             AND key NOT LIKE '%ScriptExtender/Config.json' \
             AND _is_deleted = 0 \
             AND (_is_new = 1 OR _is_modified = 1) \
             ORDER BY key",
        )
        .map_err(|e| AppError::internal(format!("ConfigFileHandler prepare: {e}")))?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| AppError::internal(format!("ConfigFileHandler query: {e}")))?;

    let mut keys = Vec::new();
    for row in rows {
        let key =
            row.map_err(|e| AppError::internal(format!("ConfigFileHandler row read: {e}")))?;
        // Double-check exclusion for SE Config.json (covers backslash variants)
        if !is_se_config(&key) {
            keys.push(key);
        }
    }
    Ok(keys)
}

/// Query ALL non-deleted config file entries (regardless of change status) for validation.
fn query_all_config_entries(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, String)>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key, value FROM _staging_authoring \
             WHERE (key LIKE '%.json' OR key LIKE '%.yaml' OR key LIKE '%.yml') \
             AND key NOT LIKE '%ScriptExtender/Config.json' \
             AND _is_deleted = 0 \
             ORDER BY key",
        )
        .map_err(|e| AppError::internal(format!("ConfigFileHandler validate prepare: {e}")))?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .map_err(|e| AppError::internal(format!("ConfigFileHandler validate query: {e}")))?;

    let mut entries = Vec::new();
    for row in rows {
        let (key, value) =
            row.map_err(|e| AppError::internal(format!("ConfigFileHandler validate row: {e}")))?;
        if !is_se_config(&key) {
            entries.push((key, value));
        }
    }
    Ok(entries)
}
