use crate::blocking;
use crate::commands::diff::{diff_section, diff_stats};
use crate::commands::rediff::cache_mod_entries;
use crate::error::AppError;
use crate::models::*;
use crate::parsers::lsx::parse_lsx_file;
use crate::parsers::meta::parse_meta_lsx;
use crate::parsers::stats_txt::parse_stats_file_typed;
use crate::reference_db::queries;
use crate::validation::{check_file_size, MAX_LSX_FILE_SIZE, MAX_MOD_FILE_SIZE};
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Mapping from relative path patterns to Section.
/// Uses exact path-component matching to avoid substring false positives.
fn file_to_section(relative_path: &str) -> Option<Section> {
    let components: Vec<&str> = relative_path.split(['\\', '/']).collect();
    let has = |name: &str| components.contains(&name);

    // Check the file name to distinguish BackgroundGoals from Backgrounds,
    // since both live inside the Backgrounds/ folder.
    let file_name = components.last().copied().unwrap_or("");

    if has("Progressions") {
        Some(Section::Progressions)
    } else if has("Races") {
        Some(Section::Races)
    } else if has("Feats") && file_name.starts_with("FeatDescriptions") || has("FeatDescriptions") {
        Some(Section::FeatDescriptions)
    } else if has("Feats") {
        Some(Section::Feats)
    } else if has("Origins") {
        Some(Section::Origins)
    } else if has("Backgrounds") && file_name.starts_with("BackgroundGoals") {
        // BackgroundGoals.lsx lives inside Backgrounds/ — must check before Backgrounds
        Some(Section::BackgroundGoals)
    } else if has("Backgrounds") && !file_name.starts_with("BackgroundGoals") {
        Some(Section::Backgrounds)
    } else if has("BackgroundGoals") {
        Some(Section::BackgroundGoals)
    } else if has("ActionResourceDefinitions") {
        Some(Section::ActionResources)
    } else if has("ActionResourceGroupDefinitions") {
        Some(Section::ActionResourceGroups)
    } else if has("ClassDescriptions") {
        Some(Section::ClassDescriptions)
    } else if has("Lists") {
        Some(Section::Lists)
    } else if has("Gods") {
        Some(Section::Gods)
    } else if has("Tags") {
        Some(Section::Tags)
    } else if has("Visuals") {
        Some(Section::Visuals)
    } else if has("CharacterCreationPresets") {
        Some(Section::CharacterCreationPresets)
    } else if has("CharacterCreation") {
        Some(Section::CharacterCreation)
    } else if has("ColorDefinitions") {
        Some(Section::ColorDefinitions)
    } else if has("Animation") && has("AnimationOverrides") {
        Some(Section::AnimationOverrides)
    } else if has("Animation") {
        Some(Section::Animation)
    } else if has("Calendar") {
        Some(Section::Calendar)
    } else if has("CinematicArenaFrequencyGroups") {
        Some(Section::CinematicArenaFrequencyGroups)
    } else if has("CombatCameraGroups") {
        Some(Section::CombatCameraGroups)
    } else if has("Content") {
        Some(Section::Content)
    } else if has("CustomDice") {
        Some(Section::CustomDice)
    } else if has("DefaultValues") {
        Some(Section::DefaultValues)
    } else if has("DifficultyClasses") {
        Some(Section::DifficultyClasses)
    } else if has("Disturbances") {
        Some(Section::Disturbances)
    } else if has("Encumbrance") {
        Some(Section::Encumbrance)
    } else if has("EquipmentTypes") {
        Some(Section::EquipmentTypes)
    } else if has("Factions") {
        Some(Section::Factions)
    } else if has("Flags") {
        Some(Section::Flags)
    } else if has("FixedHotBarSlots") {
        Some(Section::FixedHotBarSlots)
    } else if has("GUI") {
        // GUI folder contains two distinct region types:
        // - TextureAtlasInfo.lsx → TextureAtlasInfo
        // - Icons_*.lsx (e.g. Icons_Items.lsx) → IconUVList
        if file_name.starts_with("Icons") {
            Some(Section::IconUVList)
        } else {
            Some(Section::TextureAtlasInfo)
        }
    } else if has("ItemThrowParams") {
        Some(Section::ItemThrowParams)
    } else if has("Levelmaps") {
        Some(Section::Levelmaps)
    } else if has("LimbsMapping") {
        Some(Section::LimbsMapping)
    } else if has("MultiEffectInfos") {
        Some(Section::MultiEffectInfos)
    } else if has("ProjectileDefaults") {
        Some(Section::ProjectileDefaults)
    } else if has("RandomCasts") {
        Some(Section::RandomCasts)
    } else if has("RootTemplates") {
        Some(Section::RootTemplates)
    } else if has("Ruleset") {
        Some(Section::Ruleset)
    } else if has("Shapeshift") {
        Some(Section::Shapeshift)
    } else if has("Sound") {
        Some(Section::Sound)
    } else if has("Spell") {
        Some(Section::SpellMetadata)
    } else if has("Status") {
        Some(Section::StatusMetadata)
    } else if has("Surface") {
        Some(Section::Surface)
    } else if has("TooltipExtras") {
        Some(Section::TooltipExtras)
    } else if has("TrajectoryRules") {
        Some(Section::TrajectoryRules)
    } else if has("Tutorials") {
        Some(Section::Tutorials)
    } else if has("VFX") {
        Some(Section::VFX)
    } else if has("Voices") {
        Some(Section::Voices)
    } else if has("WeaponAnimationSetData") {
        Some(Section::WeaponAnimationSetData)
    } else if has("ProgressionDescriptions") {
        Some(Section::ProgressionDescriptions)
    } else if has("CompanionPresets") {
        Some(Section::CompanionPresets)
    } else if has("SpellLists") {
        Some(Section::SpellLists)
    } else if has("SkillLists") {
        Some(Section::SkillLists)
    } else if has("PassiveLists") {
        Some(Section::PassiveLists)
    } else if has("EquipmentLists") {
        Some(Section::EquipmentLists)
    } else if has("AbilityLists") {
        Some(Section::AbilityLists)
    } else if has("ExperienceRewards") {
        Some(Section::ExperienceRewards)
    } else if has("GoldValues") {
        Some(Section::GoldValues)
    } else if has("DeathEffects") {
        Some(Section::DeathEffects)
    } else if has("SpeakerGroups") {
        Some(Section::SpeakerGroups)
    } else if has("Gossips") {
        Some(Section::Gossips)
    } else {
        None
    }
}

/// Scan a mod folder and diff against vanilla data.
/// `extra_scan_paths` allows scanning additional directories (e.g. unpacked data)
/// for the same mod alongside the primary `mod_path`.
///
/// Vanilla comparison data is loaded from the reference DB at `vanilla_db_path`.
pub fn scan_mod(
    mod_path: &str,
    vanilla_db_path: &Path,
    extra_scan_paths: &[String],
) -> Result<ScanResult, String> {
    let mod_root = PathBuf::from(mod_path);

    // 1. Find and parse meta.lsx or meta.yaml
    let meta_path = find_meta_lsx(&mod_root)?;
    let meta_content =
        fs::read_to_string(&meta_path).map_err(|e| format!("Failed to read meta: {e}"))?;
    let is_yaml_meta = meta_path.extension().is_some_and(|ext| ext == "yaml");
    let mod_meta = if is_yaml_meta {
        crate::parsers::meta::parse_meta_yaml(&meta_content)?
    } else {
        parse_meta_lsx(&meta_content)?
    };

    // 2. Load vanilla comparison data (prefer DB, fall back to legacy cache)
    let (vanilla_lsx_sections, vanilla_stats): (
        HashMap<Section, HashMap<String, LsxEntry>>,
        HashMap<String, crate::models::StatsEntry>,
    ) = {
        let mut sections = HashMap::new();
        for section in Section::all_ordered() {
            if *section == Section::Meta || *section == Section::Spells {
                continue;
            }
            match queries::query_vanilla_lsx_for_scan(vanilla_db_path, section) {
                Ok(entries) if !entries.is_empty() => {
                    sections.insert(*section, entries);
                }
                Ok(_) => {} // empty – no vanilla data for this section
                Err(e) => {
                    tracing::warn!(section = ?section, error = %e, "Failed to load vanilla LSX from DB");
                }
            }
        }
        let stats = queries::query_vanilla_stats_for_scan(vanilla_db_path).unwrap_or_default();
        (sections, stats)
    };

    // 3. Scan mod data files from primary and any extra directories
    let public_path = mod_root.join("Public").join(&mod_meta.folder);
    let mut section_results: HashMap<Section, Vec<DiffEntry>> = HashMap::new();

    // Collect raw parsed entries for caching (used by rediff)
    let mut raw_lsx_entries: HashMap<Section, Vec<LsxEntry>> = HashMap::new();
    let mut raw_stats_entries: Vec<crate::models::StatsEntry> = Vec::new();
    let mut raw_source_files: HashMap<Section, String> = HashMap::new();

    // Parse meta as generic LSX entries for the Meta section
    let meta_lsx_entries = if is_yaml_meta {
        crate::parsers::lsx_yaml::yaml_to_lsx_entries(&meta_content).ok()
    } else {
        parse_lsx_file(&meta_content).ok()
    };
    if let Some(meta_lsx_entries) = meta_lsx_entries {
        let meta_relative = meta_path
            .strip_prefix(&mod_root)
            .unwrap_or(&meta_path)
            .to_string_lossy()
            .to_string();
        for lsx_entry in &meta_lsx_entries {
            let diff = DiffEntry {
                uuid: lsx_entry.uuid.clone(),
                display_name: mod_meta.name.clone(),
                source_file: meta_relative.clone(),
                entry_kind: EntryKind::New,
                changes: vec![],
                node_id: lsx_entry.node_id.clone(),
                region_id: lsx_entry.region_id.clone(),
                raw_attributes: lsx_entry
                    .attributes
                    .iter()
                    .map(|(k, v)| (k.clone(), v.value.clone()))
                    .collect(),
                raw_attribute_types: lsx_entry
                    .attributes
                    .iter()
                    .map(|(k, v)| (k.clone(), v.attr_type.clone()))
                    .collect(),
                raw_children: lsx_entry
                    .children
                    .iter()
                    .map(|c| {
                        (
                            c.group_id.clone(),
                            c.entries.iter().map(|e| e.object_guid.clone()).collect(),
                        )
                    })
                    .collect(),
                commented: lsx_entry.commented,
            };
            section_results.entry(Section::Meta).or_default().push(diff);
        }
    }

    // Build list of (scan_root, public_path) pairs to scan.
    // The primary mod path is always scanned first; extra paths are appended.
    let mut scan_roots: Vec<(PathBuf, PathBuf)> = Vec::new();
    scan_roots.push((mod_root.clone(), public_path));
    for extra in extra_scan_paths {
        let extra_root = PathBuf::from(extra);

        // SEC-03: Canonicalize to resolve any .. or relative components
        let extra_root = extra_root
            .canonicalize()
            .map_err(|e| format!("Failed to canonicalize extra scan path '{extra}': {e}"))?;

        // SEC-03: Reject symlinks
        let meta = fs::symlink_metadata(&extra_root)
            .map_err(|e| format!("Failed to read metadata for extra scan path '{extra}': {e}"))?;
        if meta.file_type().is_symlink() {
            return Err(format!(
                "Extra scan path '{extra}' is a symlink, which is not allowed for security reasons"
            ));
        }

        let extra_public = extra_root.join("Public").join(&mod_meta.folder);
        scan_roots.push((extra_root, extra_public));
    }

    for (scan_root, scan_public_path) in &scan_roots {
        // ── Try consolidated per-type YAML files first (new format) ──
        // These live at scan_root/<Section>.yaml (e.g. Progressions.yaml)
        let mut used_consolidated = false;
        for section in Section::all_ordered() {
            if *section == Section::Meta || *section == Section::Spells {
                continue; // Meta handled above, Spells is stats-based
            }
            let consolidated_path = scan_root.join(section.consolidated_filename());
            if consolidated_path.exists() {
                used_consolidated = true;
                let content = match fs::read_to_string(&consolidated_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                match crate::parsers::lsx_yaml::read_consolidated_file(&consolidated_path) {
                    Ok(cf) => {
                        let source_file = consolidated_path
                            .strip_prefix(scan_root)
                            .unwrap_or(&consolidated_path)
                            .to_string_lossy()
                            .to_string();
                        let mod_entries =
                            crate::parsers::lsx_yaml::consolidated_to_lsx_entries(&cf);

                        // Cache raw entries for later re-diffing
                        raw_lsx_entries
                            .entry(*section)
                            .or_default()
                            .extend(mod_entries.clone());
                        raw_source_files
                            .entry(*section)
                            .or_insert_with(|| source_file.clone());

                        let vanilla_entries = vanilla_lsx_sections
                            .get(section)
                            .cloned()
                            .unwrap_or_default();

                        let diffs =
                            diff_section(*section, &mod_entries, &vanilla_entries, &source_file);
                        section_results.entry(*section).or_default().extend(diffs);
                    }
                    Err(e) => {
                        tracing::warn!(path = %consolidated_path.display(), error = %e, "Failed to read consolidated file");
                    }
                }
                let _ = content; // suppress unused warning
            }
        }

        // ── Fall back to directory walk if no consolidated files found (old format) ──
        if !used_consolidated {
            // Scan LSX and YAML files
            if scan_public_path.exists() {
                for entry in WalkDir::new(scan_public_path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_type().is_file()
                            && e.path()
                                .extension()
                                .is_some_and(|ext| ext == "lsx" || ext == "yaml")
                    })
                {
                    let relative = entry
                        .path()
                        .strip_prefix(scan_root)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .to_string();

                    // For section matching, normalize .yaml paths to .lsx equivalent
                    let relative_for_section = relative.replace(".yaml", ".lsx");

                    if let Some(section) = file_to_section(&relative_for_section) {
                        if check_file_size(entry.path(), MAX_LSX_FILE_SIZE).is_err() {
                            continue;
                        }
                        let content = match fs::read_to_string(entry.path()) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };

                        let is_yaml = entry.path().extension().is_some_and(|ext| ext == "yaml");
                        let parse_result = if is_yaml {
                            crate::parsers::lsx_yaml::yaml_to_lsx_entries(&content)
                        } else {
                            parse_lsx_file(&content)
                        };

                        match parse_result {
                            Ok(mod_entries) => {
                                // Cache raw entries for later re-diffing
                                raw_lsx_entries
                                    .entry(section)
                                    .or_default()
                                    .extend(mod_entries.clone());
                                raw_source_files
                                    .entry(section)
                                    .or_insert_with(|| relative.clone());

                                let vanilla_entries = vanilla_lsx_sections
                                    .get(&section)
                                    .cloned()
                                    .unwrap_or_default();

                                let diffs = diff_section(
                                    section,
                                    &mod_entries,
                                    &vanilla_entries,
                                    &relative,
                                );
                                section_results.entry(section).or_default().extend(diffs);
                            }
                            Err(e) => {
                                tracing::warn!(file = %relative, error = %e, "Failed to parse mod file");
                            }
                        }
                    }
                }
            }
        } // end if !used_consolidated

        // ── Stats scanning: try consolidated Stats YAML first, fall back to .txt ──
        // consolidate_stats_to_yaml writes YAML directly into scan_root/ (e.g., scan_root/Passive.yaml).
        // Also check scan_root/Stats/ for alternative layouts.
        let mut used_consolidated_stats = false;
        let stats_subdir = scan_root.join("Stats");
        let stats_yaml_dirs: Vec<&Path> = [scan_root.as_path(), &stats_subdir]
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        for stats_dir in &stats_yaml_dirs {
            for entry in WalkDir::new(stats_dir)
                .follow_links(false)
                .max_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "yaml")
                })
            {
                let content = match fs::read_to_string(entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let yaml_opts = serde_saphyr::options! {
                    budget: serde_saphyr::budget! {
                        max_nodes: 5_000_000,
                        max_events: 10_000_000,
                    },
                };
                if let Ok(entries) = serde_saphyr::from_str_with_options::<Vec<serde_json::Value>>(
                    &content, yaml_opts,
                ) {
                    let mut mod_entries = Vec::new();
                    for val in &entries {
                        if let Some(map) = val.as_object() {
                            let name = map
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let entry_type = map
                                .get("type")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let parent = map
                                .get("using")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            let mut data = std::collections::HashMap::new();
                            if let Some(fields) = map.get("fields").and_then(|v| v.as_object()) {
                                for (k, v) in fields {
                                    if let Some(val) = v.as_str() {
                                        data.insert(k.to_string(), val.to_string());
                                    }
                                }
                            }
                            if !name.is_empty() {
                                mod_entries.push(crate::models::StatsEntry {
                                    name,
                                    entry_type,
                                    parent,
                                    data,
                                });
                            }
                        }
                    }
                    if !mod_entries.is_empty() {
                        used_consolidated_stats = true;
                        raw_stats_entries.extend(mod_entries.clone());
                        let mut diffs = diff_stats(&mod_entries, &vanilla_stats);
                        let rel_path = entry
                            .path()
                            .strip_prefix(&mod_root)
                            .unwrap_or(entry.path())
                            .to_string_lossy()
                            .to_string();
                        for d in &mut diffs {
                            if d.source_file.is_empty() {
                                d.source_file.clone_from(&rel_path);
                            }
                        }
                        section_results
                            .entry(Section::Spells)
                            .or_default()
                            .extend(diffs);
                    }
                }
            }
        }

        // Fallback: scan Stats .txt files if no consolidated Stats YAML found
        if !used_consolidated_stats && scan_public_path.exists() {
            let stats_path = scan_public_path
                .join("Stats")
                .join("Generated")
                .join("Data");
            if stats_path.exists() {
                for entry in WalkDir::new(&stats_path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(".txt")
                    })
                {
                    let content = match fs::read_to_string(entry.path()) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    let file_stem = entry
                        .path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    let mod_entries = parse_stats_file_typed(&content, Some(file_stem));
                    raw_stats_entries.extend(mod_entries.clone());
                    let mut diffs = diff_stats(&mod_entries, &vanilla_stats);
                    let rel_path = entry
                        .path()
                        .strip_prefix(&mod_root)
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .to_string();
                    for d in &mut diffs {
                        if d.source_file.is_empty() {
                            d.source_file.clone_from(&rel_path);
                        }
                    }
                    section_results
                        .entry(Section::Spells)
                        .or_default()
                        .extend(diffs);
                }
            }
        }
    } // end for scan_roots

    // Cache parsed entries for rediff support
    cache_mod_entries(
        mod_path,
        raw_lsx_entries,
        raw_stats_entries,
        raw_source_files,
    )?;

    // 4. Check for existing config
    let existing_config_path = find_existing_config(&mod_root, &mod_meta.folder);

    // 5. Build ordered section results
    let sections: Vec<SectionResult> = Section::all_ordered()
        .iter()
        .map(|&section| SectionResult {
            section,
            entries: section_results.remove(&section).unwrap_or_default(),
        })
        .collect();

    Ok(ScanResult {
        mod_meta,
        sections,
        existing_config_path,
    })
}

/// Maximum file size for SE script imports (2 MB).
const MAX_SE_SCRIPT_SIZE: u64 = MAX_MOD_FILE_SIZE;

/// Import Script Extender files from an existing mod into the staging authoring table.
///
/// Globs `{mod_root}/Mods/{mod_folder}/ScriptExtender/**/*.lua` and
/// `{mod_root}/Mods/{mod_folder}/ScriptExtender/Config.json`, reads each file,
/// computes a hash, and inserts into `_staging_authoring` with `_is_new=0`.
///
/// **Merge guard (CH-3):** Files that already exist in staging with `_is_modified=1`
/// or `_is_new=1` are skipped to avoid overwriting user edits.
fn import_se_scripts(
    conn: &rusqlite::Connection,
    mod_root: &Path,
    mod_folder: &str,
) -> Result<u64, String> {
    // SEC: Validate mod_folder has no path traversal
    if mod_folder.contains("..")
        || mod_folder.contains('/')
        || mod_folder.contains('\\')
        || mod_folder.contains('\0')
    {
        return Err("Invalid mod_folder: path traversal not allowed".to_string());
    }

    let se_dir = mod_root
        .join("Mods")
        .join(mod_folder)
        .join("ScriptExtender");
    if !se_dir.exists() {
        return Ok(0);
    }

    // SEC: Canonicalize to resolve symlinks and verify the path is under mod_root
    let canonical_se = se_dir
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize ScriptExtender path: {e}"))?;
    let canonical_root = mod_root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize mod root: {e}"))?;
    if !canonical_se.starts_with(&canonical_root) {
        return Err("ScriptExtender path is outside mod root".to_string());
    }

    // Ensure the authoring table exists
    crate::reference_db::staging::ensure_staging_authoring_table(conn)?;

    let mut imported = 0u64;

    for entry in WalkDir::new(&canonical_se)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();

        // Only allow .lua and .json files
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "lua" => {}
            "json" => {
                // Only allow Config.json specifically
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if file_name != "Config.json" {
                    continue;
                }
            }
            _ => continue,
        }

        // SEC: Check file size
        if check_file_size(path, MAX_SE_SCRIPT_SIZE).is_err() {
            tracing::warn!(path = %path.display(), "SE script exceeds size limit, skipping");
            continue;
        }

        // Build the staging key as relative path from mod_root
        // e.g. Mods/{folder}/ScriptExtender/Lua/BootstrapServer.lua
        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Merge guard (CH-3): skip if the key already exists with user edits
        let has_user_edits: bool = conn
            .prepare(
                "SELECT 1 FROM _staging_authoring WHERE key = ?1 AND (_is_modified = 1 OR _is_new = 1) AND _is_deleted = 0",
            )
            .map_err(|e| format!("Prepare merge guard check: {e}"))?
            .exists(rusqlite::params![relative])
            .map_err(|e| format!("Merge guard check: {e}"))?;

        if has_user_edits {
            tracing::debug!(key = %relative, "Skipping SE script import — user edits exist");
            continue;
        }

        // Read content
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "Failed to read SE script");
                continue;
            }
        };

        // Compute hash (same pattern as scripts.rs)
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        // Upsert: INSERT or UPDATE, but only for non-guarded rows
        conn.execute(
            "INSERT INTO _staging_authoring (key, value, _is_new, _is_modified, _is_deleted, _original_hash)
             VALUES (?1, ?2, 0, 0, 0, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, _is_modified = 0, _is_deleted = 0, _original_hash = excluded._original_hash",
            rusqlite::params![relative, content, hash],
        )
        .map_err(|e| format!("Insert SE script '{relative}': {e}"))?;

        imported += 1;
    }

    Ok(imported)
}

/// Tauri command: import SE scripts from an existing mod into the staging DB.
#[tauri::command]
pub async fn cmd_import_se_scripts(
    db_path: String,
    mod_path: String,
    mod_folder: String,
) -> Result<u64, AppError> {
    blocking(move || {
        let db = PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {db_path}"));
        }
        let mod_root = PathBuf::from(&mod_path);
        if !mod_root.is_dir() {
            return Err(format!("Mod path is not a directory: {mod_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        import_se_scripts(&conn, &mod_root, &mod_folder)
    })
    .await
}

/// Maximum file size for Osiris goal imports (2 MB).
const MAX_OSIRIS_GOAL_SIZE: u64 = MAX_MOD_FILE_SIZE;

/// Import Osiris goal files from an existing mod into the staging authoring table.
///
/// Globs `{mod_root}/Mods/{mod_folder}/Story/RawFiles/Goals/*.txt`, reads each file,
/// computes a hash, and inserts into `_staging_authoring` with `_is_new=0`.
///
/// **Merge guard (CH-3):** Files that already exist in staging with `_is_modified=1`
/// or `_is_new=1` are skipped to avoid overwriting user edits.
fn import_osiris_goals(
    conn: &rusqlite::Connection,
    mod_root: &Path,
    mod_folder: &str,
) -> Result<u64, String> {
    // SEC: Validate mod_folder has no path traversal
    if mod_folder.contains("..")
        || mod_folder.contains('/')
        || mod_folder.contains('\\')
        || mod_folder.contains('\0')
    {
        return Err("Invalid mod_folder: path traversal not allowed".to_string());
    }

    let goals_dir = mod_root
        .join("Mods")
        .join(mod_folder)
        .join("Story")
        .join("RawFiles")
        .join("Goals");
    if !goals_dir.exists() {
        return Ok(0);
    }

    // SEC: Canonicalize to resolve symlinks and verify the path is under mod_root
    let canonical_goals = goals_dir
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize goals path: {e}"))?;
    let canonical_root = mod_root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize mod root: {e}"))?;
    if !canonical_goals.starts_with(&canonical_root) {
        return Err("Goals directory escapes mod root (possible symlink attack)".to_string());
    }

    // Ensure the authoring table exists
    crate::reference_db::staging::ensure_staging_authoring_table(conn)?;

    let mut imported = 0u64;

    for entry in WalkDir::new(&canonical_goals)
        .follow_links(false)
        .max_depth(1) // Only direct children of Goals/
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "txt"))
    {
        let path = entry.path();

        // SEC: Check file size
        if check_file_size(path, MAX_OSIRIS_GOAL_SIZE).is_err() {
            tracing::warn!(path = %path.display(), "Osiris goal file exceeds size limit, skipping");
            continue;
        }

        // SEC: Verify no symlinks
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| format!("Failed to read metadata: {e}"))?;
        if meta.file_type().is_symlink() {
            continue;
        }

        // Read content
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read Osiris goal file"
                );
                continue;
            }
        };

        // Build the staging key as relative path from mod_root
        // Convention: Mods/{folder}/Story/RawFiles/Goals/{filename}
        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Merge guard (CH-3): skip if the key already exists with user edits
        let has_user_edits: bool = conn
            .prepare(
                "SELECT 1 FROM _staging_authoring WHERE key = ?1 AND (_is_modified = 1 OR _is_new = 1) AND _is_deleted = 0",
            )
            .map_err(|e| format!("Prepare merge guard check: {e}"))?
            .exists(rusqlite::params![relative])
            .map_err(|e| format!("Merge guard check: {e}"))?;

        if has_user_edits {
            tracing::debug!(key = %relative, "Skipping Osiris goal import — user edits exist");
            continue;
        }

        // Compute hash (same pattern as import_se_scripts)
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        // Upsert: INSERT or UPDATE, but only for non-guarded rows
        conn.execute(
            "INSERT INTO _staging_authoring (key, value, _is_new, _is_modified, _is_deleted, _original_hash)
             VALUES (?1, ?2, 0, 0, 0, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, _is_modified = 0, _is_deleted = 0, _original_hash = excluded._original_hash",
            rusqlite::params![relative, content, hash],
        )
        .map_err(|e| format!("Insert Osiris goal '{relative}': {e}"))?;

        imported += 1;
    }

    Ok(imported)
}

/// Tauri command: import Osiris goal files from an existing mod into the staging DB.
#[tauri::command]
pub async fn cmd_import_osiris_goals(
    db_path: String,
    mod_path: String,
    mod_folder: String,
) -> Result<u64, AppError> {
    blocking(move || {
        let db = PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {db_path}"));
        }
        let mod_root = PathBuf::from(&mod_path);
        if !mod_root.is_dir() {
            return Err(format!("Mod path is not a directory: {mod_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        import_osiris_goals(&conn, &mod_root, &mod_folder)
    })
    .await
}

/// Maximum file size for Khonsu script imports (2 MB).
const MAX_KHONSU_SCRIPT_SIZE: u64 = MAX_MOD_FILE_SIZE;

/// Import Khonsu script files from an existing mod into the staging authoring table.
///
/// Walks `{mod_root}/Mods/{mod_folder}/Scripts/` for `*.khn` files, reads each,
/// computes a hash, and inserts into `_staging_authoring` with `_is_new=0`.
///
/// **Merge guard (CH-3):** Files that already exist in staging with `_is_modified=1`
/// or `_is_new=1` are skipped to avoid overwriting user edits.
fn import_khonsu_scripts(
    conn: &rusqlite::Connection,
    mod_root: &Path,
    mod_folder: &str,
) -> Result<u64, String> {
    // SEC: Validate mod_folder has no path traversal
    if mod_folder.contains("..")
        || mod_folder.contains('/')
        || mod_folder.contains('\\')
        || mod_folder.contains('\0')
    {
        return Err("Invalid mod_folder: path traversal not allowed".to_string());
    }

    let scripts_dir = mod_root.join("Mods").join(mod_folder).join("Scripts");
    if !scripts_dir.exists() {
        return Ok(0);
    }

    // SEC: Canonicalize to resolve symlinks and verify the path is under mod_root
    let canonical_scripts = scripts_dir
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize Scripts path: {e}"))?;
    let canonical_root = mod_root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize mod root: {e}"))?;
    if !canonical_scripts.starts_with(&canonical_root) {
        return Err("Scripts directory escapes mod root (possible symlink attack)".to_string());
    }

    // Ensure the authoring table exists
    crate::reference_db::staging::ensure_staging_authoring_table(conn)?;

    let mut imported = 0u64;

    for entry in WalkDir::new(&canonical_scripts)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|ext| ext == "khn"))
    {
        let path = entry.path();

        // SEC: Check file size
        if check_file_size(path, MAX_KHONSU_SCRIPT_SIZE).is_err() {
            tracing::warn!(path = %path.display(), "Khonsu script exceeds size limit, skipping");
            continue;
        }

        // SEC: Verify no symlinks
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| format!("Failed to read metadata: {e}"))?;
        if meta.file_type().is_symlink() {
            continue;
        }

        // Read content
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read Khonsu script file"
                );
                continue;
            }
        };

        // Build the staging key as relative path from mod_root
        // Convention: Mods/{folder}/Scripts/{subpath}/{filename}.khn
        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Merge guard (CH-3): skip if the key already exists with user edits
        let has_user_edits: bool = conn
            .prepare(
                "SELECT 1 FROM _staging_authoring WHERE key = ?1 AND (_is_modified = 1 OR _is_new = 1) AND _is_deleted = 0",
            )
            .map_err(|e| format!("Prepare merge guard check: {e}"))?
            .exists(rusqlite::params![relative])
            .map_err(|e| format!("Merge guard check: {e}"))?;

        if has_user_edits {
            tracing::debug!(key = %relative, "Skipping Khonsu script import — user edits exist");
            continue;
        }

        // Compute hash
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        // Upsert: INSERT or UPDATE, but only for non-guarded rows
        conn.execute(
            "INSERT INTO _staging_authoring (key, value, _is_new, _is_modified, _is_deleted, _original_hash)
             VALUES (?1, ?2, 0, 0, 0, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, _is_modified = 0, _is_deleted = 0, _original_hash = excluded._original_hash",
            rusqlite::params![relative, content, hash],
        )
        .map_err(|e| format!("Insert Khonsu script '{relative}': {e}"))?;

        imported += 1;
    }

    Ok(imported)
}

/// Tauri command: import Khonsu script files from an existing mod into the staging DB.
#[tauri::command]
pub async fn cmd_import_khonsu_scripts(
    db_path: String,
    mod_path: String,
    mod_folder: String,
) -> Result<u64, AppError> {
    blocking(move || {
        let db = PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {db_path}"));
        }
        let mod_root = PathBuf::from(&mod_path);
        if !mod_root.is_dir() {
            return Err(format!("Mod path is not a directory: {mod_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        import_khonsu_scripts(&conn, &mod_root, &mod_folder)
    })
    .await
}

/// Maximum file size for Anubis script imports (2 MB).
const MAX_ANUBIS_SCRIPT_SIZE: u64 = MAX_MOD_FILE_SIZE;

/// Import Anubis behavior script files from an existing mod into the staging authoring table.
///
/// Walks `{mod_root}/Mods/{mod_folder}/Scripts/anubis/` for `*.anc`, `*.ann`, `*.anm` files,
/// reads each, computes a hash, and inserts into `_staging_authoring` with `_is_new=0`.
///
/// **Merge guard (CH-3):** Files that already exist in staging with `_is_modified=1`
/// or `_is_new=1` are skipped to avoid overwriting user edits.
fn import_anubis_scripts(
    conn: &rusqlite::Connection,
    mod_root: &Path,
    mod_folder: &str,
) -> Result<u64, String> {
    // SEC: Validate mod_folder has no path traversal
    if mod_folder.contains("..")
        || mod_folder.contains('/')
        || mod_folder.contains('\\')
        || mod_folder.contains('\0')
    {
        return Err("Invalid mod_folder: path traversal not allowed".to_string());
    }

    let scripts_dir = mod_root
        .join("Mods")
        .join(mod_folder)
        .join("Scripts")
        .join("anubis");
    if !scripts_dir.exists() {
        return Ok(0);
    }

    // SEC: Canonicalize to resolve symlinks and verify the path is under mod_root
    let canonical_scripts = scripts_dir
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize Scripts/anubis path: {e}"))?;
    let canonical_root = mod_root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize mod root: {e}"))?;
    if !canonical_scripts.starts_with(&canonical_root) {
        return Err(
            "Scripts/anubis directory escapes mod root (possible symlink attack)".to_string(),
        );
    }

    // Ensure the authoring table exists
    crate::reference_db::staging::ensure_staging_authoring_table(conn)?;

    let mut imported = 0u64;

    for entry in WalkDir::new(&canonical_scripts)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .is_some_and(|ext| ext == "anc" || ext == "ann" || ext == "anm")
        })
    {
        let path = entry.path();

        // SEC: Check file size
        if check_file_size(path, MAX_ANUBIS_SCRIPT_SIZE).is_err() {
            tracing::warn!(path = %path.display(), "Anubis script exceeds size limit, skipping");
            continue;
        }

        // SEC: Verify no symlinks
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| format!("Failed to read metadata: {e}"))?;
        if meta.file_type().is_symlink() {
            continue;
        }

        // Read content
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read Anubis script file"
                );
                continue;
            }
        };

        // Build the staging key as relative path from mod_root
        // Convention: Mods/{folder}/Scripts/anubis/{subpath}/{filename}.anc
        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Merge guard (CH-3): skip if the key already exists with user edits
        let has_user_edits: bool = conn
            .prepare(
                "SELECT 1 FROM _staging_authoring WHERE key = ?1 AND (_is_modified = 1 OR _is_new = 1) AND _is_deleted = 0",
            )
            .map_err(|e| format!("Prepare merge guard check: {e}"))?
            .exists(rusqlite::params![relative])
            .map_err(|e| format!("Merge guard check: {e}"))?;

        if has_user_edits {
            tracing::debug!(key = %relative, "Skipping Anubis script import — user edits exist");
            continue;
        }

        // Compute hash
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        // Upsert: INSERT or UPDATE, but only for non-guarded rows
        conn.execute(
            "INSERT INTO _staging_authoring (key, value, _is_new, _is_modified, _is_deleted, _original_hash)
             VALUES (?1, ?2, 0, 0, 0, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, _is_modified = 0, _is_deleted = 0, _original_hash = excluded._original_hash",
            rusqlite::params![relative, content, hash],
        )
        .map_err(|e| format!("Insert Anubis script '{relative}': {e}"))?;

        imported += 1;
    }

    Ok(imported)
}

/// Tauri command: import Anubis behavior scripts from an existing mod into the staging DB.
#[tauri::command]
pub async fn cmd_import_anubis_scripts(
    db_path: String,
    mod_path: String,
    mod_folder: String,
) -> Result<u64, AppError> {
    blocking(move || {
        let db = PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {db_path}"));
        }
        let mod_root = PathBuf::from(&mod_path);
        if !mod_root.is_dir() {
            return Err(format!("Mod path is not a directory: {mod_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        import_anubis_scripts(&conn, &mod_root, &mod_folder)
    })
    .await
}

/// Maximum file size for Constellations script imports (2 MB).
const MAX_CONSTELLATIONS_SCRIPT_SIZE: u64 = MAX_MOD_FILE_SIZE;

/// Import Constellations behavior script files from an existing mod into the staging authoring table.
///
/// Walks `{mod_root}/Mods/{mod_folder}/Scripts/constellations/` for `*.clc`, `*.cln`, `*.clm` files,
/// reads each, computes a hash, and inserts into `_staging_authoring` with `_is_new=0`.
///
/// **Merge guard (CH-3):** Files that already exist in staging with `_is_modified=1`
/// or `_is_new=1` are skipped to avoid overwriting user edits.
fn import_constellations_scripts(
    conn: &rusqlite::Connection,
    mod_root: &Path,
    mod_folder: &str,
) -> Result<u64, String> {
    // SEC: Validate mod_folder has no path traversal
    if mod_folder.contains("..")
        || mod_folder.contains('/')
        || mod_folder.contains('\\')
        || mod_folder.contains('\0')
    {
        return Err("Invalid mod_folder: path traversal not allowed".to_string());
    }

    let scripts_dir = mod_root
        .join("Mods")
        .join(mod_folder)
        .join("Scripts")
        .join("constellations");
    if !scripts_dir.exists() {
        return Ok(0);
    }

    // SEC: Canonicalize to resolve symlinks and verify the path is under mod_root
    let canonical_scripts = scripts_dir
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize Scripts/constellations path: {e}"))?;
    let canonical_root = mod_root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize mod root: {e}"))?;
    if !canonical_scripts.starts_with(&canonical_root) {
        return Err(
            "Scripts/constellations directory escapes mod root (possible symlink attack)"
                .to_string(),
        );
    }

    // Ensure the authoring table exists
    crate::reference_db::staging::ensure_staging_authoring_table(conn)?;

    let mut imported = 0u64;

    for entry in WalkDir::new(&canonical_scripts)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .is_some_and(|ext| ext == "clc" || ext == "cln" || ext == "clm")
        })
    {
        let path = entry.path();

        // SEC: Check file size
        if check_file_size(path, MAX_CONSTELLATIONS_SCRIPT_SIZE).is_err() {
            tracing::warn!(path = %path.display(), "Constellations script exceeds size limit, skipping");
            continue;
        }

        // SEC: Verify no symlinks
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| format!("Failed to read metadata: {e}"))?;
        if meta.file_type().is_symlink() {
            continue;
        }

        // Read content
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read Constellations script file"
                );
                continue;
            }
        };

        // Build the staging key as relative path from mod_root
        // Convention: Mods/{folder}/Scripts/constellations/{subpath}/{filename}.clc
        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Merge guard (CH-3): skip if the key already exists with user edits
        let has_user_edits: bool = conn
            .prepare(
                "SELECT 1 FROM _staging_authoring WHERE key = ?1 AND (_is_modified = 1 OR _is_new = 1) AND _is_deleted = 0",
            )
            .map_err(|e| format!("Prepare merge guard check: {e}"))?
            .exists(rusqlite::params![relative])
            .map_err(|e| format!("Merge guard check: {e}"))?;

        if has_user_edits {
            tracing::debug!(key = %relative, "Skipping Constellations script import — user edits exist");
            continue;
        }

        // Compute hash
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        // Upsert: INSERT or UPDATE, but only for non-guarded rows
        conn.execute(
            "INSERT INTO _staging_authoring (key, value, _is_new, _is_modified, _is_deleted, _original_hash)
             VALUES (?1, ?2, 0, 0, 0, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, _is_modified = 0, _is_deleted = 0, _original_hash = excluded._original_hash",
            rusqlite::params![relative, content, hash],
        )
        .map_err(|e| format!("Insert Constellations script '{relative}': {e}"))?;

        imported += 1;
    }

    Ok(imported)
}

/// Tauri command: import Constellations behavior scripts from an existing mod into the staging DB.
#[tauri::command]
pub async fn cmd_import_constellations_scripts(
    db_path: String,
    mod_path: String,
    mod_folder: String,
) -> Result<u64, AppError> {
    blocking(move || {
        let db = PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {db_path}"));
        }
        let mod_root = PathBuf::from(&mod_path);
        if !mod_root.is_dir() {
            return Err(format!("Mod path is not a directory: {mod_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        import_constellations_scripts(&conn, &mod_root, &mod_folder)
    })
    .await
}

/// Maximum file size for config file imports (2 MB).
const MAX_CONFIG_FILE_SIZE: u64 = MAX_MOD_FILE_SIZE;

/// Import config files (JSON/YAML) from an existing mod into the staging authoring table.
///
/// Walks `{mod_root}/Mods/{mod_folder}/` for `.json`, `.yaml`, `.yml` files,
/// excluding any files under `ScriptExtender/` (handled by `import_se_scripts`).
/// Also specifically includes `MCM_blueprint*.json` at any depth.
///
/// Excludes non-config files like `meta.lsx`, `meta.yaml`.
///
/// **Merge guard (CH-3):** Files that already exist in staging with `_is_modified=1`
/// or `_is_new=1` are skipped to avoid overwriting user edits.
fn import_config_files(
    conn: &rusqlite::Connection,
    mod_root: &Path,
    mod_folder: &str,
) -> Result<u64, String> {
    // SEC: Validate mod_folder has no path traversal
    if mod_folder.contains("..")
        || mod_folder.contains('/')
        || mod_folder.contains('\\')
        || mod_folder.contains('\0')
    {
        return Err("Invalid mod_folder: path traversal not allowed".to_string());
    }

    let mod_dir = mod_root.join("Mods").join(mod_folder);
    if !mod_dir.exists() {
        return Ok(0);
    }

    // SEC: Canonicalize to resolve symlinks and verify the path is under mod_root
    let canonical_mod_dir = mod_dir
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize mod directory path: {e}"))?;
    let canonical_root = mod_root
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize mod root: {e}"))?;
    if !canonical_mod_dir.starts_with(&canonical_root) {
        return Err("Mod directory escapes mod root (possible symlink attack)".to_string());
    }

    // Precompute the canonical ScriptExtender path for exclusion checks
    let se_dir = canonical_mod_dir.join("ScriptExtender");
    let canonical_se = se_dir.canonicalize().ok();

    // Ensure the authoring table exists
    crate::reference_db::staging::ensure_staging_authoring_table(conn)?;

    let mut imported = 0u64;

    for entry in WalkDir::new(&canonical_mod_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();

        // Only allow .json, .yaml, .yml files
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext {
            "json" | "yaml" | "yml" => {}
            _ => continue,
        }

        // Exclude files under ScriptExtender/ — those are handled by import_se_scripts
        if let Some(ref canonical_se_path) = canonical_se {
            if path.starts_with(canonical_se_path) {
                continue;
            }
        }

        // Exclude meta.lsx / meta.yaml (not config files)
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name == "meta.lsx" || file_name == "meta.yaml" {
            continue;
        }

        // SEC: Check file size
        if check_file_size(path, MAX_CONFIG_FILE_SIZE).is_err() {
            tracing::warn!(path = %path.display(), "Config file exceeds size limit, skipping");
            continue;
        }

        // SEC: Verify no symlinks
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| format!("Failed to read metadata: {e}"))?;
        if meta.file_type().is_symlink() {
            continue;
        }

        // Read content
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to read config file"
                );
                continue;
            }
        };

        // Build the staging key as relative path from mod_root
        // Convention: Mods/{folder}/{subpath}/{filename}.json
        let relative = path
            .strip_prefix(&canonical_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        // Merge guard (CH-3): skip if the key already exists with user edits
        let has_user_edits: bool = conn
            .prepare(
                "SELECT 1 FROM _staging_authoring WHERE key = ?1 AND (_is_modified = 1 OR _is_new = 1) AND _is_deleted = 0",
            )
            .map_err(|e| format!("Prepare merge guard check: {e}"))?
            .exists(rusqlite::params![relative])
            .map_err(|e| format!("Merge guard check: {e}"))?;

        if has_user_edits {
            tracing::debug!(key = %relative, "Skipping config file import — user edits exist");
            continue;
        }

        // Compute hash
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let hash = format!("{:016x}", hasher.finish());

        // Upsert: INSERT or UPDATE, but only for non-guarded rows
        conn.execute(
            "INSERT INTO _staging_authoring (key, value, _is_new, _is_modified, _is_deleted, _original_hash)
             VALUES (?1, ?2, 0, 0, 0, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, _is_modified = 0, _is_deleted = 0, _original_hash = excluded._original_hash",
            rusqlite::params![relative, content, hash],
        )
        .map_err(|e| format!("Insert config file '{relative}': {e}"))?;

        imported += 1;
    }

    Ok(imported)
}

/// Tauri command: import config files (JSON/YAML) from an existing mod into the staging DB.
#[tauri::command]
pub async fn cmd_import_config_files(
    db_path: String,
    mod_path: String,
    mod_folder: String,
) -> Result<u64, AppError> {
    blocking(move || {
        let db = PathBuf::from(&db_path);
        if !db.is_file() {
            return Err(format!("Staging database not found: {db_path}"));
        }
        let mod_root = PathBuf::from(&mod_path);
        if !mod_root.is_dir() {
            return Err(format!("Mod path is not a directory: {mod_path}"));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| format!("Open staging DB: {e}"))?;
        import_config_files(&conn, &mod_root, &mod_folder)
    })
    .await
}

fn find_meta_lsx(mod_root: &Path) -> Result<PathBuf, String> {
    // Look in Mods/<anything>/meta.lsx or meta.yaml
    let mods_dir = mod_root.join("Mods");
    if mods_dir.exists() {
        for entry in fs::read_dir(&mods_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let meta_lsx = entry.path().join("meta.lsx");
            if meta_lsx.exists() {
                return Ok(meta_lsx);
            }
            let meta_yaml = entry.path().join("meta.yaml");
            if meta_yaml.exists() {
                return Ok(meta_yaml);
            }
        }
    }

    // Also check directly at mod_root/meta.lsx or meta.yaml
    let direct_lsx = mod_root.join("meta.lsx");
    if direct_lsx.exists() {
        return Ok(direct_lsx);
    }
    let direct_yaml = mod_root.join("meta.yaml");
    if direct_yaml.exists() {
        return Ok(direct_yaml);
    }

    Err("Could not find meta.lsx in the mod folder".to_string())
}

fn find_existing_config(mod_root: &Path, folder: &str) -> Option<String> {
    let se_path = mod_root.join("Mods").join(folder).join("ScriptExtender");

    let yaml_path = se_path.join("CompatibilityFrameworkConfig.yaml");
    if yaml_path.exists() {
        return Some(yaml_path.to_string_lossy().into_owned());
    }

    let json_path = se_path.join("CompatibilityFrameworkConfig.json");
    if json_path.exists() {
        return Some(json_path.to_string_lossy().into_owned());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_to_section() {
        assert_eq!(
            file_to_section("Public/MyMod/Progressions/Progressions.lsx"),
            Some(Section::Progressions)
        );
        assert_eq!(
            file_to_section("Public/MyMod/Lists/SpellLists.lsx"),
            Some(Section::Lists)
        );
        assert_eq!(
            file_to_section("Public/MyMod/Races/Races.lsx"),
            Some(Section::Races)
        );
        assert_eq!(
            file_to_section("Public/MyMod/Feats/Feats.lsx"),
            Some(Section::Feats)
        );
        assert_eq!(
            file_to_section("Public/MyMod/Origins/Origins.lsx"),
            Some(Section::Origins)
        );
        assert_eq!(
            file_to_section("Public/MyMod/Backgrounds/Backgrounds.lsx"),
            Some(Section::Backgrounds)
        );
        assert_eq!(
            file_to_section("Public/MyMod/ActionResourceDefinitions/ActionResourceDefinitions.lsx"),
            Some(Section::ActionResources)
        );
        assert_eq!(file_to_section("Public/MyMod/Unknown/Stuff.lsx"), None);
    }

    /// Helper: create a minimal temp mod directory with meta.lsx so scan_mod
    /// can proceed past the metadata step and reach extra path validation.
    fn create_temp_mod() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let mods_dir = tmp.path().join("Mods").join("TestMod");
        fs::create_dir_all(&mods_dir).unwrap();
        let meta_content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
  <version major="4" minor="0" revision="9" build="331"/>
  <region id="Config">
    <node id="root">
      <children>
        <node id="ModuleInfo">
          <attribute id="Folder" type="LSString" value="TestMod"/>
          <attribute id="Name" type="LSString" value="TestMod"/>
          <attribute id="UUID" type="FixedString" value="00000000-0000-0000-0000-000000000001"/>
          <attribute id="Version64" type="int64" value="36028797018963968"/>
        </node>
      </children>
    </node>
  </region>
</save>"#;
        fs::write(mods_dir.join("meta.lsx"), meta_content).unwrap();
        let mod_path = tmp.path().to_path_buf();
        (tmp, mod_path)
    }

    #[test]
    fn test_extra_scan_paths_nonexistent_rejected() {
        // A nonexistent path should fail canonicalization
        let (_tmp, mod_path) = create_temp_mod();
        let result = scan_mod(
            &mod_path.to_string_lossy(),
            Path::new("nonexistent.sqlite"),
            &["Z:\\this\\path\\does\\not\\exist\\at\\all".to_string()],
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("canonicalize"),
            "Expected canonicalization error, got: {err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_extra_scan_paths_canonicalized() {
        // On Windows, a path with .. components should be canonicalized.
        // We use a real temp dir to ensure canonicalization can succeed.
        let tmp = std::env::temp_dir();
        let nested = tmp.join("cmty_test_canon").join("sub");
        let _ = fs::create_dir_all(&nested);

        // Path with .. that resolves to tmp/cmty_test_canon
        let path_with_dots = nested.join("..").to_string_lossy().to_string();

        let (_tmp_mod, mod_path) = create_temp_mod();
        let result = scan_mod(
            &mod_path.to_string_lossy(),
            Path::new("nonexistent.sqlite"),
            &[path_with_dots],
        );
        // Will fail due to missing vanilla DB or no entries, not canonicalization
        // The key assertion is that canonicalization itself succeeded
        if let Err(err) = &result {
            assert!(
                !err.contains("canonicalize"),
                "Path with .. should have been canonicalized successfully, got: {err}"
            );
        }

        // cleanup
        let _ = fs::remove_dir_all(tmp.join("cmty_test_canon"));
    }

    /// Test 14 (S-ERRTEST): scan_mod with a missing reference DB path
    /// should not panic — it should return Ok with empty section results
    /// because each per-section query failure is caught and logged as a warning.
    #[test]
    fn test_scan_mod_missing_reference_db() {
        let (_tmp, mod_path) = create_temp_mod();
        let nonexistent_db = std::path::Path::new("Z:\\nonexistent\\ref_base.sqlite");
        let result = scan_mod(&mod_path.to_string_lossy(), nonexistent_db, &[]);
        // scan_mod should succeed (not panic) — vanilla queries fail gracefully
        assert!(
            result.is_ok(),
            "scan_mod with missing reference DB should not panic or error, got: {:?}",
            result.err()
        );
        let scan = result.unwrap();
        // With no vanilla data loaded, sections should be empty (no diffs possible
        // unless the mod has entries that would be classified as New)
        assert!(
            !scan.mod_meta.name.is_empty(),
            "mod_meta should still be parsed successfully"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore] // Requires developer mode on Windows to create symlinks
    fn test_extra_scan_paths_symlink_rejected() {
        use std::os::windows::fs as win_fs;

        let tmp = std::env::temp_dir();
        let real_dir = tmp.join("cmty_test_symlink_real");
        let link_dir = tmp.join("cmty_test_symlink_link");

        let _ = fs::create_dir_all(&real_dir);
        let _ = fs::remove_file(&link_dir);
        let _ = fs::remove_dir(&link_dir);

        // Try to create a directory symlink
        if win_fs::symlink_dir(&real_dir, &link_dir).is_ok() {
            let (_tmp_mod, mod_path) = create_temp_mod();
            let result = scan_mod(
                &mod_path.to_string_lossy(),
                Path::new("nonexistent.sqlite"),
                &[link_dir.to_string_lossy().to_string()],
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(
                err.contains("symlink"),
                "Expected symlink rejection error, got: {err}"
            );

            // cleanup
            let _ = fs::remove_dir(&link_dir);
        }
        let _ = fs::remove_dir_all(&real_dir);
    }
}
