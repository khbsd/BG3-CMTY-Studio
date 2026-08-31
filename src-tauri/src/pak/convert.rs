use std::path::Path;

use crate::pak::error::{PakError, PakResult};
use crate::pak::path::PakPath;

/// Directories under Public/ whose .lsx files the game expects as binary .lsf.
const LSF_REQUIRED_DIRS: &[&str] = &[
    "RootTemplates",
    "GameObjects",
    "TimelineTemplates",
    "Voice",
    "Timeline",
    "MultiEffectInfos",
    "Localization",
    "Generated",
    "Flags",
    "Animation",
    "Tags",
    "Content",
];

/// Determine whether a pak entry should be converted from LSX to LSF binary.
///
/// Returns true if:
/// - The file has a `.lsx` extension
/// - The file is NOT `meta.lsx`
/// - The pak path contains one of the LSF_REQUIRED_DIRS segments
pub fn should_convert_to_lsf(pak_path: &PakPath) -> bool {
    // Only .lsx files can be converted
    if pak_path.extension() != Some("lsx") {
        return false;
    }

    // meta.lsx is NEVER converted — it stays as .lsx
    let name = pak_path.file_name().to_ascii_lowercase();
    if name == "meta.lsx" {
        return false;
    }

    // Check if the file is under a directory that requires binary format
    let lower = pak_path.as_str().to_ascii_lowercase();

    // Exclude Stats/Generated — only top-level Generated/ should convert
    if lower.contains("/stats/generated/") {
        return false;
    }

    for dir in LSF_REQUIRED_DIRS {
        if lower.contains(&format!("/{}/", dir.to_ascii_lowercase())) {
            return true;
        }
    }

    false
}

/// Determine whether an XML file under Mods/*/Localization/ should be converted
/// to binary `.loca`.
///
/// Returns true if the file has a `.xml` extension and its pak path is under
/// `Mods/<something>/Localization/`. Files under `Public/Localization/` are NOT
/// converted (those use LSF, not loca binary).
pub fn should_convert_loca_xml(pak_path: &PakPath) -> bool {
    let s = pak_path.as_str();
    if !s.ends_with(".xml") {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    lower.starts_with("mods/") && lower.contains("/localization/")
}

/// Convert an XML pak path under Mods/*/Localization/ to `.loca`.
///
/// - `.loca.xml` → `.loca` (strips the trailing `.xml`)
/// - `.xml`      → `.loca` (replaces `.xml` with `.loca`)
pub fn convert_pak_path_loca(pak_path: &PakPath) -> PakResult<PakPath> {
    let s = pak_path.as_str();
    if !s.ends_with(".xml") {
        return Err(PakError::invalid_path(format!(
            "cannot convert non-.xml path: {s}"
        )));
    }
    let new_path = if s.ends_with(".loca.xml") {
        // .loca.xml → .loca (strip ".xml")
        s[..s.len() - 4].to_string()
    } else {
        // .xml → .loca (replace ".xml" with ".loca")
        format!("{}.loca", &s[..s.len() - 4])
    };
    PakPath::parse(&new_path)
}

/// Read a `.loca.xml` file from disk and convert it to binary `.loca` bytes.
pub fn convert_loca_xml_file_to_binary(disk_path: &Path) -> Result<Vec<u8>, String> {
    crate::parsers::loca::loca_xml_to_binary(disk_path)
}

/// Convert a .lsx pak path to .lsf extension.
///
/// Handles the `.lsf.lsx` double-extension pattern (common in BG3 mod sources)
/// by stripping just the `.lsx` suffix, avoiding a `.lsf.lsf` result.
pub fn convert_pak_path_to_lsf(pak_path: &PakPath) -> PakResult<PakPath> {
    let s = pak_path.as_str();
    if !s.ends_with(".lsx") {
        return Err(PakError::invalid_path(format!(
            "cannot convert non-.lsx path to .lsf: {s}"
        )));
    }
    let base = &s[..s.len() - 4]; // strip ".lsx"
    let new_path = if base.ends_with(".lsf") {
        // .lsf.lsx → .lsf (just drop the .lsx)
        base.to_string()
    } else {
        format!("{base}.lsf")
    };
    PakPath::parse(&new_path)
}

/// Read an LSX file from disk and convert it to LSF binary bytes.
///
/// Returns the LSF binary data suitable for adding to a pak archive.
/// Errors include the file path for diagnostics.
pub fn convert_lsx_file_to_lsf_bytes(disk_path: &Path) -> Result<Vec<u8>, String> {
    let content = std::fs::read_to_string(disk_path)
        .map_err(|e| format!("Failed to read {}: {}", disk_path.display(), e))?;

    let resource = crate::parsers::lsx::parse_lsx_resource(&content)
        .map_err(|e| format!("Failed to parse LSX {}: {}", disk_path.display(), e))?;

    let mut buf = Vec::new();
    crate::parsers::lsf::write_lsf(&mut buf, &resource)
        .map_err(|e| format!("Failed to write LSF for {}: {}", disk_path.display(), e))?;

    Ok(buf)
}

/// Determine whether a pak entry is an `.lsfx.lsx` file that should be
/// converted to binary `.lsfx` during packaging.
///
/// These are effect bank XML files that the game expects in binary LSF format
/// with the `.lsfx` extension.
pub fn should_convert_to_lsfx(pak_path: &PakPath) -> bool {
    pak_path.as_str().ends_with(".lsfx.lsx")
}

/// Convert an `.lsfx.lsx` pak path to `.lsfx` extension.
pub fn convert_pak_path_to_lsfx(pak_path: &PakPath) -> PakResult<PakPath> {
    let s = pak_path.as_str();
    if !s.ends_with(".lsfx.lsx") {
        return Err(PakError::invalid_path(format!(
            "cannot convert non-.lsfx.lsx path to .lsfx: {s}"
        )));
    }
    // Strip the trailing ".lsx" to get ".lsfx"
    PakPath::parse(&s[..s.len() - 4])
}

/// Read an `.lsfx.lsx` file from disk and convert it to binary LSFX bytes.
///
/// The binary format is identical to LSF.
pub fn convert_lsfx_lsx_file_to_binary(disk_path: &Path) -> Result<Vec<u8>, String> {
    // LSFX binary format is identical to LSF
    convert_lsx_file_to_lsf_bytes(disk_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_convert_root_templates() {
        let path = PakPath::parse("Public/MyMod/RootTemplates/foo.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_game_objects() {
        let path = PakPath::parse("Public/MyMod/GameObjects/bar.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_timeline_templates() {
        let path = PakPath::parse("Public/MyMod/TimelineTemplates/baz.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_voice() {
        let path = PakPath::parse("Public/MyMod/Voice/some_dialog.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_tags() {
        let path = PakPath::parse("Public/MyMod/Tags/MyTag.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_content() {
        let path = PakPath::parse("Public/MyMod/Content/Items/MyItem.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_generated() {
        let path = PakPath::parse("Public/MyMod/Generated/foo.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_not_convert_stats_generated() {
        let path = PakPath::parse("Public/MyMod/Stats/Generated/Data/foo.lsx").unwrap();
        assert!(!should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_flags() {
        let path = PakPath::parse("Public/MyMod/Flags/MyFlag.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_convert_animation() {
        let path = PakPath::parse("Public/MyMod/Animation/some_anim.lsx").unwrap();
        assert!(should_convert_to_lsf(&path));
    }

    #[test]
    fn should_not_convert_meta_lsx() {
        let path = PakPath::parse("Mods/MyMod/meta.lsx").unwrap();
        assert!(!should_convert_to_lsf(&path));
    }

    #[test]
    fn should_not_convert_non_lsx() {
        let path = PakPath::parse("Public/MyMod/RootTemplates/foo.xml").unwrap();
        assert!(!should_convert_to_lsf(&path));
    }

    #[test]
    fn should_not_convert_races_lsx() {
        let path = PakPath::parse("Public/MyMod/Races/Races.lsx").unwrap();
        assert!(!should_convert_to_lsf(&path));
    }

    #[test]
    fn convert_path_lsx_to_lsf() {
        let path = PakPath::parse("Public/MyMod/RootTemplates/foo.lsx").unwrap();
        let converted = convert_pak_path_to_lsf(&path).unwrap();
        assert_eq!(converted.as_str(), "Public/MyMod/RootTemplates/foo.lsf");
    }

    #[test]
    fn convert_path_double_ext_lsf_lsx_to_lsf() {
        // Source files often have .lsf.lsx (e.g. Tags, Content _merged files)
        let path = PakPath::parse("Public/MyMod/Tags/abc-def.lsf.lsx").unwrap();
        let converted = convert_pak_path_to_lsf(&path).unwrap();
        assert_eq!(converted.as_str(), "Public/MyMod/Tags/abc-def.lsf");
    }

    #[test]
    fn convert_path_non_lsx_fails() {
        let path = PakPath::parse("Public/MyMod/RootTemplates/foo.xml").unwrap();
        assert!(convert_pak_path_to_lsf(&path).is_err());
    }

    #[test]
    fn should_convert_loca_xml_true() {
        let path = PakPath::parse("Mods/MyMod/Localization/English/Strings.loca.xml").unwrap();
        assert!(should_convert_loca_xml(&path));
    }

    #[test]
    fn should_convert_plain_xml_under_mods_localization() {
        let path = PakPath::parse("Mods/MyMod/Localization/English/SomeFile.xml").unwrap();
        assert!(should_convert_loca_xml(&path));
    }

    #[test]
    fn should_not_convert_xml_under_public_localization() {
        let path = PakPath::parse("Public/MyMod/Localization/English/SomeFile.xml").unwrap();
        assert!(!should_convert_loca_xml(&path));
    }

    #[test]
    fn should_not_convert_loca_binary() {
        let path = PakPath::parse("Mods/MyMod/Localization/English/Strings.loca").unwrap();
        assert!(!should_convert_loca_xml(&path));
    }

    #[test]
    fn should_not_convert_xml_outside_localization() {
        let path = PakPath::parse("Mods/MyMod/foo.xml").unwrap();
        assert!(!should_convert_loca_xml(&path));
    }

    #[test]
    fn convert_loca_xml_path() {
        let path = PakPath::parse("Mods/MyMod/Localization/English/Strings.loca.xml").unwrap();
        let converted = convert_pak_path_loca(&path).unwrap();
        assert_eq!(
            converted.as_str(),
            "Mods/MyMod/Localization/English/Strings.loca"
        );
    }

    #[test]
    fn convert_plain_xml_to_loca() {
        let path = PakPath::parse("Mods/MyMod/Localization/English/SomeFile.xml").unwrap();
        let converted = convert_pak_path_loca(&path).unwrap();
        assert_eq!(
            converted.as_str(),
            "Mods/MyMod/Localization/English/SomeFile.loca"
        );
    }

    #[test]
    fn convert_loca_xml_path_non_xml_fails() {
        let path = PakPath::parse("Mods/MyMod/Localization/English/Strings.loca").unwrap();
        assert!(convert_pak_path_loca(&path).is_err());
    }

    #[test]
    fn should_convert_lsfx_lsx() {
        let path = PakPath::parse("Public/MyMod/Content/Assets/Effects/MyEffect.lsfx.lsx").unwrap();
        assert!(should_convert_to_lsfx(&path));
    }

    #[test]
    fn should_not_convert_plain_lsx_as_lsfx() {
        let path = PakPath::parse("Public/MyMod/Content/Assets/Effects/MyEffect.lsx").unwrap();
        assert!(!should_convert_to_lsfx(&path));
    }

    #[test]
    fn should_not_convert_binary_lsfx() {
        let path = PakPath::parse("Public/MyMod/Content/Assets/Effects/MyEffect.lsfx").unwrap();
        assert!(!should_convert_to_lsfx(&path));
    }

    #[test]
    fn convert_lsfx_path() {
        let path = PakPath::parse("Public/MyMod/Content/Assets/Effects/MyEffect.lsfx.lsx").unwrap();
        let converted = convert_pak_path_to_lsfx(&path).unwrap();
        assert_eq!(
            converted.as_str(),
            "Public/MyMod/Content/Assets/Effects/MyEffect.lsfx"
        );
    }

    #[test]
    fn convert_lsfx_path_non_lsfx_fails() {
        let path = PakPath::parse("Public/MyMod/Content/foo.lsx").unwrap();
        assert!(convert_pak_path_to_lsfx(&path).is_err());
    }
}
