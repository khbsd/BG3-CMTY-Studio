//! Path, filesystem, and section-classification helpers.
//!
//! These functions were originally in `divine.rs` but have no relationship
//! to the Divine.exe subprocess. They provide general-purpose path resolution,
//! directory sizing, mod metadata reading, and pak section classification.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

/// Relevant subfolders under the public roots for vanilla extraction.
const VANILLA_SUBFOLDERS: &[&str] = &[
    "ActionResourceDefinitions",
    "ActionResourceGroupDefinitions",
    "Animation",
    "AnimationOverrides",
    "Backgrounds",
    "BackgroundGoals",
    "Calendar",
    "CharacterCreation",
    "CharacterCreationPresets",
    "CinematicArenaFrequencyGroups",
    "ClassDescriptions",
    "ColorDefinitions",
    "CombatCameraGroups",
    "Content",
    "CustomDice",
    "DefaultValues",
    "DifficultyClasses",
    "Disturbances",
    "Encumbrance",
    "EquipmentTypes",
    "Factions",
    "FeatDescriptions",
    "Feats",
    "Flags",
    "FixedHotBarSlots",
    "Gods",
    "GUI",
    "ItemThrowParams",
    "Levelmaps",
    "LimbsMapping",
    "Lists",
    "MultiEffectInfos",
    "Origins",
    "Progressions",
    "ProjectileDefaults",
    "Races",
    "RandomCasts",
    "RootTemplates",
    "Ruleset",
    "Shapeshift",
    "Sound",
    "Spell",
    "Status",
    "Surface",
    "Tags",
    "TooltipExtras",
    "TrajectoryRules",
    "Tutorials",
    "VFX",
    "Visuals",
    "Voices",
    "WeaponAnimationSetData",
];

/// Get the full VANILLA_SUBFOLDERS list (for use from other modules).
pub fn vanilla_subfolders() -> &'static [&'static str] {
    VANILLA_SUBFOLDERS
}

/// Compute the total size (in bytes) of all files in a directory tree.
pub fn dir_size_bytes(dir_path: &str) -> Result<u64, String> {
    let root = Path::new(dir_path);
    if !root.exists() {
        return Ok(0);
    }
    let total: u64 = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    Ok(total)
}

/// Read a mod's meta.lsx or meta.yaml file given a mod path (directory).
/// Looks for meta.lsx first, then meta.yaml, in the standard BG3 mod layout.
pub fn read_mod_meta(mod_path: &str) -> Result<crate::models::ModMeta, String> {
    let base = Path::new(mod_path);
    // Walk for meta.lsx first (original format)
    if let Some(entry) = WalkDir::new(base)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_type().is_file()
                && e.path()
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("meta.lsx"))
        })
    {
        let content = fs::read_to_string(entry.path())
            .map_err(|e| format!("Failed to read meta.lsx: {e}"))?;
        return crate::parsers::meta::parse_meta_lsx(&content);
    }
    // Fall back to meta.yaml (converted format)
    if let Some(entry) = WalkDir::new(base)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_type().is_file()
                && e.path()
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("meta.yaml"))
        })
    {
        let content = fs::read_to_string(entry.path())
            .map_err(|e| format!("Failed to read meta.yaml: {e}"))?;
        return crate::parsers::meta::parse_meta_yaml(&content);
    }
    Err("meta.lsx not found".to_string())
}

/// Information about which data sections a pak file contains.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct PakSectionInfo {
    /// The section folder name (e.g. "Progressions", "Races").
    pub folder: String,
    /// Number of files in this section.
    pub file_count: usize,
}

/// Categorize a list of file paths from a pak into data sections.
/// Returns section info for each detected section folder.
pub fn categorize_pak_sections(file_paths: &[String]) -> Vec<PakSectionInfo> {
    let mut section_counts: HashMap<String, usize> = HashMap::new();

    for path in file_paths {
        // Normalize path separators
        let normalized = path.replace('\\', "/");

        // Match Public/<root>/<SectionFolder>/... pattern
        if let Some(public_idx) = normalized.find("Public/") {
            let after_public = &normalized[public_idx + 7..]; // skip "Public/"
                                                              // Skip the root name (e.g. "Shared/", "ModName/")
            if let Some(slash_idx) = after_public.find('/') {
                let after_root = &after_public[slash_idx + 1..];
                // Get the section folder name
                if let Some(folder_end) = after_root.find('/') {
                    let folder = &after_root[..folder_end];
                    // Check if it matches a known subfolder
                    if VANILLA_SUBFOLDERS.contains(&folder) || folder == "Stats" {
                        *section_counts.entry(folder.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }

        // Also check for Localization files
        if normalized.starts_with("Localization/") || normalized.contains("/Localization/") {
            *section_counts
                .entry("Localization".to_string())
                .or_insert(0) += 1;
        }

        // Check for meta files
        if normalized.contains("Mods/")
            && (normalized.ends_with("meta.lsx") || normalized.ends_with("meta.lsf"))
        {
            *section_counts.entry("Meta".to_string()).or_insert(0) += 1;
        }
    }

    let mut results: Vec<PakSectionInfo> = section_counts
        .into_iter()
        .map(|(folder, file_count)| PakSectionInfo { folder, file_count })
        .collect();

    results.sort_by(|a, b| a.folder.cmp(&b.folder));
    results
}

// ── Mod-folder detection within a project folder ────────────────

/// A single mod discovered inside a project folder.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct DetectedMod {
    /// Absolute path to the ModFolder.
    pub mod_path: String,
    /// Mod name from meta.lsx / meta.yaml.
    pub mod_name: String,
    /// UUID from meta.lsx / meta.yaml.
    pub mod_uuid: String,
    /// `Folder` attribute from meta.lsx / meta.yaml.
    pub mod_folder: String,
    /// Author from meta.lsx / meta.yaml.
    pub author: String,
    /// Whether the effective project_path contains a `.git/` directory.
    pub has_git: bool,
    /// Whether `.cmtystudio/nexus.json` exists with a mod ID.
    pub has_nexus: bool,
    /// Whether `.cmtystudio/modio.json` exists with a selected mod ID.
    pub has_modio: bool,
}

/// Result of scanning a project folder for mod sub-folders.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct DetectResult {
    /// Effective ProjectFolder (may differ from input if auto-corrected).
    pub project_path: String,
    /// Detected ModFolders within `project_path`.
    pub mods: Vec<DetectedMod>,
}

/// Check whether `dir` looks like a ModFolder (has `Mods/*/meta.lsx` or
/// `Mods/*/meta.yaml` inside).
fn dir_is_mod_folder(dir: &Path) -> bool {
    let mods_dir = dir.join("Mods");
    if !mods_dir.is_dir() {
        return false;
    }
    let Ok(entries) = fs::read_dir(&mods_dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let p = entry.path();
        if p.join("meta.lsx").is_file() || p.join("meta.yaml").is_file() {
            return true;
        }
    }
    false
}

/// Try to build a [`DetectedMod`] from a directory that is a ModFolder.
fn try_detect_mod(dir: &Path, project_root: &Path, has_git: bool) -> Option<DetectedMod> {
    let mod_path_str = dir.to_string_lossy().to_string();
    let dir_name = dir.file_name()?.to_string_lossy().to_string();
    match read_mod_meta(&mod_path_str) {
        Ok(meta) => {
            let has_nexus = has_platform_config(project_root, &dir_name, "nexus", "modId");
            let has_modio = has_platform_config(project_root, &dir_name, "modio", "selectedModId");
            Some(DetectedMod {
                mod_path: mod_path_str,
                mod_name: meta.name,
                mod_uuid: meta.uuid,
                mod_folder: meta.folder,
                author: meta.author,
                has_git,
                has_nexus,
                has_modio,
            })
        }
        Err(_) => None,
    }
}

/// Check if `platforms.json` has a config for a given mod+platform+key.
fn has_platform_config(project_root: &Path, mod_dir_name: &str, platform: &str, key: &str) -> bool {
    let path = project_root.join(".cmtystudio").join("platforms.json");
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            return match &val[mod_dir_name][platform][key] {
                serde_json::Value::Number(n) => n.as_u64().is_some_and(|v| v > 0),
                serde_json::Value::String(s) => !s.is_empty(),
                _ => false,
            };
        }
    }
    // Fallback: check legacy per-platform files (pre-migration)
    let legacy_path = project_root.join(".cmtystudio").join(match platform {
        "nexus" => "nexus.json",
        "modio" => "modio.json",
        _ => return false,
    });
    if let Ok(content) = fs::read_to_string(&legacy_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            return match &val[key] {
                serde_json::Value::Number(n) => n.as_u64().is_some_and(|v| v > 0),
                serde_json::Value::String(s) => !s.is_empty(),
                _ => false,
            };
        }
    }
    false
}

/// Scan a project folder for valid ModFolders.
///
/// 1. Check immediate children (depth 1) for subdirectories that are
///    ModFolders (`Mods/*/meta.lsx` or `meta.yaml`).
/// 2. If none found, check whether the input folder **itself** is a
///    ModFolder. If so, treat its parent as the effective project path.
/// 3. Return a [`DetectResult`] — `mods` may be empty.
pub fn detect_mod_folders(project_path: &str) -> Result<DetectResult, String> {
    let root = Path::new(project_path);
    if !root.is_dir() {
        return Err(format!("Path is not a directory: {project_path}"));
    }

    // Phase 1: scan immediate children
    let mut detected: Vec<DetectedMod> = Vec::new();
    let has_git = root.join(".git").is_dir();

    let entries = fs::read_dir(root).map_err(|e| format!("Failed to read directory: {e}"))?;

    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() && dir_is_mod_folder(&child) {
            if let Some(dm) = try_detect_mod(&child, root, has_git) {
                detected.push(dm);
            }
        }
    }

    if !detected.is_empty() {
        return Ok(DetectResult {
            project_path: project_path.to_string(),
            mods: detected,
        });
    }

    // Phase 2: the selected folder itself might be a ModFolder
    if dir_is_mod_folder(root) {
        let effective_project = root
            .parent()
            .ok_or_else(|| "Selected folder has no parent directory".to_string())?;
        let effective_str = effective_project.to_string_lossy().to_string();
        let parent_has_git = effective_project.join(".git").is_dir();

        if let Some(dm) = try_detect_mod(root, effective_project, parent_has_git) {
            return Ok(DetectResult {
                project_path: effective_str,
                mods: vec![dm],
            });
        }
    }

    // Phase 3: nothing found — return empty result (not an error)
    Ok(DetectResult {
        project_path: project_path.to_string(),
        mods: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── dir_size_bytes ──────────────────────────────────────────

    #[test]
    fn dir_size_bytes_nonexistent_returns_zero() {
        let result = dir_size_bytes("/definitely/does/not/exist/xyz");
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn dir_size_bytes_empty_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = dir_size_bytes(tmp.path().to_str().unwrap());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn dir_size_bytes_with_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let content_a = b"hello";
        let content_b = b"world!!!";
        fs::write(tmp.path().join("a.txt"), content_a).expect("write a");
        fs::write(tmp.path().join("b.txt"), content_b).expect("write b");

        let result = dir_size_bytes(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(result, (content_a.len() + content_b.len()) as u64);
    }

    #[test]
    fn dir_size_bytes_nested_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub).expect("mkdir");
        fs::write(sub.join("nested.dat"), b"1234567890").expect("write");

        let result = dir_size_bytes(tmp.path().to_str().unwrap()).unwrap();
        assert_eq!(result, 10);
    }

    // ── categorize_pak_sections ─────────────────────────────────

    #[test]
    fn categorize_pak_sections_empty_input() {
        let result = categorize_pak_sections(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn categorize_pak_sections_known_folders() {
        let paths = vec![
            "Public/MyMod/Progressions/prog1.lsx".to_string(),
            "Public/MyMod/Progressions/prog2.lsx".to_string(),
            "Public/MyMod/Races/race1.lsx".to_string(),
        ];
        let result = categorize_pak_sections(&paths);

        let prog = result
            .iter()
            .find(|s| s.folder == "Progressions")
            .expect("Progressions");
        assert_eq!(prog.file_count, 2);

        let races = result.iter().find(|s| s.folder == "Races").expect("Races");
        assert_eq!(races.file_count, 1);
    }

    #[test]
    fn categorize_pak_sections_stats_folder() {
        let paths = vec![
            "Public/MyMod/Stats/Generated/Data/Spell_Projectile.txt".to_string(),
            "Public/MyMod/Stats/Generated/Data/Passive.txt".to_string(),
        ];
        let result = categorize_pak_sections(&paths);

        let stats = result
            .iter()
            .find(|s| s.folder == "Stats")
            .expect("Stats section");
        assert_eq!(stats.file_count, 2);
    }

    #[test]
    fn categorize_pak_sections_localization() {
        let paths = vec![
            "Localization/English/en.xml".to_string(),
            "Data/Localization/English/strings.xml".to_string(),
        ];
        let result = categorize_pak_sections(&paths);

        let loca = result
            .iter()
            .find(|s| s.folder == "Localization")
            .expect("Localization");
        assert_eq!(loca.file_count, 2);
    }

    #[test]
    fn categorize_pak_sections_meta() {
        let paths = vec![
            "Mods/MyMod/meta.lsx".to_string(),
            "Mods/SomeOther/meta.lsf".to_string(),
        ];
        let result = categorize_pak_sections(&paths);

        let meta = result
            .iter()
            .find(|s| s.folder == "Meta")
            .expect("Meta section");
        assert_eq!(meta.file_count, 2);
    }

    #[test]
    fn categorize_pak_sections_unrecognized_folder_ignored() {
        let paths = vec!["Public/MyMod/SomeRandomFolder/data.lsx".to_string()];
        let result = categorize_pak_sections(&paths);
        assert!(result.is_empty(), "unrecognized folders should be ignored");
    }

    #[test]
    fn categorize_pak_sections_backslash_normalization() {
        let paths = vec!["Public\\MyMod\\Feats\\feat1.lsx".to_string()];
        let result = categorize_pak_sections(&paths);

        let feats = result.iter().find(|s| s.folder == "Feats").expect("Feats");
        assert_eq!(feats.file_count, 1);
    }

    #[test]
    fn categorize_pak_sections_sorted_output() {
        let paths = vec![
            "Public/MyMod/Tags/tag1.lsx".to_string(),
            "Public/MyMod/Feats/feat1.lsx".to_string(),
            "Public/MyMod/Animation/anim1.lsx".to_string(),
        ];
        let result = categorize_pak_sections(&paths);
        let folders: Vec<&str> = result.iter().map(|s| s.folder.as_str()).collect();
        assert_eq!(folders, &["Animation", "Feats", "Tags"]);
    }

    // ── read_mod_meta ───────────────────────────────────────────

    #[test]
    fn read_mod_meta_no_meta_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let result = read_mod_meta(tmp.path().to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("meta.lsx not found"));
    }

    #[test]
    fn read_mod_meta_finds_lsx() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mods_dir = tmp.path().join("Mods").join("TestMod");
        fs::create_dir_all(&mods_dir).expect("mkdir");

        let meta_content = r#"<?xml version="1.0" encoding="utf-8"?>
<save>
  <version major="4" minor="0" revision="0" build="49"/>
  <region id="Config">
    <node id="root">
      <children>
        <node id="ModuleInfo">
          <attribute id="UUID" type="FixedString" value="test-uuid-1234"/>
          <attribute id="Folder" type="LSString" value="TestMod"/>
          <attribute id="Name" type="LSString" value="Test Mod Name"/>
          <attribute id="Author" type="LSString" value="Author"/>
          <attribute id="Version64" type="int64" value="36028797018963968"/>
          <attribute id="Description" type="LSString" value="A test mod"/>
          <attribute id="MD5" type="LSString" value=""/>
          <attribute id="Type" type="FixedString" value="Add-on"/>
          <attribute id="Tags" type="LSString" value=""/>
          <attribute id="NumPlayers" type="uint8" value="4"/>
          <attribute id="GMTemplate" type="LSString" value=""/>
          <attribute id="CharacterCreationLevelName" type="FixedString" value=""/>
          <attribute id="LobbyLevelName" type="FixedString" value=""/>
          <attribute id="MenuLevelName" type="FixedString" value=""/>
          <attribute id="StartupLevelName" type="FixedString" value=""/>
          <attribute id="PhotoBooth" type="FixedString" value=""/>
          <attribute id="MainMenuBackgroundVideo" type="FixedString" value=""/>
          <attribute id="PublishVersion" type="int64" value="0"/>
          <attribute id="TargetMode" type="FixedString" value="Story"/>
        </node>
      </children>
    </node>
  </region>
</save>"#;
        fs::write(mods_dir.join("meta.lsx"), meta_content).expect("write meta");

        let meta = read_mod_meta(tmp.path().to_str().unwrap()).expect("should parse meta");
        assert_eq!(meta.uuid, "test-uuid-1234");
        assert_eq!(meta.folder, "TestMod");
        assert_eq!(meta.name, "Test Mod Name");
    }

    // ── vanilla_subfolders ──────────────────────────────────────

    #[test]
    fn vanilla_subfolders_non_empty() {
        let subs = vanilla_subfolders();
        assert!(!subs.is_empty());
        assert!(subs.contains(&"Progressions"));
        assert!(subs.contains(&"Races"));
        assert!(subs.contains(&"Feats"));
    }
}
