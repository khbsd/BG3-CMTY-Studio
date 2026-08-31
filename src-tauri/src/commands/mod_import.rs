//! Native mod-import operations.
//!
//! Provides metadata extraction from `.pak` files and type definitions for
//! the mod processing pipeline. Data is ingested directly into SQLite databases
//! — no files are extracted to disk.

use std::io::Cursor;
use std::path::Path;

use crate::models::ModMeta;
use crate::pak::reader::PakReader;
use crate::parsers::lsf::parse_lsf;
use crate::parsers::meta::{parse_meta_from_resource, parse_meta_lsx};

/// Result of populating game data databases.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct PopulateResult {
    pub paks_extracted: usize,
    pub files_kept: usize,
    pub lsf_converted: usize,
    pub unpack_path: String,
    pub errors: Vec<String>,
    /// Diagnostic details showing exactly what was searched for.
    pub diagnostics: Vec<String>,
}

/// Result of processing a mod folder.
#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct ModProcessResult {
    pub lsf_converted: usize,
    pub unpack_path: String,
    pub errors: Vec<String>,
    /// Parsed mod metadata (extracted from meta.lsx inside .pak files).
    /// `None` when processing a plain directory that already has meta.lsx accessible.
    pub mod_meta: Option<ModMeta>,
    /// Whether the mod has a Public folder (mods without one are unlikely to have CF-relevant content).
    pub has_public_folder: bool,
}

/// Maximum size for a meta.lsx/meta.lsf file (1 MB — well above any real meta file).
const MAX_META_BYTES: usize = 1_024 * 1_024;

/// Open a `.pak` file and extract `ModMeta` from the first `meta.lsf` or `meta.lsx`
/// entry found under `Mods/*/`. Returns `Ok(None)` if no meta file is present.
pub fn read_meta_from_pak(pak_path: &Path) -> Result<Option<ModMeta>, String> {
    let reader = PakReader::open(pak_path).map_err(|e| format!("Failed to open pak: {e}"))?;

    // Look for meta.lsf first (binary — more common in shipped mods), then meta.lsx
    let mut meta_lsf: Option<&crate::pak::entry::PakEntry> = None;
    let mut meta_lsx: Option<&crate::pak::entry::PakEntry> = None;

    for entry in reader.entries() {
        if entry.is_deleted() {
            continue;
        }
        let path_str = entry.path.as_str();
        // Match Mods/{something}/meta.lsf or meta.lsx (case-insensitive filename)
        if !path_str.starts_with("Mods/") {
            continue;
        }
        let filename = path_str.rsplit('/').next().unwrap_or("");
        if filename.eq_ignore_ascii_case("meta.lsf") && meta_lsf.is_none() {
            meta_lsf = Some(entry);
        } else if filename.eq_ignore_ascii_case("meta.lsx") && meta_lsx.is_none() {
            meta_lsx = Some(entry);
        }
    }

    // Prefer .lsf (binary) — native parser handles it directly
    if let Some(entry) = meta_lsf.or(meta_lsx) {
        let is_binary = entry
            .path
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or("")
            .eq_ignore_ascii_case("meta.lsf");

        let mut entry_reader = reader
            .open_entry(entry)
            .map_err(|e| format!("Failed to open meta entry: {e}"))?;

        let bytes = entry_reader
            .read_to_end_with_limit(MAX_META_BYTES)
            .map_err(|e| format!("Failed to read meta entry: {e}"))?;

        let meta = if is_binary {
            let cursor = Cursor::new(bytes);
            let resource =
                parse_lsf(cursor).map_err(|e| format!("Failed to parse meta.lsf: {e}"))?;
            parse_meta_from_resource(&resource)?
        } else {
            let content = String::from_utf8(bytes)
                .map_err(|e| format!("meta.lsx is not valid UTF-8: {e}"))?;
            parse_meta_lsx(&content)?
        };

        return Ok(Some(meta));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integration test: read meta from a real mod .pak if available.
    /// Skipped when the test pak isn't present.
    #[test]
    fn test_read_meta_from_pak_real() {
        let pak = Path::new("h:\\BG3\\Mods\\AGtTPreview\\AdventurersGuideToTamriel.pak");
        if !pak.exists() {
            eprintln!("Skipping: test pak not found at {pak:?}");
            return;
        }
        let meta = read_meta_from_pak(pak).expect("failed to read pak");
        let meta = meta.expect("no meta found in pak");
        assert!(!meta.name.is_empty(), "mod name should not be empty");
        assert!(!meta.uuid.is_empty(), "mod UUID should not be empty");
        eprintln!("Mod: {} ({})", meta.name, meta.uuid);
        eprintln!("  Folder: {}", meta.folder);
        eprintln!("  Author: {}", meta.author);
        eprintln!("  Version: {}", meta.version);
        eprintln!("  Dependencies: {}", meta.dependencies.len());
        for dep in &meta.dependencies {
            eprintln!("    - {} ({})", dep.name, dep.uuid);
        }
    }
}
