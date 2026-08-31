//! Behavior script export handler (Anubis / Constellations).
//!
//! Implements [`FileTypeHandler`] for exporting behavior script files
//! from `_staging_authoring` entries. A single struct is parameterized
//! by script system name, directory, and file extensions.

use std::path::PathBuf;

use crate::error::AppError;

use super::{
    ExportContext, ExportUnit, FileAction, FileTypeHandler, HandlerWarning, WarningSeverity,
};

pub struct BehaviorScriptHandler {
    /// Human-readable name for this handler instance (e.g., "AnubisHandler").
    handler_name: String,
    /// The meta key prefix for staging authoring entries (e.g., "anubis_script").
    meta_key_prefix: String,
    /// The subdirectory under Scripts/ where these files live (e.g., "anubis").
    script_dir: String,
    /// Valid file extensions for this script type (e.g., [".anc", ".ann", ".anm"]).
    extensions: Vec<String>,
}

impl BehaviorScriptHandler {
    pub fn new(name: &str, script_dir: &str, extensions: &[&str]) -> Self {
        let capitalized = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_default()
            + &name[1..];
        Self {
            handler_name: format!("{capitalized}Handler"),
            meta_key_prefix: format!("{name}_script"),
            script_dir: script_dir.to_string(),
            extensions: extensions.iter().map(|e| e.to_string()).collect(),
        }
    }

    /// Check if a staging authoring key belongs to this handler.
    fn claims_key(&self, key: &str) -> bool {
        let normalized = key.replace('\\', "/");
        let dir_pattern = format!("Scripts/{}/", self.script_dir);
        if !normalized.contains(&dir_pattern) {
            return false;
        }
        self.extensions.iter().any(|ext| normalized.ends_with(ext))
    }

    /// Query all non-deleted script keys from `_staging_authoring` for this handler.
    fn query_keys(&self, conn: &rusqlite::Connection) -> Result<Vec<String>, AppError> {
        let pattern = format!("%Scripts/{}/%", self.script_dir);
        let mut stmt = conn
            .prepare(
                "SELECT key FROM _staging_authoring \
                 WHERE key LIKE ?1 AND \"_is_deleted\" = 0 \
                 ORDER BY key",
            )
            .map_err(|e| AppError::internal(format!("{} query keys: {e}", self.name())))?;

        let rows = stmt
            .query_map([&pattern], |row| row.get::<_, String>(0))
            .map_err(|e| AppError::internal(format!("{} query map: {e}", self.name())))?;

        let mut keys = Vec::new();
        for row in rows {
            let key =
                row.map_err(|e| AppError::internal(format!("{} row read: {e}", self.name())))?;
            if self.claims_key(&key) {
                keys.push(key);
            }
        }
        Ok(keys)
    }
}

impl FileTypeHandler for BehaviorScriptHandler {
    fn name(&self) -> &str {
        &self.handler_name
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec![]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec![&self.meta_key_prefix]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let keys = self.query_keys(&ctx.staging_conn)?;
        let mut units = Vec::new();

        for key in keys {
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
                AppError::internal(format!("{} render read key '{key}': {e}", self.name()))
            })?;

        Ok(value.into_bytes())
    }

    fn validate(&self, ctx: &ExportContext) -> Result<Vec<HandlerWarning>, AppError> {
        let keys = self.query_keys(&ctx.staging_conn)?;
        let mut warnings = Vec::new();

        let config_ext = &self.extensions[0];
        let state_ext = if self.extensions.len() > 1 {
            &self.extensions[1]
        } else {
            config_ext
        };
        let module_ext = if self.extensions.len() > 2 {
            &self.extensions[2]
        } else {
            config_ext
        };

        let mut configs: Vec<String> = Vec::new();
        let mut states: Vec<String> = Vec::new();
        let mut modules: Vec<String> = Vec::new();

        for key in &keys {
            if key.ends_with(config_ext) {
                configs.push(key.clone());
            } else if key.ends_with(state_ext) {
                states.push(key.clone());
            } else if key.ends_with(module_ext) {
                modules.push(key.clone());
            }
        }

        // Warn about configs without any states
        if !configs.is_empty() && states.is_empty() {
            warnings.push(HandlerWarning {
                handler_name: self.name().to_string(),
                message: format!(
                    "Config files found but no state files in Scripts/{}/",
                    self.script_dir
                ),
                severity: WarningSeverity::Warning,
            });
        }

        // Warn about orphaned states (states in dirs where no config exists)
        for state in &states {
            let state_dir = PathBuf::from(state)
                .parent()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let has_config = configs.iter().any(|c| {
                let config_dir = PathBuf::from(c)
                    .parent()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                config_dir == state_dir
            });
            if !has_config {
                warnings.push(HandlerWarning {
                    handler_name: self.name().to_string(),
                    message: format!(
                        "State file '{state}' has no corresponding config in the same directory"
                    ),
                    severity: WarningSeverity::Info,
                });
            }
        }

        // Check module imports resolve
        for module_key in &modules {
            let content = match ctx.staging_conn.query_row(
                "SELECT value FROM _staging_authoring WHERE key = ?1 AND \"_is_deleted\" = 0",
                [module_key.as_str()],
                |row| row.get::<_, String>(0),
            ) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("--") {
                    continue;
                }
                if let Some(start) = trimmed.find("require") {
                    if let Some(quote_start) = trimmed[start..].find('"') {
                        let after_quote = &trimmed[start + quote_start + 1..];
                        if let Some(quote_end) = after_quote.find('"') {
                            let required = &after_quote[..quote_end];
                            if required.contains('/') || required.contains('\\') {
                                let exists = keys.iter().any(|k| k.contains(required));
                                if !exists {
                                    warnings.push(HandlerWarning {
                                        handler_name: self.name().to_string(),
                                        message: format!(
                                            "Module '{module_key}' requires '{required}' which was not found"
                                        ),
                                        severity: WarningSeverity::Warning,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(warnings)
    }
}
