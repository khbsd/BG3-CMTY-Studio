use crate::blocking;
use crate::error::AppError;
use std::path::{Path, PathBuf};

/// Info about a script file on disk.
#[derive(serde::Serialize, Clone)]
pub struct ScriptFileInfo {
    pub path: String,
    pub size: i64,
    pub is_new: bool,
    pub is_modified: bool,
}

/// Built-in script templates.
fn get_script_template(template_id: &str) -> Option<&'static str> {
    match template_id {
        "lua_empty" => Some("-- {{FILE_NAME}}\n-- Created by CMTY Studio\n\n"),
        "lua_se_bootstrap" => Some("-- {{FILE_NAME}}\n-- Script Extender bootstrap\n\nExt.Vars.RegisterModVariable(\n    ModuleUUID,\n    \"{{VAR_NAME}}\",\n    {\n        Server = true,\n        Client = true,\n        SyncToClient = true,\n    }\n)\n"),
        "osiris_goal" => Some(include_str!("../../resources/templates/osiris_basic_goal.txt")),
        "osiris_basic_goal" => Some(include_str!("../../resources/templates/osiris_basic_goal.txt")),
        "osiris_state_machine" => Some(include_str!("../../resources/templates/osiris_state_machine.txt")),
        "osiris_quest_goal" => Some(include_str!("../../resources/templates/osiris_quest_goal.txt")),
        "khonsu_empty" => Some(include_str!("../../resources/templates/khonsu_basic_condition.khn")),
        "khonsu_basic_condition" => Some(include_str!("../../resources/templates/khonsu_basic_condition.khn")),
        "khonsu_context_condition" => Some(include_str!("../../resources/templates/khonsu_context_condition.khn")),
        "anubis_empty" => Some(include_str!("../../resources/templates/anubis_state.ann")),
        "anubis_config" => Some(include_str!("../../resources/templates/anubis_config.anc")),
        "anubis_state" => Some(include_str!("../../resources/templates/anubis_state.ann")),
        "anubis_module" => Some(include_str!("../../resources/templates/anubis_module.anm")),
        "constellations_empty" => Some(include_str!("../../resources/templates/constellations_state.cln")),
        "constellations_config" => Some(include_str!("../../resources/templates/constellations_config.clc")),
        "constellations_state" => Some(include_str!("../../resources/templates/constellations_state.cln")),
        "constellations_module" => Some(include_str!("../../resources/templates/constellations_module.clm")),
        "localization_contentlist" => Some("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<contentList date=\"{{DATE}}\">\n  <content contentuid=\"{{CONTENT_UID}}\" version=\"{{VERSION}}\">Sample Text</content>\n</contentList>\n"),
        "se_config" => Some(include_str!("../../resources/templates/se_config.json")),
        "se_bootstrap_server" => Some(include_str!("../../resources/templates/se_bootstrap_server.lua")),
        "se_bootstrap_client" => Some(include_str!("../../resources/templates/se_bootstrap_client.lua")),
        "lua_se_server_module" => Some(include_str!("../../resources/templates/se_server_module.lua")),
        "lua_se_client_module" => Some(include_str!("../../resources/templates/se_client_module.lua")),
        "lua_se_shared_module" => Some(include_str!("../../resources/templates/se_shared_module.lua")),
        "mcm_blueprint" => Some(include_str!("../../resources/templates/mcm_blueprint.json")),
        _ => None,
    }
}

/// Validate and resolve a relative `file_path` against the mod root directory.
/// Rejects path-traversal attempts and paths that escape `mod_path`.
fn resolve_script_path(mod_path: &str, file_path: &str) -> Result<PathBuf, String> {
    if file_path.contains("..") || file_path.contains('\0') {
        return Err("Invalid path: traversal not allowed".into());
    }
    let root = PathBuf::from(mod_path);
    if !root.is_dir() {
        return Err(format!("Mod path is not a directory: {mod_path}"));
    }
    let full = root.join(file_path);
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("Canonicalize mod root: {e}"))?;
    // For existing paths canonicalize directly; for new paths check nearest ancestor.
    let check = if full.exists() {
        full.canonicalize()
            .map_err(|e| format!("Canonicalize: {e}"))?
    } else {
        let mut ancestor = full.parent().map(Path::to_path_buf);
        while let Some(ref a) = ancestor {
            if a.exists() {
                break;
            }
            ancestor = a.parent().map(Path::to_path_buf);
        }
        ancestor
            .unwrap_or_else(|| root.clone())
            .canonicalize()
            .map_err(|e| format!("Canonicalize ancestor: {e}"))?
    };
    if !check.starts_with(&canon_root) {
        return Err("Path escapes mod root".into());
    }
    Ok(full)
}

/// Render a template with `{{VAR}}` substitution and write it to disk.
/// Returns `Ok(true)` if created, `Ok(false)` if the file already exists (skipped).
fn write_template_file(
    mod_path: &str,
    file_path: &str,
    template_id: &str,
    variables: &std::collections::HashMap<String, String>,
) -> Result<bool, String> {
    let full = resolve_script_path(mod_path, file_path)?;
    if full.exists() {
        return Ok(false);
    }
    let template = get_script_template(template_id)
        .ok_or_else(|| format!("Unknown template: {template_id}"))?;
    let mut content = template.to_string();
    for (key, value) in variables {
        content = content.replace(&format!("{{{{{key}}}}}"), value);
    }
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Create directories: {e}"))?;
    }
    std::fs::write(&full, content.as_bytes()).map_err(|e| format!("Write file: {e}"))?;
    Ok(true)
}

/// Recursively collect files from a directory into `out`.
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<ScriptFileInfo>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("Read dir: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Dir entry: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| format!("Strip prefix: {e}"))?
                .to_string_lossy()
                .replace('\\', "/");
            let size = path.metadata().map(|m| m.len() as i64).unwrap_or(0);
            out.push(ScriptFileInfo {
                path: rel,
                size,
                is_new: false,
                is_modified: false,
            });
        }
    }
    Ok(())
}

/// Rename (move) a script file on disk within the mod directory.
/// Both old and new paths are validated against path traversal via `resolve_script_path`.
#[tauri::command]
pub async fn cmd_script_rename(
    mod_path: String,
    old_path: String,
    new_path: String,
) -> Result<bool, AppError> {
    blocking(move || {
        let old_full = resolve_script_path(&mod_path, &old_path)?;
        let new_full = resolve_script_path(&mod_path, &new_path)?;

        if !old_full.is_file() {
            return Err(format!("Source file does not exist: {old_path}"));
        }
        if new_full.exists() {
            return Err(format!("Destination already exists: {new_path}"));
        }
        if let Some(parent) = new_full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Create directories: {e}"))?;
        }
        std::fs::rename(&old_full, &new_full).map_err(|e| format!("Rename file: {e}"))?;
        Ok(true)
    })
    .await
}

/// Read a script file from disk.
#[tauri::command]
pub async fn cmd_script_read(
    mod_path: String,
    file_path: String,
) -> Result<Option<String>, AppError> {
    blocking(move || {
        let full = resolve_script_path(&mod_path, &file_path)?;
        if !full.is_file() {
            return Ok(None);
        }
        std::fs::read_to_string(&full)
            .map(Some)
            .map_err(|e| format!("Read script: {e}"))
    })
    .await
}

/// Write a script file to disk (creates parent directories as needed).
#[tauri::command]
pub async fn cmd_script_write(
    mod_path: String,
    file_path: String,
    content: String,
) -> Result<bool, AppError> {
    blocking(move || {
        let full = resolve_script_path(&mod_path, &file_path)?;
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Create directories: {e}"))?;
        }
        std::fs::write(&full, content.as_bytes()).map_err(|e| format!("Write script: {e}"))?;
        Ok(true)
    })
    .await
}

/// Delete a script file from disk.
/// Returns `false` if the file does not exist.
#[tauri::command]
pub async fn cmd_script_delete(mod_path: String, file_path: String) -> Result<bool, AppError> {
    blocking(move || {
        let full = resolve_script_path(&mod_path, &file_path)?;
        if !full.is_file() {
            return Ok(false);
        }
        std::fs::remove_file(&full).map_err(|e| format!("Delete script: {e}"))?;
        Ok(true)
    })
    .await
}

/// List script files under a prefix on disk.
#[tauri::command]
pub async fn cmd_script_list(
    mod_path: String,
    prefix: Option<String>,
) -> Result<Vec<ScriptFileInfo>, AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        if !root.is_dir() {
            return Err(format!("Mod path is not a directory: {mod_path}"));
        }
        let walk_dir = if let Some(ref pfx) = prefix {
            root.join(pfx)
        } else {
            root.clone()
        };
        if !walk_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut files = Vec::new();
        collect_files(&root, &walk_dir, &mut files)?;
        Ok(files)
    })
    .await
}

/// Render a template with `{{VAR_NAME}}` substitution and write to disk.
#[tauri::command]
pub async fn cmd_script_create_from_template(
    mod_path: String,
    file_path: String,
    template_id: String,
    variables: std::collections::HashMap<String, String>,
) -> Result<bool, AppError> {
    blocking(move || {
        let full = resolve_script_path(&mod_path, &file_path)?;
        if full.exists() {
            return Err(format!("File already exists: {file_path}"));
        }
        let template = get_script_template(&template_id)
            .ok_or_else(|| format!("Unknown template: {template_id}"))?;
        let mut content = template.to_string();
        for (key, value) in &variables {
            content = content.replace(&format!("{{{{{key}}}}}"), value);
        }
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Create directories: {e}"))?;
        }
        std::fs::write(&full, content.as_bytes())
            .map_err(|e| format!("Write template file: {e}"))?;
        Ok(true)
    })
    .await
}

/// Sanitize a string to a valid Lua identifier (replace non-alphanumeric with `_`, strip leading digits).
fn sanitize_lua_identifier(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    // Strip leading digits/underscores so the result starts with a letter
    let trimmed = out.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    if trimmed.is_empty() {
        "ModTable".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Scaffold the SE directory structure on disk.
/// Creates Config.json and optionally BootstrapServer.lua / BootstrapClient.lua.
#[tauri::command]
pub async fn cmd_scaffold_se_structure(
    mod_path: String,
    mod_folder: String,
    include_server: bool,
    include_client: bool,
) -> Result<Vec<String>, AppError> {
    blocking(move || {
        let mod_table = sanitize_lua_identifier(&mod_folder);
        let base = format!("Mods/{mod_folder}/ScriptExtender");
        let mut created: Vec<String> = Vec::new();

        let mut vars = std::collections::HashMap::new();
        vars.insert("MOD_TABLE".to_string(), mod_table.clone());
        vars.insert("MOD_NAME".to_string(), mod_folder.clone());
        vars.insert("VAR_NAME".to_string(), mod_table.clone());

        // 1. Config.json
        let config_path = format!("{base}/Config.json");
        if write_template_file(&mod_path, &config_path, "se_config", &vars)? {
            created.push(config_path);
        }

        // 2. Server bootstrap
        if include_server {
            vars.insert("FILE_NAME".to_string(), "BootstrapServer.lua".to_string());
            let server_path = format!("{base}/Lua/BootstrapServer.lua");
            if write_template_file(&mod_path, &server_path, "se_bootstrap_server", &vars)? {
                created.push(server_path);
            }
        }

        // 3. Client bootstrap
        if include_client {
            vars.insert("FILE_NAME".to_string(), "BootstrapClient.lua".to_string());
            let client_path = format!("{base}/Lua/BootstrapClient.lua");
            if write_template_file(&mod_path, &client_path, "se_bootstrap_client", &vars)? {
                created.push(client_path);
            }
        }

        Ok(created)
    })
    .await
}

/// Scaffold the Khonsu Scripts/ directory structure on disk.
/// Creates a starter `.khn` condition file.
#[tauri::command]
pub async fn cmd_scaffold_khonsu_structure(
    mod_path: String,
    mod_folder: String,
) -> Result<Vec<String>, AppError> {
    blocking(move || {
        let base = format!("Mods/{mod_folder}/Scripts/thoth/helpers");
        let mut created: Vec<String> = Vec::new();

        let mut vars = std::collections::HashMap::new();
        vars.insert("MOD_NAME".to_string(), mod_folder.clone());
        vars.insert("FILE_NAME".to_string(), "basic_condition.khn".to_string());

        let file_path = format!("{base}/basic_condition.khn");
        if write_template_file(&mod_path, &file_path, "khonsu_basic_condition", &vars)? {
            created.push(file_path);
        }

        Ok(created)
    })
    .await
}

/// Scaffold the Osiris Story/RawFiles/Goals/ directory structure on disk.
/// Creates a starter goal file.
#[tauri::command]
pub async fn cmd_scaffold_osiris_structure(
    mod_path: String,
    mod_folder: String,
) -> Result<Vec<String>, AppError> {
    blocking(move || {
        let base = format!("Mods/{mod_folder}/Story/RawFiles/Goals");
        let mut created: Vec<String> = Vec::new();

        let mut vars = std::collections::HashMap::new();
        vars.insert("MOD_NAME".to_string(), mod_folder.clone());
        vars.insert("FILE_NAME".to_string(), "MyGoal.txt".to_string());
        vars.insert("GOAL_NAME".to_string(), "MyGoal".to_string());

        let file_path = format!("{base}/MyGoal.txt");
        if write_template_file(&mod_path, &file_path, "osiris_basic_goal", &vars)? {
            created.push(file_path);
        }

        Ok(created)
    })
    .await
}

// ── External template support ──

/// Info about an external template file discovered in a user-defined template folder.
#[derive(serde::Serialize, Clone)]
pub struct ExternalTemplateInfo {
    /// Unique identifier: `ext:<category>/<filename>`
    pub id: String,
    /// Display label (filename without extension)
    pub label: String,
    /// Section category this template belongs to (e.g. "lua-se", "anubis")
    pub category: String,
    /// File extension including dot (e.g. ".lua", ".ann")
    pub extension: String,
    /// Absolute path to the template source file
    pub source_path: String,
}

/// Map user-facing subdirectory names to section category keys.
fn map_subdirectory_to_category(dir_name: &str) -> Option<&'static str> {
    match dir_name.to_lowercase().as_str() {
        "lua" | "lua-se" | "scriptextender" => Some("lua-se"),
        "osiris" => Some("osiris"),
        "khonsu" => Some("khonsu"),
        "anubis" => Some("anubis"),
        "constellations" => Some("constellations"),
        "json" => Some("lua-se"),
        "yaml" => Some("lua-se"),
        _ => None,
    }
}

/// Known template file extensions.
fn is_template_extension(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        "lua"
            | "txt"
            | "khn"
            | "anc"
            | "ann"
            | "anm"
            | "clc"
            | "cln"
            | "clm"
            | "json"
            | "yaml"
            | "xml"
    )
}

/// Scan a user-defined template folder for template files organized by category subdirectories.
#[tauri::command]
pub async fn cmd_list_external_templates(
    folder_path: String,
) -> Result<Vec<ExternalTemplateInfo>, AppError> {
    blocking(move || {
        let root = std::path::Path::new(&folder_path);
        if !root.is_dir() {
            return Ok(Vec::new());
        }

        let mut templates = Vec::new();

        // Scan immediate subdirectories only (one level deep)
        let entries =
            std::fs::read_dir(root).map_err(|e| format!("Failed to read template folder: {e}"))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };

            let category = match map_subdirectory_to_category(&dir_name) {
                Some(c) => c,
                None => continue,
            };

            // Scan files in this category subdirectory
            let sub_entries = match std::fs::read_dir(&path) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for sub_entry in sub_entries.flatten() {
                let file_path = sub_entry.path();
                if !file_path.is_file() {
                    continue;
                }

                let ext = match file_path.extension().and_then(|e| e.to_str()) {
                    Some(e) if is_template_extension(e) => e.to_string(),
                    _ => continue,
                };

                let file_name = match file_path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                let label = file_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&file_name)
                    .to_string();

                let abs_path = match file_path.canonicalize() {
                    Ok(p) => p.to_string_lossy().to_string(),
                    Err(_) => file_path.to_string_lossy().to_string(),
                };

                templates.push(ExternalTemplateInfo {
                    id: format!("ext:{dir_name}/{file_name}"),
                    label,
                    category: category.to_string(),
                    extension: format!(".{ext}"),
                    source_path: abs_path,
                });
            }
        }

        // Sort by category then label for consistent ordering
        templates.sort_by(|a, b| a.category.cmp(&b.category).then(a.label.cmp(&b.label)));

        Ok(templates)
    })
    .await
}

/// Create a file from an external template. Reads the template source file,
/// applies variable substitution, and writes the result to the target path.
#[tauri::command]
pub async fn cmd_create_from_external_template(
    mod_path: String,
    file_path: String,
    source_path: String,
    variables: std::collections::HashMap<String, String>,
) -> Result<bool, AppError> {
    blocking(move || {
        let full = resolve_script_path(&mod_path, &file_path)?;
        if full.exists() {
            return Err(format!("File already exists: {file_path}"));
        }

        // Validate source path exists and is a file
        let source = std::path::Path::new(&source_path);
        if !source.is_file() {
            return Err(format!("Template source not found: {source_path}"));
        }

        // Read template content
        let mut content =
            std::fs::read_to_string(source).map_err(|e| format!("Failed to read template: {e}"))?;

        // Apply variable substitution
        for (key, value) in &variables {
            content = content.replace(&format!("{{{{{key}}}}}"), value);
        }

        // Create parent directories and write
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Create directories: {e}"))?;
        }
        std::fs::write(&full, content.as_bytes()).map_err(|e| format!("Write file: {e}"))?;

        Ok(true)
    })
    .await
}

/// Validate an MCM blueprint JSON string and return diagnostics.
#[tauri::command]
pub async fn cmd_validate_mcm_blueprint(
    content: String,
) -> Result<Vec<crate::validation::mcm_blueprint::Diagnostic>, AppError> {
    blocking(move || {
        Ok::<_, String>(crate::validation::mcm_blueprint::validate_mcm_blueprint(
            &content,
        ))
    })
    .await
}

// ── Unified script validation ──

/// Unified diagnostic type returned by `cmd_validate_script`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScriptDiagnostic {
    pub line: usize,
    pub message: String,
    pub severity: ScriptDiagnosticSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub enum ScriptDiagnosticSeverity {
    Error,
    Warning,
    Info,
}

/// Validate script content based on its language/file type.
///
/// Dispatches to the appropriate validator:
/// - `"json"` with filename containing `MCM_blueprint` → MCM blueprint validation
/// - `"json"` → basic JSON parse check
/// - `"yaml"` → basic YAML parse check
/// - `"xml"` → basic XML parse check
/// - `"osiris"` / `"txt"` → Osiris goal structural validation
/// - `"lua"` → empty (no Lua validator yet)
#[tauri::command]
pub async fn cmd_validate_script(
    project_path: Option<String>,
    file_path: String,
    language: String,
    content: String,
) -> Result<Vec<ScriptDiagnostic>, AppError> {
    blocking(move || {
        let mut diags = match language.as_str() {
            "json" => {
                if file_path.contains("MCM_blueprint") {
                    let mcm_diags =
                        crate::validation::mcm_blueprint::validate_mcm_blueprint(&content);
                    mcm_diags
                        .into_iter()
                        .map(|d| ScriptDiagnostic {
                            line: d.line,
                            message: d.message,
                            severity: match d.severity {
                                crate::validation::mcm_blueprint::DiagnosticSeverity::Error => {
                                    ScriptDiagnosticSeverity::Error
                                }
                                crate::validation::mcm_blueprint::DiagnosticSeverity::Warning => {
                                    ScriptDiagnosticSeverity::Warning
                                }
                                crate::validation::mcm_blueprint::DiagnosticSeverity::Info => {
                                    ScriptDiagnosticSeverity::Info
                                }
                            },
                            source: None,
                        })
                        .collect()
                } else {
                    validate_json_parse(&content)
                }
            }
            "yaml" => validate_yaml_parse(&content),
            "xml" => validate_xml_parse(&content),
            "osiris" | "txt" => {
                let osiris_diags = crate::validation::osiris::validate_osiris_goal(&content);
                osiris_diags
                    .into_iter()
                    .map(|d| ScriptDiagnostic {
                        line: d.line,
                        message: d.message,
                        severity: match d.severity {
                            crate::validation::osiris::DiagnosticSeverity::Error => {
                                ScriptDiagnosticSeverity::Error
                            }
                            crate::validation::osiris::DiagnosticSeverity::Warning => {
                                ScriptDiagnosticSeverity::Warning
                            }
                            crate::validation::osiris::DiagnosticSeverity::Info => {
                                ScriptDiagnosticSeverity::Info
                            }
                        },
                        source: None,
                    })
                    .collect()
            }
            "lua" => Vec::new(),
            _ => Vec::new(),
        };

        // Apply custom linters if a project path is provided
        if let Some(ref proj) = project_path {
            let linters = crate::validation::custom_linters::load_custom_linters(proj);
            if !linters.is_empty() {
                let config = crate::validation::custom_linters::load_lint_config(proj);
                let custom_diags = crate::validation::custom_linters::apply_custom_linters(
                    &content, &language, &file_path, &linters, &config,
                );
                for cd in custom_diags {
                    diags.push(ScriptDiagnostic {
                        line: cd.line,
                        message: cd.message,
                        severity: match cd.severity {
                            crate::validation::custom_linters::DiagnosticSeverity::Error => {
                                ScriptDiagnosticSeverity::Error
                            }
                            crate::validation::custom_linters::DiagnosticSeverity::Warning => {
                                ScriptDiagnosticSeverity::Warning
                            }
                            crate::validation::custom_linters::DiagnosticSeverity::Info => {
                                ScriptDiagnosticSeverity::Info
                            }
                        },
                        source: Some(cd.source),
                    });
                }
            }
        }

        Ok(diags)
    })
    .await
}

/// List loaded custom linter modules for a project.
#[tauri::command]
pub async fn cmd_list_custom_linters(
    project_path: String,
) -> Result<Vec<crate::validation::custom_linters::LinterModuleInfo>, AppError> {
    blocking(move || {
        let modules = crate::validation::custom_linters::load_custom_linters(&project_path);
        Ok(modules
            .into_iter()
            .map(|m| crate::validation::custom_linters::LinterModuleInfo {
                filename: m.filename,
                name: m.name,
                languages: m.languages,
                rule_count: m.rules.len(),
            })
            .collect())
    })
    .await
}

/// Basic JSON parse validation. Returns a single error diagnostic on parse failure.
fn validate_json_parse(content: &str) -> Vec<ScriptDiagnostic> {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(_) => Vec::new(),
        Err(e) => {
            vec![ScriptDiagnostic {
                line: e.line(),
                message: format!("Invalid JSON: {e}"),
                severity: ScriptDiagnosticSeverity::Error,
                source: None,
            }]
        }
    }
}

/// Basic YAML parse validation using serde-saphyr.
fn validate_yaml_parse(content: &str) -> Vec<ScriptDiagnostic> {
    match serde_saphyr::from_str::<serde_json::Value>(content) {
        Ok(_) => Vec::new(),
        Err(e) => {
            vec![ScriptDiagnostic {
                line: 1,
                message: format!("Invalid YAML: {e}"),
                severity: ScriptDiagnosticSeverity::Error,
                source: None,
            }]
        }
    }
}

/// Basic XML parse validation using quick-xml.
fn validate_xml_parse(content: &str) -> Vec<ScriptDiagnostic> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(content);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                let pos = reader.buffer_position() as usize;
                let line = content
                    .get(..pos)
                    .map_or(1, |s| s.matches('\n').count() + 1);
                return vec![ScriptDiagnostic {
                    line,
                    message: format!("Invalid XML: {e}"),
                    severity: ScriptDiagnosticSeverity::Error,
                    source: None,
                }];
            }
        }
        buf.clear();
    }
    Vec::new()
}
