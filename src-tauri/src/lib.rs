pub mod allspark;
pub mod commands;
pub mod converters;
pub mod db_manager;
pub mod error;
pub mod export;
#[cfg(feature = "git-integration")]
pub mod git;
pub mod logging;
pub mod models;
pub mod pak;
pub mod parsers;
pub mod platform;
pub mod reference_db;
pub mod schema;
pub mod script;
pub mod serializers;
pub mod validation;

use commands::generate::{
    detect_anchors, generate_json, generate_json_preview, generate_yaml, generate_yaml_preview,
};
use commands::mod_import::ModProcessResult;
use commands::paths;
use commands::rediff;
use commands::scan::scan_mod;
use error::AppError;
use models::*;
use parsers::lsx::parse_lsx_file;
use parsers::lsx_yaml::yaml_to_lsx_entries;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
#[cfg(test)]
use ts_rs::TS;
use walkdir::WalkDir;

use validation::{
    check_file_size, validate_folder_name, MAX_CONFIG_FILE_SIZE, MAX_LOCA_FILE_SIZE,
    MAX_LSX_FILE_SIZE, MAX_MOD_FILE_SIZE,
};

/// Check if a file extension is a supported data file (.lsx or .yaml).
fn is_data_file_ext(ext: &std::ffi::OsStr) -> bool {
    ext == "lsx" || ext == "yaml"
}

/// Parse a data file (.lsx or .yaml) into LsxEntry objects.
fn parse_data_file(path: &std::path::Path, content: &str) -> Result<Vec<LsxEntry>, String> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("yaml") => yaml_to_lsx_entries(content),
        _ => parse_lsx_file(content),
    }
}

/// P-08: Run a synchronous closure on the blocking thread pool and unify the
/// panic-message format. Replaces `tokio::task::spawn_blocking(…).await.map_err(…)?`
/// boilerplate that was repeated ~18 times.
/// Uses a default 120s timeout.
pub(crate) async fn blocking<T, F>(f: F) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    blocking_with_timeout(std::time::Duration::from_secs(120), f).await
}

/// Like `blocking`, but with an explicit timeout duration.
pub(crate) async fn blocking_with_timeout<T, F>(
    timeout: std::time::Duration,
    f: F,
) -> Result<T, AppError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    match tokio::time::timeout(timeout, tokio::task::spawn_blocking(f)).await {
        Ok(join_result) => join_result
            .map_err(|e| AppError::task_panicked(e.to_string()))?
            .map_err(AppError::from),
        Err(_) => Err(AppError::timeout(format!(
            "Operation timed out after {}s",
            timeout.as_secs()
        ))),
    }
}

/// Lightweight entry info for combobox population.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(Debug, TS))]
#[cfg_attr(test, ts(export))]
pub struct VanillaEntryInfo {
    pub uuid: String,
    pub display_name: String,
    /// The LSX node_id, e.g. "SpellList", "ActionResourceDefinition".
    pub node_id: String,
    /// Optional hex color value (e.g. "#RRGGBBAA") for color swatch display.
    /// Present for ColorDefinition, CharacterCreation color entries, etc.
    pub color: Option<String>,
    /// Optional parent GUID (e.g. ParentGuid for Race entries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_guid: Option<String>,
    /// Optional loca handle from a `Text` (TranslatedString) attribute.
    /// Present for TooltipExtraTexts, TooltipUpcastDescriptions, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_handle: Option<String>,
}

/// Describes a data section available in the reference DB.
/// Returned by `cmd_list_available_sections` so the frontend can build its
/// section list dynamically instead of relying on a hardcoded enum.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct SectionInfo {
    /// The region ID (e.g. "Races", "CharacterCreationEyeColors").
    pub region_id: String,
    /// Table name in the DB (e.g. "lsx__Races").
    pub table_name: String,
    /// The node ID within the region (may differ from region_id for multi-node regions).
    pub node_id: String,
    /// Source type: "lsx", "stats", "loca", "equipment", "valuelist", "modifier".
    pub source_type: String,
    /// Number of rows in this table.
    pub row_count: i32,
    /// Primary key column name (e.g. "UUID", "MapKey", "_entry_name").
    pub pk_column: String,
    /// Child tables linked via junction tables.
    pub children: Vec<ChildTableInfo>,
}

/// Describes a parent→child junction relationship in the reference DB.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ChildTableInfo {
    /// Junction table name (e.g. "lsx__Races__to__Race").
    pub junction_table: String,
    /// Child data table name (e.g. "lsx__Race").
    pub child_table: String,
    /// Child node ID (e.g. "Race").
    pub child_node_id: String,
    /// Number of rows in the child table.
    pub child_row_count: i32,
}

/// T2-2 / IPC-06: Paginated response wrapper for large collection commands.
/// When `limit` is 0 (default), all items are returned (backward-compatible).
#[derive(serde::Serialize, Clone)]
// PaginatedResponse<T> is hand-maintained in TS (generic bounds incompatible with ts-rs Dummy)
pub struct PaginatedResponse<T: serde::Serialize + Clone> {
    pub items: Vec<T>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

impl<T: serde::Serialize + Clone> PaginatedResponse<T> {
    fn from_vec(mut all: Vec<T>, offset: Option<usize>, limit: Option<usize>) -> Self {
        let total = all.len();
        let off = offset.unwrap_or(0).min(total);
        let lim = limit.unwrap_or(0); // 0 = unlimited

        if off > 0 {
            all = all.into_iter().skip(off).collect();
        }
        if lim > 0 {
            all.truncate(lim);
        }

        Self {
            items: all,
            total,
            offset: off,
            limit: lim,
        }
    }
}

/// CQ-016: Wrapper that carries parse warnings alongside paginated data.
/// Surfaces XML/YAML parse failures instead of silently dropping entries.
#[derive(serde::Serialize, Clone)]
pub struct ParseResultWithWarnings<T: serde::Serialize + Clone> {
    pub data: PaginatedResponse<T>,
    pub warnings: Vec<String>,
}

/// Lightweight stat entry info for combobox population.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct StatEntryInfo {
    pub name: String,
    pub entry_type: String,
}

/// A key → list-of-values entry from ValueLists.txt.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ValueListInfo {
    pub key: String,
    pub values: Vec<String>,
}

/// Convert BG3's #AARRGGBB hex color string to CSS #RRGGBBAA format.
/// Returns None if the input is not a valid 9-character hex color.
pub(crate) fn parse_bgra_color(value: &str) -> Option<String> {
    let v = value.trim();
    if v.starts_with('#') && v.len() == 9 && v.is_ascii() {
        // #AARRGGBB → #RRGGBBAA
        let aa = &v[1..3];
        let rr = &v[3..5];
        let gg = &v[5..7];
        let bb = &v[7..9];
        Some(format!("#{rr}{gg}{bb}{aa}"))
    } else {
        None
    }
}

#[tauri::command]
async fn cmd_scan_mod(
    app: tauri::AppHandle,
    mod_path: String,
    extra_scan_paths: Option<Vec<String>>,
    is_primary: Option<bool>,
) -> Result<ScanResult, AppError> {
    blocking(move || {
        let db_paths = db_manager::ensure_schema_dbs(&app)
            .map_err(|e| format!("DB paths not available: {e}"))?;
        let base_db = &db_paths.base;
        if !base_db.exists() {
            return Err(format!("Reference DB not found at {}", base_db.display()));
        }
        let result = scan_mod(&mod_path, base_db, &extra_scan_paths.unwrap_or_default())?;

        // Post-scan DB ingest: additional mods → ref_mods.
        // Primary mod staging is handled by the frontend via
        // rehydrateStaging() (recreate + populate) to avoid
        // _sources UNIQUE constraint violations on re-scan.
        if !is_primary.unwrap_or(false) {
            let mod_dir = std::path::Path::new(&mod_path);
            let mod_name = result.mod_meta.name.clone();
            let options = reference_db::BuildOptions::default();

            if db_paths.mods.exists() {
                match reference_db::populate_mods_db(mod_dir, &mod_name, &db_paths.mods, &options) {
                    Ok(s) => tracing::info!(
                        rows = s.total_rows,
                        "Populated ref_mods DB from additional mod"
                    ),
                    Err(e) => tracing::warn!(error = %e, "Failed to populate ref_mods DB"),
                }
            }
        }

        Ok(result)
    })
    .await
}

/// List all populated sections from the reference DB. The frontend uses this
/// to build its section list dynamically instead of relying on a hardcoded enum.
#[tauri::command]
async fn cmd_list_available_sections(app: tauri::AppHandle) -> Result<Vec<SectionInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_available_sections(&db_paths.base)
    })
    .await
}

/// Query entries for any section by region_id. Unified replacement for
/// cmd_get_vanilla_entries + cmd_get_entries_by_folder.
/// Aggregates from ref_base first, then merges in any additional entries from
/// ref_honor (Honor Mode data) and ref_mods (additional loaded mods), deduped by UUID.
#[tauri::command]
async fn cmd_query_section_entries(
    app: tauri::AppHandle,
    region_id: String,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<PaginatedResponse<VanillaEntryInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        let mut entries =
            reference_db::queries::query_section_entries(&db_paths.base, &region_id, None, None)?;

        // Merge additional entries from ref_honor (Honor Mode) and ref_mods
        let mut seen_uuids: std::collections::HashSet<String> =
            entries.iter().map(|e| e.uuid.clone()).collect();
        for extra_db in [&db_paths.honor, &db_paths.mods] {
            if extra_db.is_file() {
                if let Ok(extra) =
                    reference_db::queries::query_section_entries(extra_db, &region_id, None, None)
                {
                    for e in extra {
                        if seen_uuids.insert(e.uuid.clone()) {
                            entries.push(e);
                        }
                    }
                }
            }
        }

        Ok(PaginatedResponse::from_vec(entries, offset, limit))
    })
    .await
}

#[tauri::command]
async fn cmd_get_vanilla_entries(
    app: tauri::AppHandle,
    section: String,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<PaginatedResponse<VanillaEntryInfo>, AppError> {
    blocking(move || {
        let cf_section =
            Section::parse_name(&section).ok_or_else(|| format!("Unknown section: {section}"))?;
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        let entries = reference_db::queries::query_vanilla_entries(
            &db_paths.base,
            &cf_section,
            limit,
            offset,
        )?;
        Ok(PaginatedResponse::from_vec(entries, offset, limit))
    })
    .await
}

/// Info about a single list entry and its items.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ListItemsInfo {
    pub uuid: String,
    pub node_id: String,
    pub display_name: String,
    /// The item strings contained in this list (e.g. Spell IDs, Passive IDs).
    pub items: Vec<String>,
    /// Which attribute key the items came from (e.g. "Spells", "Passives").
    pub item_key: String,
}

/// ActionResource metadata used by stats cost editors.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct CostResourceInfo {
    pub name: String,
    pub display_name: String,
    pub max_level: i32,
    /// Either "resource" or "group".
    pub kind: String,
}

/// Look up the items contained in a vanilla List entry by UUID.
#[tauri::command]
async fn cmd_get_list_items(
    app: tauri::AppHandle,
    uuids: Vec<String>,
) -> Result<Vec<ListItemsInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_list_items(&db_paths.base, &uuids)
    })
    .await
}

/// Load ActionResource definitions/groups with the fields needed by stats cost editors.
#[tauri::command]
async fn cmd_get_cost_resources(app: tauri::AppHandle) -> Result<Vec<CostResourceInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;

        let mut merged = std::collections::BTreeMap::<String, CostResourceInfo>::new();

        for item in reference_db::queries::query_cost_resources(&db_paths.base)? {
            merged.insert(item.name.to_lowercase(), item);
        }

        for item in reference_db::queries::query_cost_resources(&db_paths.mods)? {
            // ref_mods should override ref_base for duplicate names.
            merged.insert(item.name.to_lowercase(), item);
        }

        Ok(merged.into_values().collect())
    })
    .await
}

#[tauri::command]
async fn cmd_generate_config(
    entries: Vec<SelectedEntry>,
    options: SerializeOptions,
) -> Result<String, AppError> {
    match options.format {
        OutputFormat::Yaml => Ok(generate_yaml(&entries, options.include_comments)),
        OutputFormat::Json => Ok(generate_json(&entries)?),
    }
}

#[tauri::command]
async fn cmd_preview_config(
    entries: Vec<SelectedEntry>,
    manual_entries: Vec<ManualEntry>,
    auto_entry_overrides: HashMap<String, HashMap<String, String>>,
    format: String,
    include_comments: bool,
    enable_section_comments: bool,
    enable_entry_comments: bool,
) -> Result<String, AppError> {
    blocking(move || match format.as_str() {
        "json" => generate_json_preview(&entries, &manual_entries, &auto_entry_overrides),
        _ => Ok(generate_yaml_preview(
            &entries,
            &manual_entries,
            &auto_entry_overrides,
            include_comments,
            enable_section_comments,
            enable_entry_comments,
        )),
    })
    .await
}

#[tauri::command]
async fn cmd_save_config(
    content: String,
    output_path: String,
    backup: bool,
) -> Result<(), AppError> {
    let output_path = output_path.trim().to_string();
    let path = PathBuf::from(&output_path);

    // Reject NTFS Alternate Data Streams (e.g. "config.yaml:hidden")
    if output_path.contains(':') && !output_path.starts_with("\\\\?\\") {
        // Allow drive letter prefix (C:\...) but reject ADS
        let after_drive = if output_path.len() >= 2 && output_path.as_bytes()[1] == b':' {
            &output_path[2..]
        } else {
            &output_path
        };
        if after_drive.contains(':') {
            return Err(AppError::security(
                "Invalid path: NTFS Alternate Data Streams not allowed",
            ));
        }
    }

    // Validate file extension — reject double extensions (e.g. config.yaml.exe)
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("Invalid file name")?;
    let dot_count = file_name.chars().filter(|&c| c == '.').count();
    if dot_count > 1 {
        return Err(AppError::invalid_input(
            "Invalid file name: double extensions not allowed",
        ));
    }

    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("yaml" | "json" | "txt" | "lsx" | "xml" | "xaml" | "bak") => {}
        _ => {
            return Err(AppError::invalid_input(
                "Unsupported file extension. Allowed: .yaml, .json, .txt, .lsx, .xml, .xaml",
            ))
        }
    }

    // Create parent directories
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directories: {e}"))?;
    }

    // Backup existing file
    if backup && path.exists() {
        let bak_path = path.with_extension("bak");
        fs::copy(&path, &bak_path).map_err(|e| format!("Failed to create backup: {e}"))?;
    }

    // Write new config
    fs::write(&path, content).map_err(|e| format!("Failed to write config: {e}"))?;
    Ok(())
}

#[tauri::command]
async fn cmd_rename_dir(from_path: String, to_path: String) -> Result<(), AppError> {
    let from = PathBuf::from(&from_path);
    let to = PathBuf::from(&to_path);
    if !from.is_dir() {
        return Err(AppError::not_found(format!(
            "Source directory does not exist: {from_path}"
        )));
    }
    if to.exists() {
        return Err(AppError::invalid_input(format!(
            "Target already exists: {to_path}"
        )));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create parent dirs: {e}"))?;
    }
    fs::rename(&from, &to).map_err(|e| format!("Failed to rename directory: {e}"))?;
    Ok(())
}

#[tauri::command]
async fn cmd_copy_file(source: String, destination: String) -> Result<(), AppError> {
    let src = PathBuf::from(&source);
    let dst = PathBuf::from(&destination);
    if !src.is_file() {
        return Err(AppError::not_found(format!(
            "Source file does not exist: {source}"
        )));
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| AppError::io_error(format!("Create parent dirs: {e}")))?;
    }
    fs::copy(&src, &dst).map_err(|e| AppError::io_error(format!("Copy file: {e}")))?;
    Ok(())
}

#[tauri::command]
async fn cmd_file_exists(path: String) -> bool {
    PathBuf::from(&path).is_file()
}

/// Search for an existing README file in the mod folder and its parent directory.
///
/// Checks the parent directory first (typical project root for Git repos), then
/// the mod folder itself. Tries common casing variants and both `.md` / `.txt`.
/// Returns the absolute path if found, or `None`.
#[tauri::command]
async fn cmd_find_readme(mod_path: String) -> Option<String> {
    let mod_dir = PathBuf::from(&mod_path);
    let candidates = [
        "README.md",
        "readme.md",
        "Readme.md",
        "README.txt",
        "readme.txt",
        "Readme.txt",
    ];

    // Check parent directory first (project root typically holds the readme)
    if let Some(parent) = mod_dir.parent() {
        for name in &candidates {
            let p = parent.join(name);
            if p.is_file() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }

    // Then check mod directory itself
    for name in &candidates {
        let p = mod_dir.join(name);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }

    None
}

#[tauri::command]
async fn cmd_read_text_file(path: String) -> Result<String, AppError> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(AppError::not_found(format!("File not found: {path}")));
    }
    check_file_size(&p, MAX_MOD_FILE_SIZE)?;
    fs::read_to_string(&p).map_err(|e| AppError::io_error(format!("Read file: {e}")))
}

/// List all files matching a given extension in a directory (non-recursive).
#[tauri::command]
async fn cmd_list_files_by_ext(dir_path: String, ext: String) -> Result<Vec<String>, AppError> {
    let dir = PathBuf::from(&dir_path);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let ext_lower = ext.to_lowercase();
    let mut files = Vec::new();
    let entries = fs::read_dir(&dir).map_err(|e| AppError::io_error(format!("Read dir: {e}")))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(file_ext) = path.extension() {
                if file_ext.to_string_lossy().to_lowercase() == ext_lower {
                    files.push(path.to_string_lossy().to_string());
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

#[tauri::command]
async fn cmd_write_text_file(path: String, content: String) -> Result<(), AppError> {
    let p = PathBuf::from(&path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io_error(format!("Create dirs: {e}")))?;
    }
    fs::write(&p, content.as_bytes()).map_err(|e| AppError::io_error(format!("Write file: {e}")))
}

#[tauri::command]
async fn cmd_detect_anchors(entries: Vec<SelectedEntry>, threshold: usize) -> Vec<AnchorGroup> {
    detect_anchors(&entries, threshold)
}

#[tauri::command]
async fn cmd_read_existing_config(config_path: String) -> Result<String, AppError> {
    let path = PathBuf::from(&config_path);
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("yaml" | "json") => {}
        _ => {
            return Err(AppError::invalid_input(
                "Config path must have a .yaml or .json extension",
            ))
        }
    }
    check_file_size(&path, MAX_CONFIG_FILE_SIZE).map_err(|e| format!("Config too large: {e}"))?;
    Ok(fs::read_to_string(&config_path).map_err(|e| format!("Failed to read config: {e}"))?)
}

/// A text file entry discovered within a mod directory.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ModFileEntry {
    /// Path relative to the mod root (using forward slashes).
    pub rel_path: String,
    /// File extension without the dot (e.g. "lua", "yaml", "json", "md", "txt").
    pub extension: String,
    /// File size in bytes.
    pub size: u64,
}

/// Allowed text file extensions for mod file preview.
const TEXT_FILE_EXTS: &[&str] = &[
    "md", "txt", "lua", "json", "yaml", "yml", "cfg", "xml", "xaml", "loca", "khn", "toml", "ini",
    "anc", "ann", "anm", "clc", "cln", "clm", "lsx", "lsefx",
];

/// List previewable text files within a mod directory.
/// Returns paths relative to `mod_path` with forward-slash separators.
/// `max_depth` limits directory recursion (default 10).
/// `max_files` caps the number of returned entries (default 2 000).
#[tauri::command]
async fn cmd_list_mod_files(
    mod_path: String,
    max_depth: Option<usize>,
    max_files: Option<usize>,
) -> Result<Vec<ModFileEntry>, AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        if !root.is_dir() {
            return Err(format!("Not a directory: {mod_path}"));
        }
        let depth_limit = max_depth.unwrap_or(10);
        let file_limit = max_files.unwrap_or(2_000);
        // Canonicalize root for boundary verification
        let canon_root = root
            .canonicalize()
            .map_err(|e| format!("Invalid mod path: {e}"))?;
        let mut files = Vec::new();
        // Security: do NOT follow symlinks — prevents TOCTOU race and path traversal
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .max_depth(depth_limit)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            // Reject symlinks entirely — mods should not rely on them
            if entry.file_type().is_symlink() {
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            // Verify the entry is within the mod directory boundary
            if let Ok(canon) = entry.path().canonicalize() {
                if !canon.starts_with(&canon_root) {
                    continue;
                }
            }
            let ext_lower = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .unwrap_or_default();
            if !TEXT_FILE_EXTS.contains(&ext_lower.as_str()) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(ModFileEntry {
                rel_path: rel,
                extension: ext_lower,
                size,
            });
            if files.len() >= file_limit {
                break;
            }
        }
        files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        Ok(files)
    })
    .await
}

/// Read the text content of a file within a mod directory.
/// The `rel_path` is validated to prevent path traversal.
#[tauri::command]
async fn cmd_read_mod_file(mod_path: String, rel_path: String) -> Result<String, AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        let full = root.join(&rel_path);
        // Canonicalize both to prevent path traversal attacks
        let canon_root = root
            .canonicalize()
            .map_err(|e| format!("Invalid mod path: {e}"))?;
        let canon_full = full
            .canonicalize()
            .map_err(|e| format!("File not found: {e}"))?;
        if !canon_full.starts_with(&canon_root) {
            return Err("Path traversal not allowed".to_string());
        }
        // Reject files larger than 2 MB
        check_file_size(&canon_full, MAX_MOD_FILE_SIZE)?;
        fs::read_to_string(&canon_full).map_err(|e| format!("Failed to read file: {e}"))
    })
    .await
}

/// Scan LSX files in a specific folder (e.g. ColorDefinitions, Gods, Tags) under vanilla data.
/// Returns entries with UUID and display name for combobox population.
#[tauri::command]
async fn cmd_get_entries_by_folder(
    app: tauri::AppHandle,
    folder_name: String,
) -> Result<Vec<VanillaEntryInfo>, AppError> {
    validate_folder_name(&folder_name)?;
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        let section = Section::parse_name(&folder_name)
            .or_else(|| match folder_name.as_str() {
                "ActionResourceDefinitions" => Some(Section::ActionResources),
                "ActionResourceGroupDefinitions" => Some(Section::ActionResourceGroups),
                "Spell" => Some(Section::SpellMetadata),
                "Status" => Some(Section::StatusMetadata),
                _ => Section::parse_name(&folder_name),
            })
            .ok_or_else(|| format!("Unknown folder/section: {folder_name}"))?;
        reference_db::queries::query_entries_by_folder(&db_paths.base, &section)
    })
    .await
}

/// A localized string entry: handle UUID → display text.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(Debug, TS))]
#[cfg_attr(test, ts(export))]
pub struct LocaEntry {
    pub handle: String,
    pub text: String,
}

/// CQ-016: Mod localization result with entries and parse warnings.
#[derive(serde::Serialize, Clone)]
pub struct ModLocaResult {
    pub entries: Vec<LocaEntry>,
    pub warnings: Vec<String>,
}

/// Input entry for auto-localization XML generation.
#[derive(serde::Deserialize)]
pub struct AutoLocaInput {
    pub contentuid: String,
    pub version: u32,
    pub text: String,
}

/// Generate a BG3-format localization XML string from auto-localization entries.
/// If `existing_xml` is provided, merges new entries with existing ones
/// (updates matching contentuids, appends new).
#[tauri::command]
fn cmd_generate_localization_xml(
    entries: Vec<AutoLocaInput>,
    existing_xml: Option<String>,
) -> Result<String, AppError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    use std::collections::HashMap;

    // Build a map of contentuid → (version, text) from new entries
    let mut entry_map: HashMap<String, (u32, String)> = HashMap::new();
    for e in &entries {
        entry_map.insert(e.contentuid.clone(), (e.version, e.text.clone()));
    }

    // If there's existing XML, parse it and merge
    if let Some(ref xml_str) = existing_xml {
        let mut reader = Reader::from_str(xml_str);
        let mut existing_uids: Vec<(String, u32, String)> = Vec::new();

        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) if e.name().as_ref() == b"content" => {
                    let mut uid = String::new();
                    let mut ver: u32 = 1;
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        match attr.key.as_ref() {
                            b"contentuid" => uid = String::from_utf8_lossy(&attr.value).to_string(),
                            b"version" => {
                                ver = String::from_utf8_lossy(&attr.value).parse().unwrap_or(1);
                            }
                            _ => {}
                        }
                    }
                    let text = reader.read_text(e.name()).unwrap_or_default().to_string();
                    if !uid.is_empty() {
                        existing_uids.push((uid, ver, text));
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }

        // Merge: update existing, track which new entries were merged
        let mut merged: Vec<(String, u32, String)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (uid, ver, text) in existing_uids {
            if let Some((new_ver, new_text)) = entry_map.get(&uid) {
                merged.push((uid.clone(), *new_ver, new_text.clone()));
                seen.insert(uid);
            } else {
                merged.push((uid, ver, text));
            }
        }
        // Append new entries not in existing
        for e in &entries {
            if !seen.contains(&e.contentuid) {
                merged.push((e.contentuid.clone(), e.version, e.text.clone()));
            }
        }

        // Generate merged XML
        return write_loca_xml(&merged);
    }

    // No existing XML — generate fresh
    let all: Vec<(String, u32, String)> = entries
        .into_iter()
        .map(|e| (e.contentuid, e.version, e.text))
        .collect();
    write_loca_xml(&all)
}

/// Escape XML special characters into a String buffer.
/// Handles the five predefined XML entities: & < > " '
fn xml_escape_into_loca(buf: &mut String, s: &str) {
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

/// Write localization entries as BG3-format XML.
///
/// Uses manual string building (same approach as `render_content_list` in loca_handler.rs)
/// to guarantee a well-known output format with correctly inline text content.
/// The quick_xml indented writer was intentionally avoided here because its
/// newline injection can place indent characters inside `<content>` text nodes,
/// causing DOMParser to return whitespace-only `textContent` for all entries.
fn write_loca_xml(entries: &[(String, u32, String)]) -> Result<String, AppError> {
    let mut out = String::with_capacity(entries.len() * 120 + 80);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<contentList>\n");

    for (uid, ver, text) in entries {
        out.push_str("  <content contentuid=\"");
        xml_escape_into_loca(&mut out, uid);
        out.push_str("\" version=\"");
        out.push_str(&ver.to_string());
        out.push_str("\">");
        xml_escape_into_loca(&mut out, text);
        out.push_str("</content>\n");
    }

    out.push_str("</contentList>\n");
    Ok(out)
}

/// Read localization files from the unpacked vanilla Localization directory.
/// Supports both YAML (preferred, handle→text maps) and legacy XML format.
/// Returns a map of handle UUID → localized text string, plus any parse warnings.
#[tauri::command]
async fn cmd_get_localization_map(
    app: tauri::AppHandle,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<ParseResultWithWarnings<LocaEntry>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        let entries = reference_db::queries::query_localization_map(&db_paths.base)?;
        Ok(ParseResultWithWarnings {
            data: PaginatedResponse::from_vec(entries, offset, limit),
            warnings: Vec::new(),
        })
    })
    .await
}

/// Read localization files from an unpacked mod's Localization directory.
/// Supports both YAML (preferred) and legacy XML format.
/// Returns a list of LocaEntry {handle, text} for all entries found, plus any parse warnings.
#[tauri::command]
async fn cmd_get_mod_localization(mod_path: String) -> Result<ModLocaResult, AppError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    blocking(move || {
        let path = PathBuf::from(&mod_path);

        let mut entries = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // Try consolidated localization.yaml at root level first
        let consolidated = path.join("localization.yaml");
        if consolidated.exists() && check_file_size(&consolidated, MAX_LOCA_FILE_SIZE).is_ok() {
            if let Ok(content) = fs::read_to_string(&consolidated) {
                if let Ok(map) = serde_saphyr::from_str::<std::collections::BTreeMap<String, String>>(&content) {
                    for (handle, text) in map {
                        entries.push(LocaEntry { handle, text });
                    }
                    return Ok(ModLocaResult { entries, warnings });
                }
            }
        }

        // Fallback: check multiple possible localization subdirectories
        let mut loca_dirs = vec![
            path.join("Localization"),
            path.join("English"),            // Some mods put loca directly in language folder
        ];

        // Also check Mods/*/Localization/ (standard BG3 mod layout:
        // e.g. ModRoot/Mods/ModFolder/Localization/English/*.loca.xml)
        let mods_dir = path.join("Mods");
        if mods_dir.exists() {
            if let Ok(read_dir) = fs::read_dir(&mods_dir) {
                for dir_entry in read_dir.filter_map(|e| e.ok()) {
                    if dir_entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                        let sub_loca = dir_entry.path().join("Localization");
                        if sub_loca.exists() {
                            loca_dirs.push(sub_loca);
                        }
                    }
                }
            }
        }

        let mut entries = Vec::new();

        for loca_dir in &loca_dirs {
            if !loca_dir.exists() {
                continue;
            }

            for entry in WalkDir::new(loca_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().is_file()
                        && e.path().extension().is_some_and(|ext| ext == "yaml" || ext == "xml")
                })
            {
                if check_file_size(entry.path(), MAX_LOCA_FILE_SIZE).is_err() { continue; }
                let content = match fs::read_to_string(entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let ext = entry.path().extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext == "yaml" {
                    // YAML format: handle → text map
                    if let Ok(map) = serde_saphyr::from_str::<std::collections::BTreeMap<String, String>>(&content) {
                        for (handle, text) in map {
                            entries.push(LocaEntry { handle, text });
                        }
                    }
                } else {
                    // Legacy XML format
                    let mut reader = Reader::from_str(&content);
                    let mut current_handle: Option<String> = None;
                    let file_path_display = entry.path().display().to_string();

                    loop {
                        match reader.read_event() {
                            Ok(Event::Start(ref e)) if e.name().as_ref() == b"content" => {
                                for attr in e.attributes().filter_map(|a| a.ok()) {
                                    if attr.key.as_ref() == b"contentuid" {
                                        current_handle = std::str::from_utf8(attr.value.as_ref()).ok().map(String::from);
                                    }
                                }
                            }
                            Ok(Event::Text(ref e)) => {
                                if let Some(handle) = current_handle.take() {
                                    if let Ok(text) = e.decode() {
                                        let text: String = text.to_string();
                                        if !text.is_empty() {
                                            entries.push(LocaEntry { handle, text });
                                        }
                                    }
                                }
                            }
                            Ok(Event::End(ref e)) if e.name().as_ref() == b"content" => {
                                current_handle = None;
                            }
                            Ok(Event::Eof) => break,
                            Err(e) => {
                                warnings.push(format!(
                                    "XML parse error in {file_path_display}: {e} — entries after this point may be missing"
                                ));
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(ModLocaResult { entries, warnings })
    })
    .await
}

/// Validate that divine.exe exists at the given path.
/// (Kept for potential future use; IPC command removed since no frontend workflow needs Divine.exe.)
/// Get stat entry names grouped by entry_type (e.g. "SpellData", "PassiveData", "Armor", etc.).
/// Optionally filter to a single entry_type; pass empty string to get all.
#[tauri::command]
async fn cmd_get_stat_entries(
    app: tauri::AppHandle,
    entry_type: String,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<PaginatedResponse<StatEntryInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        let entries = reference_db::queries::query_stat_entries(&db_paths.base, &entry_type)?;
        Ok(PaginatedResponse::from_vec(entries, offset, limit))
    })
    .await
}

/// Get stat entries parsed from the scanned mod (cached after `scan_mod`).
/// Returns all entry names/types from the mod's Stats .txt / .yaml files.
#[tauri::command]
async fn cmd_get_mod_stat_entries(mod_path: String) -> Result<Vec<StatEntryInfo>, AppError> {
    blocking(move || {
        use crate::commands::rediff::get_mod_stat_entries;
        Ok(get_mod_stat_entries(&mod_path))
    })
    .await
}

/// Get unique stat field names (the `data "Key" "..."` keys) from all .txt stat files.
/// Optionally filter by entry_type; pass empty string to get all field names across all types.
/// Returns sorted, deduplicated field name strings.
#[tauri::command]
async fn cmd_get_stat_field_names(
    app: tauri::AppHandle,
    entry_type: String,
) -> Result<Vec<String>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_stat_field_names(&db_paths.base, &entry_type)
    })
    .await
}

#[tauri::command]
async fn cmd_get_stat_entry_fields(
    app: tauri::AppHandle,
    entry_name: String,
    entry_type: String,
) -> Result<HashMap<String, String>, AppError> {
    blocking(move || {
        use crate::commands::rediff::get_scanned_stat_entry_fields;

        if let Some(fields) = get_scanned_stat_entry_fields(&entry_name, &entry_type) {
            return Ok(fields);
        }

        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_stat_entry_fields(&db_paths.base, &entry_name, &entry_type)
    })
    .await
}

/// Serialize Stats entries to BG3 .txt format.
#[tauri::command]
async fn cmd_write_stats(entries: Vec<StatsEntry>) -> String {
    parsers::stats_txt::write_stats_file(&entries)
}

/// Get unique ProgressionTableUUIDs from the reference DB.
/// Extracts from Progressions (TableUUID attr) and ClassDescriptions (ProgressionTableUUID attr).
/// Returns entries with the table UUID and a display name (e.g., class name).
#[tauri::command]
async fn cmd_get_progression_table_uuids(
    app: tauri::AppHandle,
) -> Result<Vec<VanillaEntryInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_progression_table_uuids(&db_paths.base)
    })
    .await
}

/// Collect unique VoiceTable UUIDs from the reference DB.
/// Voice entries have both a UUID (primary identifier) and a TableUUID
/// (the value that ClassDescriptions/Origins VoiceTableUUID references).
#[tauri::command]
async fn cmd_get_voice_table_uuids(
    app: tauri::AppHandle,
) -> Result<Vec<VanillaEntryInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_voice_table_uuids(&db_paths.base)
    })
    .await
}

/// Collect unique SelectorId string values from Progressions data.
/// Scrapes: vanilla unpacked data + additional mod paths + scanned mod path.
/// Checks consolidated Progressions.yaml first, then falls back to walking Progressions/ folders.
/// Returns a sorted Vec of `{ id, source }` objects where source is "Vanilla" or the mod folder name.
#[derive(serde::Serialize, Clone)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
struct SelectorIdInfo {
    id: String,
    source: String,
}

#[tauri::command]
async fn cmd_get_selector_ids(
    app: tauri::AppHandle,
    extra_paths: Option<Vec<String>>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<PaginatedResponse<SelectorIdInfo>, AppError> {
    blocking(move || {
        let mut id_sources: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // DB path: query vanilla SelectorIds from ref_base.sqlite
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        if let Ok(pairs) = reference_db::queries::query_selector_ids(&db_paths.base) {
            for (id, source) in pairs {
                id_sources.entry(id).or_insert(source);
            }
        }

        // Helper: extract SelectorId from LsxEntry list
        let scrape_entries =
            |entries: &[LsxEntry],
             source_name: &str,
             id_sources: &mut std::collections::HashMap<String, String>| {
                for lsx_entry in entries {
                    if let Some(attr) = lsx_entry.attributes.get("SelectorId") {
                        let v = attr.value.trim().to_string();
                        if !v.is_empty() {
                            id_sources
                                .entry(v)
                                .or_insert_with(|| source_name.to_string());
                        }
                    }
                }
            };

        // Helper: try consolidated YAML for a path, return true if found
        let try_consolidated = |base: &PathBuf,
                                source_name: &str,
                                id_sources: &mut std::collections::HashMap<String, String>|
         -> bool {
            let consolidated_path = base.join(Section::Progressions.consolidated_filename());
            if consolidated_path.exists() {
                if let Ok(cf) = parsers::lsx_yaml::read_consolidated_file(&consolidated_path) {
                    let entries = parsers::lsx_yaml::consolidated_to_lsx_entries(&cf);
                    scrape_entries(&entries, source_name, id_sources);
                    return true;
                }
            }
            false
        };

        // Helper: walk a single root looking for Progressions/ folder (fallback).
        let scrape_root =
            |root: &PathBuf,
             source_name: &str,
             id_sources: &mut std::collections::HashMap<String, String>| {
                let folder = root.join("Progressions");
                if !folder.exists() {
                    return;
                }
                for entry in WalkDir::new(&folder)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_type().is_file()
                            && e.path().extension().is_some_and(is_data_file_ext)
                    })
                {
                    if check_file_size(entry.path(), MAX_LSX_FILE_SIZE).is_err() {
                        continue;
                    }
                    let content = match fs::read_to_string(entry.path()) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    if let Ok(entries) = parse_data_file(entry.path(), &content) {
                        scrape_entries(&entries, source_name, id_sources);
                    }
                }
            };

        // Extra paths (additional mods + scanned mod)
        if let Some(extras) = extra_paths {
            for p in extras {
                let mod_base = PathBuf::from(&p);

                // SEC-03: Canonicalize to resolve any .. or relative components
                let mod_base = mod_base
                    .canonicalize()
                    .map_err(|e| format!("Failed to canonicalize extra path '{p}': {e}"))?;

                // SEC-03: Reject symlinks
                let meta = fs::symlink_metadata(&mod_base)
                    .map_err(|e| format!("Failed to read metadata for extra path '{p}': {e}"))?;
                if meta.file_type().is_symlink() {
                    return Err(format!(
                        "Extra path '{p}' is a symlink, which is not allowed for security reasons"
                    ));
                }

                let mod_name = mod_base
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Mod".to_string());
                // Try consolidated first
                if !try_consolidated(&mod_base, &mod_name, &mut id_sources) {
                    // Fallback: Direct path/Progressions/ and Public/*/Progressions/
                    scrape_root(&mod_base, &mod_name, &mut id_sources);
                    let public_dir = mod_base.join("Public");
                    if public_dir.is_dir() {
                        if let Ok(pub_entries) = fs::read_dir(&public_dir) {
                            for pub_entry in pub_entries.filter_map(|e| e.ok()) {
                                if pub_entry.path().is_dir() {
                                    scrape_root(&pub_entry.path(), &mod_name, &mut id_sources);
                                }
                            }
                        }
                    }
                }
            }
        }

        let mut result: Vec<SelectorIdInfo> = id_sources
            .into_iter()
            .map(|(id, source)| SelectorIdInfo { id, source })
            .collect();
        result.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(PaginatedResponse::from_vec(result, offset, limit))
    })
    .await
}

/// Get equipment set names from the reference DB.
#[tauri::command]
async fn cmd_get_equipment_names(app: tauri::AppHandle) -> Result<Vec<String>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_equipment_names(&db_paths.base)
    })
    .await
}

/// Get icon MapKey names from all IconUVList tables in the reference DB.
/// Used to populate the iconName: combobox descriptor for the Icon field.
#[tauri::command]
async fn cmd_get_icon_names(app: tauri::AppHandle) -> Result<Vec<String>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_icon_names(&db_paths.base)
    })
    .await
}

/// Get atlas UV data for all icons (MapKey → atlas path + UV coords).
/// Used to enable icon image previews in the Icon field combobox.
#[tauri::command]
async fn cmd_get_icon_atlas_data(
    app: tauri::AppHandle,
) -> Result<Vec<reference_db::queries::IconAtlasEntry>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_icon_atlas_data(&db_paths.base)
    })
    .await
}

/// Get atlas UV data for icons defined in the active mod's staging database.
/// Used alongside cmd_get_icon_atlas_data to show previews for mod-specific icons.
#[tauri::command]
async fn cmd_get_staging_icon_atlas_data(
    app: tauri::AppHandle,
) -> Result<Vec<reference_db::queries::IconAtlasEntry>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        reference_db::queries::query_icon_atlas_data(&db_paths.staging)
    })
    .await
}

/// Get value lists from the reference DB.
/// Optionally filter by key name; pass empty string to get all lists.
#[tauri::command]
async fn cmd_get_value_lists(
    app: tauri::AppHandle,
    list_key: String,
) -> Result<Vec<ValueListInfo>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app).map_err(|e| format!("DB paths: {e}"))?;
        let pairs = reference_db::queries::query_value_lists(&db_paths.base, &list_key)?;
        Ok(pairs
            .into_iter()
            .map(|(key, values)| ValueListInfo { key, values })
            .collect())
    })
    .await
}

/// Process a mod: stream its files into ref_mods.sqlite.
#[tauri::command]
async fn cmd_process_mod_folder(
    app: tauri::AppHandle,
    mod_path: String,
    mod_name: String,
) -> Result<ModProcessResult, AppError> {
    blocking(move || {
        let db_paths = db_manager::ensure_schema_dbs(&app)?;
        let mod_dir = std::path::Path::new(&mod_path);

        if !mod_dir.exists() {
            return Err(format!("Mod path does not exist: {mod_path}"));
        }

        let is_pak = mod_dir.is_file()
            && mod_dir
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pak"));

        let mut errors: Vec<String> = Vec::new();
        let mod_meta;
        let has_public_folder;
        let mut files_processed: usize = 0;

        if is_pak {
            // Native pak path
            mod_meta = commands::mod_import::read_meta_from_pak(mod_dir).unwrap_or_else(|e| {
                errors.push(e);
                None
            });

            let (files, has_public) = reference_db::collect_mod_files_from_pak(mod_dir, &mod_name)?;
            has_public_folder = has_public;
            files_processed = files.len();

            if !files.is_empty() {
                let options = reference_db::BuildOptions::default();
                reference_db::builder::populate_db(
                    &db_paths.mods,
                    &files,
                    mod_dir, // _unpacked_path (unused)
                    &options,
                )?;
            }
        } else if mod_dir.is_dir() {
            // Directory mod: walk on disk
            let options = reference_db::BuildOptions::default();
            match reference_db::populate_mods_db(mod_dir, &mod_name, &db_paths.mods, &options) {
                Ok(summary) => files_processed = summary.total_files,
                Err(e) => errors.push(e),
            }
            has_public_folder = mod_dir.join("Public").is_dir();
            mod_meta = paths::read_mod_meta(&mod_path).ok();
        } else {
            return Err(format!(
                "Mod path is neither a .pak file nor a directory: {mod_path}"
            ));
        }

        Ok(ModProcessResult {
            lsf_converted: files_processed,
            unpack_path: db_paths.dir.to_string_lossy().into_owned(),
            errors,
            mod_meta,
            has_public_folder,
        })
    })
    .await
}

/// Compute total disk usage (bytes) of a directory.
#[tauri::command]
async fn cmd_dir_size(dir_path: String) -> Result<u64, AppError> {
    blocking(move || paths::dir_size_bytes(&dir_path)).await
}

/// Read a mod's meta.lsx and return ModMeta.
#[tauri::command]
async fn cmd_read_mod_meta(mod_path: String) -> Result<ModMeta, AppError> {
    blocking(move || paths::read_mod_meta(&mod_path)).await
}

/// Detect valid ModFolders within a project folder.
#[tauri::command]
async fn cmd_detect_mod_folders(project_path: String) -> Result<paths::DetectResult, AppError> {
    blocking(move || paths::detect_mod_folders(&project_path)).await
}

/// Re-diff a primary mod against a different comparison source.
/// If compare_mod_path is empty, diffs against vanilla.
#[tauri::command]
async fn cmd_rediff_mod(
    app: tauri::AppHandle,
    primary_mod_path: String,
    compare_mod_path: String,
) -> Result<Vec<SectionResult>, AppError> {
    blocking(move || {
        let db_paths =
            db_manager::get_db_paths(&app).map_err(|e| format!("DB paths not available: {e}"))?;
        if !db_paths.base.exists() {
            return Err(format!(
                "Reference DB not found at {}",
                db_paths.base.display()
            ));
        }
        rediff::rediff_mod(&primary_mod_path, &compare_mod_path, &db_paths.base)
    })
    .await
}

/// List all .pak files in the BG3 Mods directory (%LOCALAPPDATA%\Larian Studios\Baldur's Gate 3\Mods).
/// Returns an empty Vec if the directory doesn't exist.
#[tauri::command]
async fn cmd_list_load_order_paks() -> Result<Vec<String>, AppError> {
    blocking(|| {
        let local_app_data = dirs::data_local_dir()
            .ok_or_else(|| "Could not determine LocalAppData directory".to_string())?;
        let mods_dir = local_app_data
            .join("Larian Studios")
            .join("Baldur's Gate 3")
            .join("Mods");
        if !mods_dir.exists() {
            return Ok(Vec::new());
        }
        let mut paks: Vec<String> = std::fs::read_dir(&mods_dir)
            .map_err(|e| format!("Failed to read BG3 Mods directory: {e}"))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("pak"))
            })
            .map(|e| e.path().to_string_lossy().into_owned())
            .collect();
        paks.sort();
        Ok(paks)
    })
    .await
}

/// Read modsettings.lsx from BG3 player profiles and return the Folder names of active mods.
/// Searches all profiles for the most recently modified modsettings.lsx.
#[tauri::command]
async fn cmd_get_active_mod_folders() -> Result<Vec<String>, AppError> {
    blocking(|| {
        let local_app_data = dirs::data_local_dir()
            .ok_or_else(|| "Could not determine LocalAppData directory".to_string())?;
        let profiles_dir = local_app_data
            .join("Larian Studios")
            .join("Baldur's Gate 3")
            .join("PlayerProfiles");
        if !profiles_dir.exists() {
            return Ok(Vec::new());
        }

        // Find the most recently modified modsettings.lsx across all profiles
        let mut best_file: Option<std::path::PathBuf> = None;
        let mut best_time: Option<std::time::SystemTime> = None;

        if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let modsettings = entry.path().join("modsettings.lsx");
                if modsettings.exists() {
                    let mtime = modsettings.metadata().and_then(|m| m.modified()).ok();
                    if best_time.is_none() || mtime > best_time {
                        best_file = Some(modsettings);
                        best_time = mtime;
                    }
                }
            }
        }

        let modsettings_path = best_file
            .ok_or_else(|| "No modsettings.lsx found in any BG3 player profile".to_string())?;

        check_file_size(&modsettings_path, MAX_CONFIG_FILE_SIZE)
            .map_err(|e| format!("modsettings.lsx too large: {e}"))?;
        let content = std::fs::read_to_string(&modsettings_path)
            .map_err(|e| format!("Failed to read modsettings.lsx: {e}"))?;

        // Parse the XML to extract Folder attributes from active mod entries
        let mut folders: Vec<String> = Vec::new();

        // modsettings.lsx has <node id="ModuleShortDesc"> entries under <node id="Mods">
        // Each has <attribute id="Folder" type="LSString" value="..." />
        use regex::Regex;
        use std::sync::LazyLock;
        static RE_FOLDER: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r#"<attribute\s+id="Folder"\s+[^>]*value="([^"]+)""#).unwrap()
        });

        // Find the Mods section and extract folders
        // The Mods section contains ModuleShortDesc nodes for each active mod
        let mut in_mods_section = false;
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.contains(r#"id="Mods""#) {
                in_mods_section = true;
            }
            if in_mods_section {
                if let Some(caps) = RE_FOLDER.captures(trimmed) {
                    let folder = caps[1].to_string();
                    // Skip the base game modules
                    if folder != "Gustav" && folder != "GustavDev" {
                        folders.push(folder);
                    }
                }
                // Stop when we exit the Mods section
                if trimmed == "</children>" && in_mods_section && !folders.is_empty() {
                    break;
                }
            }
        }

        Ok(folders)
    })
    .await
}

/// Check whether the given Game Data folder contains any of the expected vanilla .pak files
/// (Gustav.pak, GustavX.pak, Shared.pak). Returns true if at least one is found.
///
/// Accepts both the BG3 installation root (e.g. `.../Baldurs Gate 3`) and the
/// `Data/` subfolder directly.  If paks are not found at the given path, the
/// `Data/` child directory is checked as a fallback.
#[tauri::command]
async fn cmd_validate_game_data_path(game_data_path: String) -> Result<bool, AppError> {
    let dir = std::path::Path::new(&game_data_path);
    if !dir.is_dir() {
        return Ok(false);
    }
    let expected = ["Gustav.pak", "GustavX.pak", "Shared.pak"];
    // Check given directory first
    for name in &expected {
        if dir.join(name).is_file() {
            return Ok(true);
        }
    }
    // Fallback: check Data/ subfolder (user may have passed the install root)
    let data_sub = dir.join("Data");
    if data_sub.is_dir() {
        for name in &expected {
            if data_sub.join(name).is_file() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Open a file or directory path in the native OS file explorer / default application.
/// Uses `explorer.exe` on Windows, `xdg-open` on Linux, `open` on macOS.
#[tauri::command]
async fn cmd_open_path(path: String) -> Result<(), AppError> {
    let p = std::path::Path::new(&path);
    if !p.exists() {
        return Err(AppError::not_found(format!("Path does not exist: {path}")));
    }

    #[cfg(target_os = "windows")]
    {
        // explorer.exe needs backslash paths on Windows
        let win_path = path.replace('/', "\\");
        let mut cmd = std::process::Command::new("explorer.exe");
        cmd.arg(&win_path);
        cmd.spawn()
            .map_err(|e| format!("Failed to open path: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to open path: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to open path: {e}"))?;
    }

    Ok(())
}

/// Reveal a file or directory in the native OS file manager (selecting it if it's a file).
/// On Windows: `explorer.exe /select,"path"` for files, `explorer.exe "path"` for directories.
/// On macOS: `open -R "path"`. On Linux: opens the parent directory with `xdg-open`.
#[tauri::command]
async fn cmd_reveal_path(path: String) -> Result<(), AppError> {
    let p = std::path::Path::new(&path);
    if !p.exists() {
        return Err(AppError::not_found(format!("Path does not exist: {path}")));
    }

    #[cfg(target_os = "windows")]
    {
        let win_path = path.replace('/', "\\");
        let mut cmd = std::process::Command::new("explorer.exe");
        if p.is_file() {
            cmd.arg("/select,");
            cmd.arg(&win_path);
        } else {
            cmd.arg(&win_path);
        }
        cmd.spawn()
            .map_err(|e| format!("Failed to reveal path: {e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to reveal path: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        let dir = if p.is_file() {
            p.parent()
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or(path.clone())
        } else {
            path.clone()
        };
        std::process::Command::new("xdg-open")
            .arg(&dir)
            .spawn()
            .map_err(|e| format!("Failed to reveal path: {e}"))?;
    }

    Ok(())
}

/// Auto-detect the BG3 game Data folder via Windows Registry (Steam & GOG).
#[tauri::command]
async fn cmd_detect_game_data_path() -> Result<Option<String>, AppError> {
    #[cfg(target_os = "windows")]
    {
        use winreg::enums::*;
        use winreg::RegKey;

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);

        // Try Steam: "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 1086940"
        if let Ok(key) = hklm
            .open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\Steam App 1086940")
        {
            if let Ok(loc) = key.get_value::<String, _>("InstallLocation") {
                let data = std::path::Path::new(&loc).join("Data");
                if data.is_dir() {
                    return Ok(Some(data.to_string_lossy().into_owned()));
                }
            }
        }

        // Try GOG (64-bit registry view): "HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\1456460669"
        if let Ok(key) = hklm.open_subkey(r"SOFTWARE\WOW6432Node\GOG.com\Games\1456460669") {
            if let Ok(path_val) = key.get_value::<String, _>("path") {
                let data = std::path::Path::new(&path_val).join("Data");
                if data.is_dir() {
                    return Ok(Some(data.to_string_lossy().into_owned()));
                }
            }
        }

        // Try common Steam library paths
        let common_paths = [
            r"C:\Program Files (x86)\Steam\steamapps\common\Baldurs Gate 3\Data",
            r"C:\Program Files\Steam\steamapps\common\Baldurs Gate 3\Data",
            r"D:\SteamLibrary\steamapps\common\Baldurs Gate 3\Data",
            r"E:\SteamLibrary\steamapps\common\Baldurs Gate 3\Data",
        ];
        for p in &common_paths {
            if std::path::Path::new(p).is_dir() {
                return Ok(Some(p.to_string()));
            }
        }

        Ok(None)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(None)
    }
}

use serializers::lsx_writer;

/// An entry sent from the frontend for LSX preview/export.
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct LsxPreviewEntry {
    pub uuid: String,
    pub node_id: String,
    pub raw_attributes: HashMap<String, String>,
    pub raw_attribute_types: HashMap<String, String>,
    pub raw_children: HashMap<String, Vec<String>>,
}

#[tauri::command]
async fn cmd_preview_lsx(
    entries: Vec<LsxPreviewEntry>,
    region_id: String,
) -> Result<String, AppError> {
    blocking(move || {
        let lsx_entries: Vec<LsxEntry> = entries
            .iter()
            .map(|e| {
                lsx_writer::reconstruct_lsx_entry(
                    &e.uuid,
                    &e.node_id,
                    &e.raw_attributes,
                    &e.raw_attribute_types,
                    &e.raw_children,
                )
            })
            .collect();
        Ok(lsx_writer::entries_to_lsx(&lsx_entries, &region_id))
    })
    .await
}

#[tauri::command]
async fn cmd_save_lsx(
    entries: Vec<LsxPreviewEntry>,
    region_id: String,
    output_path: String,
) -> Result<(), AppError> {
    let path = PathBuf::from(&output_path);

    // Validate file extension
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("lsx") => {}
        _ => {
            return Err(AppError::invalid_input(
                "Output path must have a .lsx extension",
            ))
        }
    }

    // Create parent directories
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directories: {e}"))?;
    }

    blocking(move || {
        let lsx_entries: Vec<LsxEntry> = entries
            .iter()
            .map(|e| {
                lsx_writer::reconstruct_lsx_entry(
                    &e.uuid,
                    &e.node_id,
                    &e.raw_attributes,
                    &e.raw_attribute_types,
                    &e.raw_children,
                )
            })
            .collect();
        let xml = lsx_writer::entries_to_lsx(&lsx_entries, &region_id);
        fs::write(&path, xml).map_err(|e| format!("Failed to write LSX file: {e}"))
    })
    .await
}

/// A single file to export in a batch mod export operation.
#[derive(serde::Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ExportFileSpec {
    /// Absolute path to write the file to.
    pub output_path: String,
    /// LSX region ID (e.g. "Progressions", "Races").
    pub region_id: String,
    /// Entries to serialize into this file.
    pub entries: Vec<LsxPreviewEntry>,
}

/// Summary of a single exported file.
#[derive(serde::Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ExportedFile {
    pub path: String,
    pub entry_count: usize,
    pub bytes_written: usize,
    pub backed_up: bool,
}

/// Result of a full mod export operation.
#[derive(serde::Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ExportModResult {
    pub files: Vec<ExportedFile>,
    pub errors: Vec<String>,
    pub file_errors: HashMap<String, Vec<String>>,
}

#[tauri::command]
async fn cmd_export_mod(
    lsx_files: Vec<ExportFileSpec>,
    config_content: Option<String>,
    config_path: Option<String>,
    backup: bool,
) -> Result<ExportModResult, AppError> {
    blocking(move || {
        let mut result = ExportModResult {
            files: Vec::new(),
            errors: Vec::new(),
            file_errors: HashMap::new(),
        };

        // Pending write: (final_path, tmp_path, content, report_key, entry_count, backed_up)
        let mut pending: Vec<(PathBuf, PathBuf, String, String, usize, bool)> = Vec::new();

        // ── Phase 1: Validate, create dirs, backup, generate content ──

        // LSX files
        for spec in &lsx_files {
            let path = PathBuf::from(&spec.output_path);

            // Validate extension
            match path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref()
            {
                Some("lsx") => {}
                _ => {
                    result
                        .file_errors
                        .entry(spec.output_path.clone())
                        .or_default()
                        .push("Skipped: not a .lsx file".to_string());
                    continue;
                }
            }

            // Create parent directories
            if let Some(parent) = path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    result
                        .file_errors
                        .entry(spec.output_path.clone())
                        .or_default()
                        .push(format!("Failed to create directory: {e}"));
                    continue;
                }
            }

            // Backup existing file before any writes
            let backed_up = if backup && path.exists() {
                let bak_path = path.with_extension("lsx.bak");
                match fs::copy(&path, &bak_path) {
                    Ok(_) => true,
                    Err(e) => {
                        result
                            .file_errors
                            .entry(spec.output_path.clone())
                            .or_default()
                            .push(format!("Backup failed: {e}"));
                        continue;
                    }
                }
            } else {
                false
            };

            // Generate LSX content
            let lsx_entries: Vec<LsxEntry> = spec
                .entries
                .iter()
                .map(|e| {
                    lsx_writer::reconstruct_lsx_entry(
                        &e.uuid,
                        &e.node_id,
                        &e.raw_attributes,
                        &e.raw_attribute_types,
                        &e.raw_children,
                    )
                })
                .collect();

            let xml = lsx_writer::entries_to_lsx(&lsx_entries, &spec.region_id);
            let tmp_path = path.with_extension("lsx.cmty_tmp");
            pending.push((
                path,
                tmp_path,
                xml,
                spec.output_path.clone(),
                spec.entries.len(),
                backed_up,
            ));
        }

        // CF config file
        if let (Some(content), Some(cfg_path)) = (config_content, config_path) {
            let path = PathBuf::from(&cfg_path);

            let ext_ok = matches!(
                path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .as_deref(),
                Some("yaml" | "json")
            );

            if !ext_ok {
                result
                    .file_errors
                    .entry(cfg_path.clone())
                    .or_default()
                    .push("Skipped: not a .yaml or .json file".to_string());
            } else {
                let mut skip_config = false;

                if let Some(parent) = path.parent() {
                    if let Err(e) = fs::create_dir_all(parent) {
                        result
                            .file_errors
                            .entry(cfg_path.clone())
                            .or_default()
                            .push(format!("Failed to create directory: {e}"));
                        skip_config = true;
                    }
                }

                if !skip_config {
                    let backed_up = if backup && path.exists() {
                        let bak_path = path.with_extension("bak");
                        match fs::copy(&path, &bak_path) {
                            Ok(_) => true,
                            Err(e) => {
                                result
                                    .file_errors
                                    .entry(cfg_path.clone())
                                    .or_default()
                                    .push(format!("Backup failed: {e}"));
                                skip_config = true;
                                false
                            }
                        }
                    } else {
                        false
                    };

                    if !skip_config {
                        let ext_str = path.extension().and_then(|e| e.to_str()).unwrap_or("tmp");
                        let tmp_ext = format!("{ext_str}.cmty_tmp");
                        let tmp_path = path.with_extension(&tmp_ext);
                        pending.push((path, tmp_path, content, cfg_path, 0, backed_up));
                    }
                }
            }
        }

        // ── Phase 2: Write all files to .cmty_tmp temps ──
        let mut tmp_written: Vec<PathBuf> = Vec::new();
        let mut write_failed = false;

        for (_, tmp_path, content, key, _, _) in &pending {
            match fs::write(tmp_path, content) {
                Ok(()) => {
                    tmp_written.push(tmp_path.clone());
                }
                Err(e) => {
                    result
                        .file_errors
                        .entry(key.clone())
                        .or_default()
                        .push(format!("Write failed: {e}"));
                    write_failed = true;
                    break;
                }
            }
        }

        // If any temp write failed, clean up all temps and return
        if write_failed {
            for tmp in &tmp_written {
                let _ = fs::remove_file(tmp);
            }
            return Ok(result);
        }

        // ── Phase 3: Rename all .cmty_tmp → final (atomic per-file) ──
        for (final_path, tmp_path, content, key, entry_count, backed_up) in &pending {
            match fs::rename(tmp_path, final_path) {
                Ok(()) => {
                    result.files.push(ExportedFile {
                        path: key.clone(),
                        entry_count: *entry_count,
                        bytes_written: content.len(),
                        backed_up: *backed_up,
                    });
                }
                Err(e) => {
                    result
                        .file_errors
                        .entry(key.clone())
                        .or_default()
                        .push(format!(
                            "Rename failed (temp file preserved at {}): {}",
                            tmp_path.display(),
                            e
                        ));
                }
            }
        }

        Ok(result)
    })
    .await
}

#[tauri::command]
async fn cmd_region_id_for_section(section: String) -> Result<String, AppError> {
    Ok(Section::parse_name(&section)
        .map(|s| s.region_id().to_string())
        .ok_or_else(|| format!("Unknown section: {section}"))?)
}

#[tauri::command]
async fn cmd_infer_schemas(
    app: tauri::AppHandle,
) -> Result<Vec<schema::infer::NodeSchema>, AppError> {
    blocking(move || {
        let db_paths = db_manager::get_db_paths(&app)
            .map_err(|e| format!("DB paths not available: {e}"))?;
        if !db_paths.base.exists() {
            return Err(format!("Reference DB not found at {}", db_paths.base.display()));
        }
        let mut lsx_sections = std::collections::HashMap::new();
        for section in Section::all_ordered() {
            if *section == Section::Meta || *section == Section::Spells {
                continue;
            }
            // Query each region_id the section covers (umbrella sections have multiple)
            let mut merged = std::collections::HashMap::new();
            for region_id in section.region_ids() {
                match reference_db::queries::query_vanilla_lsx_by_region(&db_paths.base, region_id) {
                    Ok(entries) => {
                        merged.extend(entries);
                    }
                    Err(e) => {
                        tracing::warn!(section = ?section, region_id = %region_id, error = %e, "Failed to load vanilla LSX from DB for schema inference");
                    }
                }
            }
            if !merged.is_empty() {
                lsx_sections.insert(*section, merged);
            }
        }
        Ok(schema::infer::infer_schemas(&lsx_sections))
    })
    .await
}

/// Dump node schemas from DB metadata (no data scanning required).
/// Replaces the slow `cmd_infer_schemas` path with a metadata-only query.
#[tauri::command]
async fn cmd_dump_db_schemas(
    app: tauri::AppHandle,
) -> Result<Vec<schema::infer::NodeSchema>, AppError> {
    blocking(move || {
        let db_paths =
            db_manager::get_db_paths(&app).map_err(|e| format!("DB paths not available: {e}"))?;
        if !db_paths.base.exists() {
            return Err(format!(
                "Reference DB not found at {}",
                db_paths.base.display()
            ));
        }
        reference_db::queries::query_db_schemas(&db_paths.base)
    })
    .await
}

// ─── Pak Section Listing & On-Demand Extraction ─────────────────────

/// List which data sections exist inside a .pak file without extracting it.
#[tauri::command]
async fn cmd_list_pak_sections(pak_path: String) -> Result<Vec<paths::PakSectionInfo>, AppError> {
    blocking(move || {
        let reader =
            pak::PakReader::open(std::path::Path::new(&pak_path)).map_err(|err| err.to_string())?;
        let paths_list = reader.active_entry_paths();
        Ok(paths::categorize_pak_sections(&paths_list))
    })
    .await
}

#[derive(serde::Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
struct CreateModResult {
    project_root: String,
    mod_root: String,
    meta_path: String,
    public_path: String,
    has_script_extender: bool,
}

/// Parse a version string like "1.0.0.0" into the BG3 int64 encoding.
/// Format: major * 2^55 + minor * 2^47 + revision * 2^31 + build
fn parse_version_to_int64(version: &str) -> i64 {
    let parts: Vec<u64> = version
        .split('.')
        .filter_map(|p| p.trim().parse().ok())
        .collect();
    let major = parts.first().copied().unwrap_or(1);
    let minor = parts.get(1).copied().unwrap_or(0);
    let revision = parts.get(2).copied().unwrap_or(0);
    let build = parts.get(3).copied().unwrap_or(0);
    (major << 55 | minor << 47 | revision << 31 | build) as i64
}

#[tauri::command]
async fn cmd_create_mod_scaffold(
    target_dir: String,
    mod_name: String,
    author: String,
    description: String,
    use_script_extender: bool,
    folder: String,
    version: String,
) -> Result<CreateModResult, AppError> {
    blocking(move || {
        let uuid = uuid::Uuid::new_v4().to_string();
        let folder_name = if folder.trim().is_empty() { mod_name.clone() } else { folder.trim().to_string() };
        let version64 = parse_version_to_int64(&version);
        let project_root = PathBuf::from(&target_dir).join(format!("Proj_{folder_name}"));
        let mod_root = project_root.join(&folder_name);
        let mods_dir = mod_root.join("Mods").join(&folder_name);
        let public_dir = mod_root.join("Public").join(&folder_name);

        fs::create_dir_all(&mods_dir)
            .map_err(|e| format!("Failed to create Mods dir: {e}"))?;
        fs::create_dir_all(&public_dir)
            .map_err(|e| format!("Failed to create Public dir: {e}"))?;

        // Generate meta.lsx
        let meta_path = mods_dir.join("meta.lsx");
        let meta_content = format!(
r#"<?xml version="1.0" encoding="utf-8"?>
<save>
  <version major="4" minor="0" revision="0" build="49" />
  <region id="Config">
    <node id="root">
      <children>
        <node id="Dependencies">
          <children />
        </node>
        <node id="ModuleInfo">
          <attribute id="Author" type="LSWString" value="{author}" />
          <attribute id="CharacterCreationLevelName" type="FixedString" value="" />
          <attribute id="Description" type="LSWString" value="{description}" />
          <attribute id="Folder" type="LSWString" value="{folder_name}" />
          <attribute id="GMTemplate" type="FixedString" value="" />
          <attribute id="LobbyLevelName" type="FixedString" value="" />
          <attribute id="MD5" type="LSString" value="" />
          <attribute id="MainMenuBackgroundVideo" type="FixedString" value="" />
          <attribute id="MenuLevelName" type="FixedString" value="" />
          <attribute id="Name" type="FixedString" value="{mod_name}" />
          <attribute id="NumPlayers" type="uint8" value="4" />
          <attribute id="PhotoBooth" type="FixedString" value="" />
          <attribute id="StartupLevelName" type="FixedString" value="" />
          <attribute id="Tags" type="LSWString" value="" />
          <attribute id="Type" type="FixedString" value="Add-on" />
          <attribute id="UUID" type="FixedString" value="{uuid}" />
          <attribute id="Version64" type="int64" value="{version64}" />
          <children>
            <node id="PublishVersion">
              <attribute id="Version64" type="int64" value="{version64}" />
            </node>
            <node id="Scripts" />
            <node id="TargetModes">
              <children>
                <node id="Target">
                  <attribute id="Object" type="FixedString" value="Story" />
                </node>
              </children>
            </node>
          </children>
        </node>
      </children>
    </node>
  </region>
</save>"#,
        );

        fs::write(&meta_path, meta_content)
            .map_err(|e| format!("Failed to write meta.lsx: {e}"))?;

        // Optional: Script Extender folder
        if use_script_extender {
            let se_dir = mod_root.join("Mods").join(&folder_name).join("ScriptExtender");
            let lua_dir = se_dir.join("Lua");
            fs::create_dir_all(&lua_dir)
                .map_err(|e| format!("Failed to create ScriptExtender dir: {e}"))?;

            // Sanitize folder name into a valid Lua identifier for ModTable
            let mod_table: String = folder_name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
                .collect();
            let mod_table = if mod_table.starts_with(|c: char| c.is_ascii_digit()) {
                format!("_{mod_table}")
            } else {
                mod_table
            };

            // Config.json
            let config_content = format!(
                r#"{{
    "RequiredVersion": 1,
    "ModTable": "{mod_table}",
    "FeatureFlags": [
        "Lua"
    ]
}}"#
            );
            fs::write(se_dir.join("Config.json"), &config_content)
                .map_err(|e| format!("Failed to write Config.json: {e}"))?;

            // BootstrapServer.lua
            let server_content = format!(
                "-- BootstrapServer.lua\n\
                 -- Script Extender server bootstrap for {folder_name}\n\
                 -- This file is loaded first in the SERVER context.\n\
                 -- Register event listeners, Osiris callbacks, and server-side logic here.\n\
                 -- Do NOT call Osi functions at the top level \u{2014} use Ext.Osiris.RegisterListener instead.\n\
                 \n\
                 Ext.Vars.RegisterModVariable(\n\
                 \x20   ModuleUUID,\n\
                 \x20   \"{mod_table}\",\n\
                 \x20   {{\n\
                 \x20       Server = true,\n\
                 \x20       Client = true,\n\
                 \x20       SyncToClient = true,\n\
                 \x20   }}\n\
                 )\n\
                 \n\
                 Ext.Utils.Print(\"[{folder_name}] Server bootstrap loaded\")\n"
            );
            fs::write(lua_dir.join("BootstrapServer.lua"), &server_content)
                .map_err(|e| format!("Failed to write BootstrapServer.lua: {e}"))?;

            // BootstrapClient.lua
            let client_content = format!(
                "-- BootstrapClient.lua\n\
                 -- Script Extender client bootstrap for {folder_name}\n\
                 -- This file is loaded first in the CLIENT context.\n\
                 -- Register UI hooks, IMGUI panels, and client-side logic here.\n\
                 -- Ext.Entity and Ext.IMGUI are available in this context.\n\
                 \n\
                 Ext.Utils.Print(\"[{folder_name}] Client bootstrap loaded\")\n"
            );
            fs::write(lua_dir.join("BootstrapClient.lua"), &client_content)
                .map_err(|e| format!("Failed to write BootstrapClient.lua: {e}"))?;
        }

        Ok(CreateModResult {
            project_root: project_root.to_string_lossy().into_owned(),
            mod_root: mod_root.to_string_lossy().into_owned(),
            meta_path: meta_path.to_string_lossy().into_owned(),
            public_path: public_dir.to_string_lossy().into_owned(),
            has_script_extender: use_script_extender,
        })
    })
    .await
}

// ── Secure storage commands (OS keychain via platform::credentials) ───

#[tauri::command]
async fn cmd_get_secure_setting(key: String) -> Result<String, AppError> {
    blocking(
        move || match platform::credentials::get_credential("settings", &key) {
            Ok(Some(val)) => Ok(val),
            Ok(None) => Ok(String::new()),
            Err(e) => Err(e.to_string()),
        },
    )
    .await
}

#[tauri::command]
async fn cmd_set_secure_setting(key: String, value: String) -> Result<(), AppError> {
    blocking(move || {
        if value.is_empty() {
            platform::credentials::delete_credential("settings", &key).map_err(|e| e.to_string())
        } else {
            platform::credentials::store_credential("settings", &key, &value)
                .map_err(|e| e.to_string())
        }
    })
    .await
}

// ── DDS texture conversion commands ──────────────────────────────────

#[tauri::command]
async fn cmd_convert_dds_to_png(path: String, project_dir: String) -> Result<String, AppError> {
    blocking(move || commands::texture::convert_dds_to_png(&path, &project_dir)).await
}

#[tauri::command]
async fn cmd_get_dds_dimensions(path: String, project_dir: String) -> Result<(u32, u32), AppError> {
    blocking(move || commands::texture::get_dds_dimensions(&path, &project_dir)).await
}

/// One-time migration from legacy keyring service ("cmtystudio") to the new
/// unified platform service ("bg3-cmty-studio").
fn migrate_legacy_keyring() {
    // Settings keys stored by the old secure_storage module
    let settings_keys = ["vanillaPath", "gameDataPath", "lastProjectPath"];
    for key in &settings_keys {
        if let Ok(Some(value)) = platform::credentials::get_legacy_credential(key) {
            if platform::credentials::store_credential("settings", key, &value).is_ok() {
                let _ = platform::credentials::delete_legacy_credential(key);
            }
        }
    }

    // Forge tokens stored by the old forge_api/secure_storage (format: "forge_token:{host}")
    let common_hosts = ["github.com", "gitlab.com", "codeberg.org"];
    for host in &common_hosts {
        let old_key = format!("forge_token:{host}");
        if let Ok(Some(token)) = platform::credentials::get_legacy_credential(&old_key) {
            if platform::credentials::store_credential("forge_token", host, &token).is_ok() {
                let _ = platform::credentials::delete_legacy_credential(&old_key);
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[allow(unused_imports)]
    use tauri::Manager;
    logging::init();

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init());

    #[cfg(feature = "nexus-integration")]
    let builder = builder.manage(platform::nexus::commands::NexusState {
        client: std::sync::Mutex::new(None),
    });

    #[cfg(feature = "modio-integration")]
    let builder = builder.manage(platform::modio::commands::ModioState {
        client: std::sync::Mutex::new(None),
    });

    builder
        .setup(|app| {
            // Migrate credentials from legacy keyring service name
            migrate_legacy_keyring();
            // Suppress unused-variable warning when all platform features are disabled.
            let _ = &app;

            #[cfg(feature = "nexus-integration")]
            {
                // Auto-initialise the Nexus client from stored keyring credential.
                let nexus_state = app.state::<platform::nexus::commands::NexusState>();
                platform::nexus::commands::try_auto_init(nexus_state.inner());
            }

            #[cfg(feature = "modio-integration")]
            {
                // Auto-initialise the mod.io client from stored keyring credentials.
                let modio_state = app.state::<platform::modio::commands::ModioState>();
                if let Err(e) = platform::modio::commands::try_restore_client(modio_state.inner()) {
                    tracing::warn!("Failed to auto-initialise mod.io client: {e}");
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd_scan_mod,
            cmd_list_available_sections,
            cmd_query_section_entries,
            cmd_generate_config,
            cmd_preview_config,
            cmd_save_config,
            cmd_rename_dir,
            cmd_detect_anchors,
            cmd_read_existing_config,
            cmd_get_vanilla_entries,
            cmd_get_list_items,
            cmd_get_cost_resources,
            cmd_get_entries_by_folder,
            cmd_get_stat_entries,
            cmd_get_mod_stat_entries,
            cmd_get_stat_field_names,
            cmd_get_stat_entry_fields,
            cmd_write_stats,
            cmd_get_progression_table_uuids,
            cmd_get_voice_table_uuids,
            cmd_get_selector_ids,
            cmd_get_equipment_names,
            cmd_get_icon_names,
            cmd_get_icon_atlas_data,
            cmd_get_staging_icon_atlas_data,
            cmd_get_value_lists,
            cmd_get_localization_map,
            cmd_get_mod_localization,
            cmd_generate_localization_xml,
            cmd_process_mod_folder,
            cmd_dir_size,
            cmd_read_mod_meta,
            cmd_detect_mod_folders,
            cmd_rediff_mod,
            cmd_list_load_order_paks,
            cmd_get_active_mod_folders,
            cmd_detect_game_data_path,
            cmd_validate_game_data_path,
            cmd_open_path,
            cmd_reveal_path,
            cmd_copy_file,
            cmd_file_exists,
            cmd_find_readme,
            cmd_read_text_file,
            cmd_list_files_by_ext,
            cmd_write_text_file,
            cmd_preview_lsx,
            cmd_save_lsx,
            cmd_export_mod,
            cmd_region_id_for_section,
            cmd_infer_schemas,
            cmd_dump_db_schemas,
            cmd_create_mod_scaffold,
            cmd_list_pak_sections,
            cmd_list_mod_files,
            cmd_read_mod_file,
            cmd_get_secure_setting,
            cmd_set_secure_setting,
            commands::db::cmd_build_reference_db,
            commands::db::cmd_populate_reference_db,
            commands::db::cmd_populate_honor_db,
            commands::db::cmd_populate_mods_db,
            commands::db::cmd_remove_mod_from_mods_db,
            commands::db::cmd_clear_mods_db,
            commands::db::cmd_validate_cross_db_fks,
            commands::db::cmd_create_staging_db,
            commands::db::cmd_populate_staging_from_mod,
            commands::db::cmd_get_db_paths,
            commands::db::cmd_get_db_status,
            commands::db::cmd_reset_databases,
            commands::db::cmd_reset_reference_dbs,
            commands::db::cmd_recreate_staging,
            commands::db::cmd_check_staging_integrity,
            commands::db::cmd_unpack_and_populate,
            commands::db::cmd_populate_game_data,
            commands::db::cmd_staging_upsert_row,
            commands::db::cmd_staging_mark_deleted,
            commands::db::cmd_staging_unmark_deleted,
            commands::db::cmd_staging_batch_write,
            commands::db::cmd_staging_query_changes,
            commands::db::cmd_staging_list_sections,
            commands::db::cmd_staging_query_section,
            commands::db::cmd_staging_get_row,
            commands::db::cmd_staging_get_meta,
            commands::db::cmd_staging_set_meta,
            commands::db::cmd_staging_snapshot,
            commands::db::cmd_staging_undo,
            commands::db::cmd_staging_redo,
            commands::db::cmd_staging_wal_checkpoint,
            commands::db::cmd_staging_compact_undo,
            commands::db::cmd_staging_replace_section,
            commands::db::cmd_validate_handlers,
            commands::export::cmd_save_project,
            commands::export::cmd_save_section,
            commands::package::cmd_package_mod,
            commands::scripts::cmd_script_read,
            commands::scripts::cmd_script_write,
            commands::scripts::cmd_script_delete,
            commands::scripts::cmd_script_list,
            commands::scripts::cmd_script_rename,
            commands::scripts::cmd_script_create_from_template,
            commands::scripts::cmd_scaffold_se_structure,
            commands::scripts::cmd_scaffold_khonsu_structure,
            commands::scripts::cmd_scaffold_osiris_structure,
            commands::scripts::cmd_list_external_templates,
            commands::scripts::cmd_create_from_external_template,
            commands::scripts::cmd_validate_mcm_blueprint,
            commands::scripts::cmd_validate_script,
            commands::scripts::cmd_list_custom_linters,
            commands::scan::cmd_import_se_scripts,
            commands::scan::cmd_import_osiris_goals,
            commands::scan::cmd_import_khonsu_scripts,
            commands::scan::cmd_import_anubis_scripts,
            commands::scan::cmd_import_constellations_scripts,
            commands::scan::cmd_import_config_files,
            commands::filesystem::cmd_touch_file,
            commands::filesystem::cmd_create_mod_directory,
            commands::filesystem::cmd_move_mod_file,
            commands::filesystem::cmd_copy_mod_file,
            commands::filesystem::cmd_delete_mod_path,
            commands::http::cmd_fetch_remote_schema,
            // Git integration
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_init,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_clone,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_repo_info,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_open,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_status,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_stage,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_stage_all,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_unstage,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_discard,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_diff_file,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_commit,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_amend,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_log,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_show,
            #[cfg(feature = "git-integration")]
            commands::git_branch::cmd_git_branches,
            #[cfg(feature = "git-integration")]
            commands::git_branch::cmd_git_checkout,
            #[cfg(feature = "git-integration")]
            commands::git_branch::cmd_git_create_branch,
            #[cfg(feature = "git-integration")]
            commands::git_branch::cmd_git_delete_branch,
            #[cfg(feature = "git-integration")]
            commands::git_branch::cmd_git_merge,
            #[cfg(feature = "git-integration")]
            commands::git_remote::cmd_git_remotes,
            #[cfg(feature = "git-integration")]
            commands::git_remote::cmd_git_add_remote,
            #[cfg(feature = "git-integration")]
            commands::git_remote::cmd_git_remove_remote,
            #[cfg(feature = "git-integration")]
            commands::git_remote::cmd_git_fetch,
            #[cfg(feature = "git-integration")]
            commands::git_remote::cmd_git_pull,
            #[cfg(feature = "git-integration")]
            commands::git_remote::cmd_git_push,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_stash,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_stash_list,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_stash_apply,
            #[cfg(feature = "git-integration")]
            commands::git_core::cmd_git_stash_drop,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_detect,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_auth_status,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_set_token,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_clear_token,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_list_repos,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_create_repo,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_list_prs,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_create_pr,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_list_issues,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_create_issue,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_get_issue,
            #[cfg(feature = "git-integration")]
            commands::forge_api::cmd_forge_assign_issue,
            commands::project_settings::cmd_ensure_cmtystudio_dir,
            commands::project_settings::cmd_read_project_file,
            commands::project_settings::cmd_write_project_file,
            // DDS texture conversion
            cmd_convert_dds_to_png,
            cmd_get_dds_dimensions,
            // Nexus Mods
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_set_api_key,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_clear_api_key,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_has_api_key,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_validate_api_key,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_resolve_mod,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_get_file_groups,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_get_file_versions,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_get_all_mod_files,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_upload_file,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_create_mod_file,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_package_and_upload,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_get_mod_requirements,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_set_file_dependencies,
            #[cfg(feature = "nexus-integration")]
            platform::nexus::commands::cmd_nexus_rename_file_group,
            // mod.io
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_set_api_key,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_set_oauth_token,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_has_oauth_token,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_clear_api_key,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_connect,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_verify_code,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_disconnect,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_get_user,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_has_api_key,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_get_my_mods,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_get_mod,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_get_mod_by_name_id,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_upload_file,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_package_and_upload,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_create_mod,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_edit_mod,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_add_media,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_delete_media,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_list_files,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_edit_file,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_delete_file,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_get_dependencies,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_add_dependencies,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_remove_dependencies,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_get_game_tags,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_add_tags,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_remove_tags,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_get_metadata,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_add_metadata,
            #[cfg(feature = "modio-integration")]
            platform::modio::commands::cmd_modio_remove_metadata,
            // Dependency suggestions
            platform::deps_suggest::cmd_suggest_dependencies,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// ── S-ERRTEST: Error path tests for lib.rs IPC commands ─────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use tempfile::TempDir;

    // ── Test 3: cmd_save_config — NTFS ADS path injection ──────────

    #[tokio::test]
    async fn save_config_rejects_ntfs_ads_bare_path() {
        let result =
            cmd_save_config("test: content".into(), "config.yaml:hidden".into(), false).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::SecurityViolation),
            "Expected SecurityViolation, got {:?}: {}",
            err.kind,
            err.message
        );
    }

    #[tokio::test]
    async fn save_config_rejects_ntfs_ads_with_drive() {
        let result = cmd_save_config(
            "test: content".into(),
            "C:\\temp\\config.yaml:hidden".into(),
            false,
        )
        .await;
        let err = result.unwrap_err();
        assert!(matches!(err.kind, ErrorKind::SecurityViolation));
    }

    // ── Test 4: cmd_save_config — Double extension ─────────────────

    #[tokio::test]
    async fn save_config_rejects_double_extension() {
        let result = cmd_save_config(
            "test: content".into(),
            "C:\\temp\\config.yaml.exe".into(),
            false,
        )
        .await;
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::InvalidInput),
            "Expected InvalidInput, got {:?}: {}",
            err.kind,
            err.message
        );
        assert!(err.message.contains("double extension"));
    }

    // ── Test 5: cmd_save_config — Unsupported extension ────────────

    #[tokio::test]
    async fn save_config_rejects_unsupported_extension() {
        let result = cmd_save_config(
            "test: content".into(),
            "C:\\temp\\config.toml".into(),
            false,
        )
        .await;
        let err = result.unwrap_err();
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
        assert!(err.message.contains("Unsupported file extension"));
    }

    #[tokio::test]
    async fn save_config_allows_supported_extensions() {
        // Verify valid extensions are accepted (will fail at fs::write, not validation)
        for ext in &["yaml", "json", "txt", "lsx", "xml"] {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join(format!("config.{ext}"));
            let result = cmd_save_config(
                "test: content".into(),
                path.to_string_lossy().into_owned(),
                false,
            )
            .await;
            // Should succeed (file gets written)
            assert!(
                result.is_ok(),
                "Extension .{ext} should be allowed but got: {:?}",
                result.err()
            );
        }
    }

    // ── Test 11: cmd_save_lsx — Non-.lsx extension ─────────────────

    #[tokio::test]
    async fn save_lsx_rejects_non_lsx_extension() {
        let result = cmd_save_lsx(vec![], "TestRegion".into(), "C:\\temp\\output.exe".into()).await;
        let err = result.unwrap_err();
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
        assert!(err.message.contains(".lsx extension"));
    }

    #[tokio::test]
    async fn save_lsx_rejects_txt_extension() {
        let result = cmd_save_lsx(vec![], "TestRegion".into(), "C:\\temp\\output.txt".into()).await;
        let err = result.unwrap_err();
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
    }

    // ── Test 20: cmd_read_mod_file — Path traversal rejection ──────

    #[tokio::test]
    async fn read_mod_file_rejects_path_traversal() {
        let tmp = TempDir::new().unwrap();
        let mod_root = tmp.path().join("mod_root");
        fs::create_dir_all(&mod_root).unwrap();
        fs::write(mod_root.join("legit.txt"), "ok").unwrap();
        // Place a file outside the mod root
        fs::write(tmp.path().join("secret.txt"), "secret data").unwrap();

        let result = cmd_read_mod_file(
            mod_root.to_string_lossy().into_owned(),
            "../secret.txt".into(),
        )
        .await;

        assert!(result.is_err(), "Path traversal should be rejected");
        let err = result.unwrap_err();
        // The internal error from blocking() becomes Internal kind
        assert!(
            err.message.contains("Path traversal not allowed")
                || err.message.contains("File not found"),
            "Unexpected error message: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn read_mod_file_rejects_windows_backslash_traversal() {
        let tmp = TempDir::new().unwrap();
        let mod_root = tmp.path().join("mod_root");
        fs::create_dir_all(&mod_root).unwrap();
        fs::write(tmp.path().join("secret.txt"), "secret data").unwrap();

        let result = cmd_read_mod_file(
            mod_root.to_string_lossy().into_owned(),
            "..\\secret.txt".into(),
        )
        .await;

        assert!(
            result.is_err(),
            "Backslash path traversal should be rejected"
        );
    }

    // ── Test 21: cmd_read_mod_file — File too large ────────────────

    #[tokio::test]
    async fn read_mod_file_rejects_oversized_file() {
        let tmp = TempDir::new().unwrap();
        let big_file = tmp.path().join("big.txt");
        // MAX_MOD_FILE_SIZE is 2 MB — create a file that exceeds it
        let data = vec![b'A'; (MAX_MOD_FILE_SIZE + 1024) as usize];
        fs::write(&big_file, &data).unwrap();

        let result =
            cmd_read_mod_file(tmp.path().to_string_lossy().into_owned(), "big.txt".into()).await;

        let err = result.unwrap_err();
        assert!(
            err.message.contains("exceeding"),
            "Expected size-limit error, got: {}",
            err.message
        );
    }

    // ── Test 22: cmd_read_existing_config — Bad extension, oversized ─

    #[tokio::test]
    async fn read_existing_config_rejects_exe_extension() {
        let result = cmd_read_existing_config("C:\\temp\\config.exe".into()).await;
        let err = result.unwrap_err();
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
        assert!(err.message.contains(".yaml or .json"));
    }

    #[tokio::test]
    async fn read_existing_config_rejects_txt_extension() {
        let result = cmd_read_existing_config("C:\\temp\\config.txt".into()).await;
        let err = result.unwrap_err();
        assert!(matches!(err.kind, ErrorKind::InvalidInput));
    }

    #[tokio::test]
    async fn read_existing_config_rejects_oversized_file() {
        let tmp = TempDir::new().unwrap();
        let big_file = tmp.path().join("big.yaml");
        // MAX_CONFIG_FILE_SIZE is 10 MB — exceed it
        let data = vec![b'A'; (MAX_CONFIG_FILE_SIZE + 1024) as usize];
        fs::write(&big_file, &data).unwrap();

        let result = cmd_read_existing_config(big_file.to_string_lossy().into_owned()).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("too large") || err.message.contains("exceeding"),
            "Expected size-limit error, got: {}",
            err.message
        );
    }

    // ── Test 23: cmd_get_selector_ids — Symlink / canonicalization ──
    //
    // Full symlink testing requires elevated privileges on Windows.
    // We verify that canonicalization of nonexistent paths fails, which is
    // the same code path that rejects symlink-injected extra paths in
    // cmd_get_selector_ids (SEC-03).

    #[test]
    fn canonicalize_rejects_nonexistent_path() {
        let bogus = PathBuf::from("Z:\\nonexistent_path_that_cannot_exist\\mod");
        assert!(
            bogus.canonicalize().is_err(),
            "Canonicalize should fail for nonexistent paths"
        );
    }

    // ── Test 24: cmd_rename_dir — Source missing / target exists ────

    #[tokio::test]
    async fn rename_dir_rejects_missing_source() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("does_not_exist");
        let target = tmp.path().join("target");

        let result = cmd_rename_dir(
            nonexistent.to_string_lossy().into_owned(),
            target.to_string_lossy().into_owned(),
        )
        .await;

        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::NotFound),
            "Expected NotFound, got {:?}: {}",
            err.kind,
            err.message
        );
    }

    #[tokio::test]
    async fn rename_dir_rejects_existing_target() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source_dir");
        let target = tmp.path().join("target_dir");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        let result = cmd_rename_dir(
            source.to_string_lossy().into_owned(),
            target.to_string_lossy().into_owned(),
        )
        .await;

        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::InvalidInput),
            "Expected InvalidInput, got {:?}: {}",
            err.kind,
            err.message
        );
        assert!(err.message.contains("Target already exists"));
    }

    // ── Test 25: Unknown section handling ───────────────────────────
    //
    // cmd_get_vanilla_entries and cmd_get_entries_by_folder require an
    // AppHandle and cannot be unit-tested directly. We verify the
    // validation layers they rely on: Section::parse_name and
    // validate_folder_name.

    #[test]
    fn parse_name_returns_none_for_unknown_section() {
        assert!(Section::parse_name("NonExistentSection999").is_none());
        assert!(Section::parse_name("").is_none());
        assert!(Section::parse_name("Bogus").is_none());
        assert!(
            Section::parse_name("races").is_none(),
            "parse_name is case-sensitive"
        );
    }

    #[test]
    fn parse_name_returns_some_for_known_sections() {
        assert!(Section::parse_name("Races").is_some());
        assert!(Section::parse_name("Progressions").is_some());
        assert!(Section::parse_name("Feats").is_some());
    }

    #[test]
    fn validate_folder_name_rejects_traversal() {
        assert!(validate_folder_name("../etc/passwd").is_err());
        assert!(validate_folder_name("..\\system32").is_err());
        assert!(validate_folder_name("sub/folder").is_err());
        assert!(validate_folder_name("/absolute").is_err());
        assert!(validate_folder_name("C:drive").is_err());
        assert!(validate_folder_name("has\0null").is_err());
    }

    #[test]
    fn validate_folder_name_accepts_valid_names() {
        assert!(validate_folder_name("Progressions").is_ok());
        assert!(validate_folder_name("NonExistentSection999").is_ok());
        assert!(validate_folder_name("ColorDefinitions").is_ok());
    }

    // ── Test 26: cmd_generate_localization_xml — Malformed XML ─────

    #[test]
    fn generate_loca_xml_handles_incomplete_xml() {
        let entries = vec![AutoLocaInput {
            contentuid: "h_new_entry".into(),
            version: 1,
            text: "Hello World".into(),
        }];
        // Unterminated element — parser should break on error, then
        // new entries are appended to whatever was parsed.
        let malformed = "<contentList><content contentuid=\"h_old\" version=\"1\">Old text";

        let result = cmd_generate_localization_xml(entries, Some(malformed.into()));
        assert!(
            result.is_ok(),
            "Should not panic on malformed XML: {:?}",
            result.err()
        );

        let xml = result.unwrap();
        assert!(
            xml.contains("h_new_entry"),
            "New entry must be present in output"
        );
        assert!(xml.contains("Hello World"));
    }

    #[test]
    fn generate_loca_xml_handles_total_garbage() {
        let entries = vec![AutoLocaInput {
            contentuid: "h_fresh".into(),
            version: 2,
            text: "Fresh entry".into(),
        }];

        let result = cmd_generate_localization_xml(entries, Some("<<<not xml at all>>>".into()));
        assert!(result.is_ok(), "Should not panic on garbage input");
        let xml = result.unwrap();
        assert!(xml.contains("h_fresh"));
        assert!(xml.contains("Fresh entry"));
    }

    #[test]
    fn generate_loca_xml_merges_with_valid_existing() {
        let entries = vec![AutoLocaInput {
            contentuid: "h_existing".into(),
            version: 3,
            text: "Updated text".into(),
        }];
        let existing = r#"<?xml version="1.0" encoding="utf-8"?>
<contentList>
  <content contentuid="h_existing" version="1">Old text</content>
  <content contentuid="h_other" version="1">Keep me</content>
</contentList>"#;

        let result = cmd_generate_localization_xml(entries, Some(existing.into()));
        assert!(result.is_ok());
        let xml = result.unwrap();
        // Existing entry should be updated
        assert!(xml.contains("Updated text"), "Should contain updated text");
        assert!(!xml.contains("Old text"), "Old text should be replaced");
        // Non-matching entry preserved
        assert!(xml.contains("h_other"));
        assert!(xml.contains("Keep me"));
    }

    #[test]
    fn generate_loca_xml_handles_empty_entries() {
        let result = cmd_generate_localization_xml(vec![], None);
        assert!(result.is_ok());
        let xml = result.unwrap();
        assert!(xml.contains("contentList"));
    }
}
