//! Auto-suggest platform dependencies from a mod's meta.lsx.

use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::models::ModMeta;

/// A suggested dependency with platform identifiers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencySuggestion {
    /// Human-readable name from meta.lsx
    pub name: String,
    /// UUID from meta.lsx
    pub uuid: String,
    /// Folder name from meta.lsx
    pub folder: String,
    /// Known Nexus mod ID (if mapped), e.g., "1933" for Community Library
    pub nexus_mod_id: Option<String>,
    /// Known mod.io mod ID (if mapped)
    pub modio_mod_id: Option<u64>,
    /// Whether this is the BG3 base game dependency (should be excluded)
    pub is_base_game: bool,
}

/// Well-known BG3 mod dependency mappings.
///
/// These map meta.lsx UUIDs/folders to platform IDs.
/// Extend this list as more well-known mods are identified.
const KNOWN_MODS: &[KnownMod] = &[
    KnownMod {
        uuid: "ed539163-bb70-431b-96a7-f5571f1b0f68",
        folder: "CommunityLibrary",
        name: "Community Library",
        nexus_id: Some("1933"),
        modio_id: None,
        is_base: false,
    },
    KnownMod {
        uuid: "28ac9ce2-7d5f-4fec-b1f0-51b1baa4195e",
        folder: "CompatibilityFramework",
        name: "Compatibility Framework",
        nexus_id: Some("3933"),
        modio_id: None,
        is_base: false,
    },
    // Gustav (base game) — always excluded
    KnownMod {
        uuid: "991c9c7a-fb80-40cb-8f0d-b92d4e80e9b1",
        folder: "Gustav",
        name: "Baldur's Gate 3",
        nexus_id: None,
        modio_id: None,
        is_base: true,
    },
    // GustavDev (base game dev dependency) — always excluded
    KnownMod {
        uuid: "28ac9ce2-2aba-8cda-b3b5-6e922f71b6b8",
        folder: "GustavDev",
        name: "Baldur's Gate 3 (Dev)",
        nexus_id: None,
        modio_id: None,
        is_base: true,
    },
];

#[allow(dead_code)]
struct KnownMod {
    uuid: &'static str,
    folder: &'static str,
    name: &'static str,
    nexus_id: Option<&'static str>,
    modio_id: Option<u64>,
    is_base: bool,
}

/// Given a mod's meta.lsx dependencies, return platform dependency suggestions.
///
/// Filters out base game dependencies and enriches with known platform IDs.
pub fn suggest_dependencies(meta: &ModMeta) -> Vec<DependencySuggestion> {
    meta.dependencies
        .iter()
        .map(|dep| {
            // Try to match against known mods by UUID first, then folder
            let known = KNOWN_MODS
                .iter()
                .find(|k| k.uuid == dep.uuid || k.folder == dep.folder);

            DependencySuggestion {
                name: dep.name.clone(),
                uuid: dep.uuid.clone(),
                folder: dep.folder.clone(),
                nexus_mod_id: known.and_then(|k| k.nexus_id.map(|s| s.to_string())),
                modio_mod_id: known.and_then(|k| k.modio_id),
                is_base_game: known.map(|k| k.is_base).unwrap_or(false),
            }
        })
        .collect()
}

/// IPC command to get dependency suggestions for a mod.
#[tauri::command]
pub async fn cmd_suggest_dependencies(
    mod_path: String,
) -> Result<Vec<DependencySuggestion>, AppError> {
    crate::blocking(move || {
        let meta = crate::commands::paths::read_mod_meta(&mod_path)
            .map_err(|e| format!("Failed to read mod meta: {e}"))?;
        Ok(suggest_dependencies(&meta))
    })
    .await
}
