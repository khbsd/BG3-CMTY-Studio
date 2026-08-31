//! ScriptExtender Lua file export handler.
//!
//! Implements [`FileTypeHandler`] for exporting all `ScriptExtender/**/*.lua`
//! files from `_staging_authoring` entries.

use std::collections::HashSet;
use std::path::PathBuf;

use regex::Regex;

use crate::error::AppError;

use super::{
    ExportContext, ExportUnit, FileAction, FileTypeHandler, HandlerWarning, WarningSeverity,
};

pub struct SeLuaHandler;

impl FileTypeHandler for SeLuaHandler {
    fn name(&self) -> &str {
        "SeLuaHandler"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec![]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec!["se_lua"]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let rows = query_se_lua_keys(&ctx.staging_conn)?;

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
                AppError::internal(format!("SeLuaHandler render read key '{key}': {e}"))
            })?;

        Ok(value.into_bytes())
    }

    fn validate(&self, ctx: &ExportContext) -> Result<Vec<HandlerWarning>, AppError> {
        let lua_keys = query_all_se_lua_keys(&ctx.staging_conn)?;
        let mut warnings = Vec::new();

        if lua_keys.is_empty() {
            return Ok(warnings);
        }

        // Collect all SE keys (including config) for cross-reference
        let all_se_keys = query_all_se_keys(&ctx.staging_conn)?;
        let key_set: HashSet<&str> = all_se_keys.iter().map(|s| s.as_str()).collect();

        let has_server_modules = lua_keys.iter().any(|k| k.contains("/Lua/Server/"));
        let has_client_modules = lua_keys.iter().any(|k| k.contains("/Lua/Client/"));
        let has_bootstrap_server = lua_keys.iter().any(|k| k.ends_with("/BootstrapServer.lua"));
        let has_bootstrap_client = lua_keys.iter().any(|k| k.ends_with("/BootstrapClient.lua"));
        let has_config = all_se_keys
            .iter()
            .any(|k| k.ends_with("/ScriptExtender/Config.json"));

        // Check 1: Server modules without BootstrapServer.lua
        if has_server_modules && !has_bootstrap_server {
            warnings.push(HandlerWarning {
                handler_name: self.name().to_string(),
                message: "Server/ modules found but BootstrapServer.lua is missing".into(),
                severity: WarningSeverity::Warning,
            });
        }

        // Check 2: Client modules without BootstrapClient.lua
        if has_client_modules && !has_bootstrap_client {
            warnings.push(HandlerWarning {
                handler_name: self.name().to_string(),
                message: "Client/ modules found but BootstrapClient.lua is missing".into(),
                severity: WarningSeverity::Warning,
            });
        }

        // Check 3: Lua files exist but Config.json is missing
        if !has_config {
            warnings.push(HandlerWarning {
                handler_name: self.name().to_string(),
                message: "SE Lua files exist but ScriptExtender/Config.json is missing".into(),
                severity: WarningSeverity::Warning,
            });
        }

        // Check 4: Validate Ext.Require() references
        let ext_require_re = Regex::new(r#"Ext\.Require\(\s*"([^"]+)"\s*\)"#).expect("valid regex");

        for key in &lua_keys {
            let content = match query_se_file_content(&ctx.staging_conn, key) {
                Ok(c) => c,
                Err(_) => continue,
            };

            for cap in ext_require_re.captures_iter(&content) {
                let required_path = &cap[1];
                // Ext.Require paths are relative to the Lua/ directory.
                // Resolve against the key's base SE path.
                if let Some(lua_base) = find_lua_base(key) {
                    let resolved = format!("{lua_base}{required_path}.lua");
                    if !key_set.contains(resolved.as_str()) {
                        warnings.push(HandlerWarning {
                            handler_name: self.name().to_string(),
                            message: format!(
                                "Ext.Require(\"{required_path}\") in {key} references \
                                 non-existent file: {resolved}"
                            ),
                            severity: WarningSeverity::Warning,
                        });
                    }
                }
            }
        }

        Ok(warnings)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Query non-deleted, changed Lua file keys from `_staging_authoring`.
fn query_se_lua_keys(conn: &rusqlite::Connection) -> Result<Vec<String>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key FROM _staging_authoring \
             WHERE key LIKE '%ScriptExtender/%.lua' \
             AND _is_deleted = 0 \
             AND (_is_new = 1 OR _is_modified = 1)",
        )
        .map_err(|e| AppError::internal(format!("SeLuaHandler prepare: {e}")))?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| AppError::internal(format!("SeLuaHandler query: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("SeLuaHandler rows: {e}")))?;

    Ok(rows)
}

/// Query ALL non-deleted Lua file keys (regardless of change status) for validation.
fn query_all_se_lua_keys(conn: &rusqlite::Connection) -> Result<Vec<String>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key FROM _staging_authoring \
             WHERE key LIKE '%ScriptExtender/%.lua' \
             AND _is_deleted = 0",
        )
        .map_err(|e| AppError::internal(format!("SeLuaHandler validate prepare: {e}")))?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| AppError::internal(format!("SeLuaHandler validate query: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("SeLuaHandler validate rows: {e}")))?;

    Ok(rows)
}

/// Query ALL non-deleted SE keys (lua + config) for cross-reference validation.
fn query_all_se_keys(conn: &rusqlite::Connection) -> Result<Vec<String>, AppError> {
    let mut stmt = conn
        .prepare(
            "SELECT key FROM _staging_authoring \
             WHERE key LIKE '%ScriptExtender/%' \
             AND _is_deleted = 0",
        )
        .map_err(|e| AppError::internal(format!("SeLuaHandler se_keys prepare: {e}")))?;

    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| AppError::internal(format!("SeLuaHandler se_keys query: {e}")))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::internal(format!("SeLuaHandler se_keys rows: {e}")))?;

    Ok(rows)
}

/// Read a single file's content from `_staging_authoring`.
fn query_se_file_content(conn: &rusqlite::Connection, key: &str) -> Result<String, AppError> {
    conn.query_row(
        "SELECT value FROM _staging_authoring WHERE key = ?1 AND \"_is_deleted\" = 0",
        [key],
        |row| row.get::<_, Option<String>>(0),
    )
    .map(|v| v.unwrap_or_default())
    .map_err(|e| AppError::internal(format!("SeLuaHandler read content '{key}': {e}")))
}

/// Extract the `Mods/{folder}/ScriptExtender/Lua/` base path from a key.
///
/// E.g. `Mods/MyMod/ScriptExtender/Lua/Server/Foo.lua`
///    → `Mods/MyMod/ScriptExtender/Lua/`
fn find_lua_base(key: &str) -> Option<String> {
    key.find("/ScriptExtender/Lua/")
        .map(|idx| key[..idx + "/ScriptExtender/Lua/".len()].to_string())
}
