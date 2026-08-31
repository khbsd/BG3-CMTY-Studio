//! Pre-packaging merge operations.
//!
//! Before files are added to a `.pak` archive, several consolidation steps must
//! happen to match the game's expected binary layout:
//!
//! 1. **RootTemplates merge** — All `.lsx` / `.lsf` files under `RootTemplates/`
//!    are parsed, their nodes combined, and written as a single `_merged.lsf`.
//! 2. **REGIONTYPE.lsx reconstruction** — In specific directories (Lists, Races,
//!    ClassDescriptions, ActionResourceDefinitions, Progressions), files named
//!    `Name.REGIONTYPE.lsx` are merged into `REGIONTYPE.lsx` at the directory
//!    root. These stay as `.lsx` (never converted to `.lsf`).
//! 3. **Stats condensation** — `.txt` stat files under `Stats/Generated/Data/`
//!    are grouped by top-level subdirectory and merged into single files with
//!    comment headers preserving original file boundaries.
//! 4. **Double-extension fix** — `.lsf.lsf` paths are normalised to `.lsf`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::models::{LsxNode, LsxNodeAttribute, LsxResource};
use crate::pak::path::PakPath;

// ─── Constants ───────────────────────────────────────────────────────────────

/// Directories whose `Name.REGIONTYPE.lsx` files should be merged into
/// `REGIONTYPE.lsx`. These merged files stay as `.lsx` — they are NEVER
/// converted to binary `.lsf`.
const REGIONTYPE_MERGE_DIRS: &[&str] = &[
    "Lists",
    "Races",
    "ClassDescriptions",
    "ActionResourceDefinitions",
    "Progressions",
];

// ─── Public API ──────────────────────────────────────────────────────────────

/// The result of merge planning: which source entries to skip and what merged
/// entries to add instead.
pub struct MergePlan {
    /// Indices into the original `file_entries` slice that should be skipped
    /// (their content is folded into a merged entry).
    pub skip_indices: HashSet<usize>,

    /// New entries produced by merging. Each is `(pak_path, bytes)`.
    /// Bytes may be binary LSF, LSX XML, or plain text depending on the merge
    /// type.
    pub merged_entries: Vec<(PakPath, Vec<u8>)>,

    /// Pak-path rewrites for double-extension fixes (`.lsf.lsf` → `.lsf`).
    /// Maps original index → corrected PakPath.
    pub path_rewrites: BTreeMap<usize, PakPath>,
}

/// Analyse a list of file entries and compute all merge operations.
///
/// The caller passes the full `(disk_path, pak_path)` list collected during the
/// directory walk. This function:
/// - Reads and parses source files as needed
/// - Produces merged byte blobs
/// - Returns which original entries to skip and what to add in their place
pub fn compute_merges(file_entries: &[(PathBuf, PakPath)]) -> Result<MergePlan, String> {
    let mut plan = MergePlan {
        skip_indices: HashSet::new(),
        merged_entries: Vec::new(),
        path_rewrites: BTreeMap::new(),
    };

    // Fix double extensions first (before other analysis)
    fix_double_extensions(file_entries, &mut plan);

    // Flatten subdirectories under MultiEffectInfos
    flatten_multieffectinfos(file_entries, &mut plan);

    // RootTemplates merge
    merge_root_templates(file_entries, &mut plan)?;

    // REGIONTYPE.lsx reconstruction
    merge_regiontype_files(file_entries, &mut plan)?;

    // Stats condensation
    merge_stats_files(file_entries, &mut plan)?;

    Ok(plan)
}

// ─── Double-extension fix ────────────────────────────────────────────────────

fn fix_double_extensions(file_entries: &[(PathBuf, PakPath)], plan: &mut MergePlan) {
    for (i, (_disk, pak)) in file_entries.iter().enumerate() {
        let s = pak.as_str();
        // Strip one redundant extension: .lsf.lsf → .lsf, .lsfx.lsfx → .lsfx
        let fixed = s
            .strip_suffix(".lsf.lsf")
            .map(|base| format!("{base}.lsf"))
            .or_else(|| {
                s.strip_suffix(".lsfx.lsfx")
                    .map(|base| format!("{base}.lsfx"))
            });
        if let Some(fixed) = fixed {
            if let Ok(new_pak) = PakPath::parse(&fixed) {
                plan.path_rewrites.insert(i, new_pak);
            }
        }
    }
}

// ─── MultiEffectInfos flatten ────────────────────────────────────────────────

/// Flatten subdirectories under `MultiEffectInfos/` so that files like
/// `MultiEffectInfos/SubDir/Foo.lsf.lsx` become `MultiEffectInfos/Foo.lsf.lsx`.
/// The actual `.lsf.lsx` → `.lsf` conversion is handled later by the packaging
/// step.
fn flatten_multieffectinfos(file_entries: &[(PathBuf, PakPath)], plan: &mut MergePlan) {
    for (i, (_disk, pak)) in file_entries.iter().enumerate() {
        let s = pak.as_str();
        let lower = s.to_ascii_lowercase();
        if let Some(pos) = lower.find("/multieffectinfos/") {
            let base_end = pos + "/multieffectinfos/".len();
            let after = &s[base_end..];
            // If there's a subdirectory (contains '/'), flatten by keeping
            // only the filename.
            if let Some(last_slash) = after.rfind('/') {
                let filename = &after[last_slash + 1..];
                let base_dir = &s[..base_end];
                let new_path = format!("{base_dir}{filename}");
                if let Ok(new_pak) = PakPath::parse(&new_path) {
                    plan.path_rewrites.insert(i, new_pak);
                }
            }
        }
    }
}

// ─── RootTemplates merge ─────────────────────────────────────────────────────

/// Find all files under any `RootTemplates/` directory, parse them, merge their
/// nodes, and emit a single `_merged.lsf` binary.
fn merge_root_templates(
    file_entries: &[(PathBuf, PakPath)],
    plan: &mut MergePlan,
) -> Result<(), String> {
    // Group files by the RootTemplates base path:
    //   "Public/ModName/RootTemplates/" → indices
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (i, (_disk, pak)) in file_entries.iter().enumerate() {
        let lower = pak.as_str().to_ascii_lowercase();
        if let Some(pos) = lower.find("/roottemplates/") {
            let base = &pak.as_str()[..pos + "/RootTemplates/".len()];
            // Normalise casing to match original path
            let base_key = pak.as_str()[..pos + "/RootTemplates/".len()].to_string();
            groups.entry(base_key).or_default().push(i);
            let _ = base; // suppress warning
        }
    }

    for (base_path, indices) in &groups {
        if indices.len() <= 1 {
            // Single file — check if it's already `_merged.lsf`; if so, leave alone
            if indices.len() == 1 {
                let pak = &file_entries[indices[0]].1;
                let name = pak.file_name().to_ascii_lowercase();
                if name == "_merged.lsf" {
                    continue;
                }
            }
        }

        let mut merged_resource = LsxResource {
            regions: Vec::new(),
        };

        for &idx in indices {
            let (disk_path, pak_path) = &file_entries[idx];
            let resource = parse_resource_file(disk_path, pak_path)?;
            merge_resource_into(&mut merged_resource, &resource);
            plan.skip_indices.insert(idx);
        }

        if merged_resource.regions.is_empty() {
            continue;
        }

        let merged_pak = PakPath::parse(&format!("{base_path}_merged.lsf"))
            .map_err(|e| format!("Invalid merged path: {e}"))?;

        let mut lsf_bytes = Vec::new();
        crate::parsers::lsf::write_lsf(&mut lsf_bytes, &merged_resource)
            .map_err(|e| format!("Failed to write merged RootTemplates: {e}"))?;

        plan.merged_entries.push((merged_pak, lsf_bytes));
    }

    Ok(())
}

// ─── REGIONTYPE.lsx reconstruction ──────────────────────────────────────────

/// For each file matching `Name.REGIONTYPE.lsx` under a known merge directory,
/// merge all nodes into `Dir/REGIONTYPE.lsx`.
fn merge_regiontype_files(
    file_entries: &[(PathBuf, PakPath)],
    plan: &mut MergePlan,
) -> Result<(), String> {
    // Key: (base_dir_pak_path, region_type_name) → Vec<index>
    //   e.g. ("Public/MyMod/Races/", "Races") → [3, 7, 12]
    let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    // Track which entries are the "base" file (e.g. Races/Races.lsx)
    let mut base_files: BTreeMap<(String, String), usize> = BTreeMap::new();

    for (i, (_disk, pak)) in file_entries.iter().enumerate() {
        // Already skipped by another merge?
        if plan.skip_indices.contains(&i) {
            continue;
        }

        let s = pak.as_str();
        if !s.ends_with(".lsx") {
            continue;
        }

        // Check if this file is under a REGIONTYPE_MERGE_DIR
        let lower = s.to_ascii_lowercase();
        for &dir_name in REGIONTYPE_MERGE_DIRS {
            let dir_lower = dir_name.to_ascii_lowercase();
            let segment = format!("/{dir_lower}/");
            if let Some(pos) = lower.find(&segment) {
                // base_dir is everything up to and including the merge dir
                let base_end = pos + segment.len();
                let base_dir = s[..base_end].to_string();

                let file_name = pak.file_name();

                // Check if this is: Name.REGIONTYPE.lsx
                if let Some(region_type) = extract_region_type(file_name, dir_name) {
                    let key = (base_dir.clone(), region_type.clone());
                    groups.entry(key).or_default().push(i);
                } else if let Some(stem) = file_name.strip_suffix(".lsx") {
                    // Could be the base file: e.g., Races.lsx in Races/
                    // It's a base file if it's directly in the merge dir root
                    // (no deeper subdirectory) and its stem matches a known
                    // region type name.
                    let rel_after_base = &s[base_end..];
                    if !rel_after_base.contains('/') {
                        // Direct child of the merge dir
                        let key = (base_dir.clone(), stem.to_string());
                        base_files.entry(key).or_insert(i);
                    }
                }
            }
        }
    }

    // Now process each group
    for ((base_dir, region_type), indices) in &groups {
        let key = (base_dir.clone(), region_type.clone());

        // Start with the base file if it exists
        let mut merged_resource = LsxResource {
            regions: Vec::new(),
        };
        let mut skip_base = false;

        if let Some(&base_idx) = base_files.get(&key) {
            let (disk_path, pak_path) = &file_entries[base_idx];
            let resource = parse_lsx_file_resource(disk_path, pak_path)?;
            merge_resource_into(&mut merged_resource, &resource);
            plan.skip_indices.insert(base_idx);
            skip_base = true;
        }

        // Merge all Name.REGIONTYPE.lsx files
        for &idx in indices {
            let (disk_path, pak_path) = &file_entries[idx];
            let resource = parse_lsx_file_resource(disk_path, pak_path)?;
            merge_resource_into(&mut merged_resource, &resource);
            plan.skip_indices.insert(idx);
        }

        if merged_resource.regions.is_empty() {
            continue;
        }

        // Output path: base_dir + REGIONTYPE.lsx
        let merged_pak_str = format!("{base_dir}{region_type}.lsx");
        let merged_pak = PakPath::parse(&merged_pak_str)
            .map_err(|e| format!("Invalid merged regiontype path: {e}"))?;

        let xml_bytes = write_lsx_resource_xml(&merged_resource);
        plan.merged_entries.push((merged_pak, xml_bytes));

        let _ = skip_base;
    }

    // Process orphan base files — standalone REGIONTYPE.lsx files with no
    // Name.REGIONTYPE.lsx companions. Parsing and re-serialising strips XML
    // comments and normalises formatting.
    for ((base_dir, region_type), &base_idx) in &base_files {
        let key = (base_dir.clone(), region_type.clone());
        if groups.contains_key(&key) {
            continue; // Already processed as part of a group
        }
        if plan.skip_indices.contains(&base_idx) {
            continue;
        }

        let (disk_path, pak_path) = &file_entries[base_idx];
        let resource = parse_lsx_file_resource(disk_path, pak_path)?;

        let merged_pak_str = format!("{base_dir}{region_type}.lsx");
        let merged_pak =
            PakPath::parse(&merged_pak_str).map_err(|e| format!("Invalid regiontype path: {e}"))?;

        let xml_bytes = write_lsx_resource_xml(&resource);
        plan.skip_indices.insert(base_idx);
        plan.merged_entries.push((merged_pak, xml_bytes));
    }

    Ok(())
}

/// Extract the REGIONTYPE from a filename matching `Name.REGIONTYPE.lsx`.
///
/// Returns `Some(region_type)` if the filename matches the `*.REGIONTYPE.lsx` pattern
/// where REGIONTYPE is a known region type name for the given directory.
/// Lists directory has multiple region types (SpellLists, PassiveLists, etc.).
fn extract_region_type(file_name: &str, dir_name: &str) -> Option<String> {
    // Must end with .lsx
    let stem = file_name.strip_suffix(".lsx")?;

    // Must contain at least one dot (Name.REGIONTYPE)
    let dot_pos = stem.rfind('.')?;
    let region_part = &stem[dot_pos + 1..];

    // For Lists, the region type is the suffix part (SpellLists, PassiveLists, etc.)
    // For others, it should match the dir name or a known variant
    if dir_name == "Lists" {
        // Accept any *Lists suffix
        if region_part.ends_with("Lists") {
            return Some(region_part.to_string());
        }
    } else if dir_name == "Progressions" {
        // Accept Progressions and ProgressionDescriptions
        if region_part == "Progressions" || region_part == "ProgressionDescriptions" {
            return Some(region_part.to_string());
        }
    } else {
        // For ActionResourceDefinitions, Races, ClassDescriptions:
        // the region type should match the directory name
        if region_part.eq_ignore_ascii_case(dir_name) {
            return Some(region_part.to_string());
        }
    }

    None
}

// ─── Stats condensation ─────────────────────────────────────────────────────

/// Merge `.txt` stat files under `Stats/Generated/Data/` into category-based
/// files, using `// === original_filename.txt ===` comment headers.
fn merge_stats_files(
    file_entries: &[(PathBuf, PakPath)],
    plan: &mut MergePlan,
) -> Result<(), String> {
    // Group by (stats_data_base, top_level_category_dir)
    // e.g. "Public/MyMod/Stats/Generated/Data/" + "CL_Class_Specific" → indices
    let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    // Track files directly in Data/ (no subdirectory) — these are not condensed
    let mut direct_data_files: HashSet<usize> = HashSet::new();

    for (i, (_disk, pak)) in file_entries.iter().enumerate() {
        if plan.skip_indices.contains(&i) {
            continue;
        }

        let s = pak.as_str();
        if !s.ends_with(".txt") {
            continue;
        }

        let lower = s.to_ascii_lowercase();
        // Match: .../stats/generated/data/CATEGORY/...file.txt
        if let Some(data_pos) = lower.find("/stats/generated/data/") {
            let data_end = data_pos + "/Stats/Generated/Data/".len();
            let base_dir = s[..data_end].to_string();
            let after_data = &s[data_end..];

            if let Some(slash_pos) = after_data.find('/') {
                // File is in a subdirectory — group by category
                let category = after_data[..slash_pos].to_string();
                groups.entry((base_dir, category)).or_default().push(i);
            } else {
                // File directly in Data/ — not condensed
                direct_data_files.insert(i);
            }
        }
    }

    for ((base_dir, category), indices) in &groups {
        if indices.len() <= 1 {
            continue; // nothing to merge
        }

        let mut merged_content = String::new();
        merged_content.push_str("// ==== Generated with CMTY Studio ====\n");

        // Sort by pak path for deterministic output
        let mut sorted: Vec<usize> = indices.clone();
        sorted.sort_by(|a, b| file_entries[*a].1.as_str().cmp(file_entries[*b].1.as_str()));

        for &idx in &sorted {
            let (disk_path, pak_path) = &file_entries[idx];
            let file_name = pak_path.file_name();

            let content = std::fs::read_to_string(disk_path)
                .map_err(|e| format!("Failed to read stats file {}: {e}", disk_path.display()))?;
            let content = content.trim();

            if !merged_content.is_empty() {
                merged_content.push('\n');
            }
            merged_content.push_str(&format!("// === {file_name} ===\n"));
            merged_content.push_str(content);
            merged_content.push('\n');

            plan.skip_indices.insert(idx);
        }

        // Output to base_dir / category.txt
        let merged_pak_str = format!("{base_dir}{category}.txt");
        let merged_pak = PakPath::parse(&merged_pak_str)
            .map_err(|e| format!("Invalid merged stats path: {e}"))?;

        plan.merged_entries
            .push((merged_pak, merged_content.into_bytes()));
    }

    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Parse a resource file from disk. Supports `.lsx` (XML) and `.lsf` (binary).
fn parse_resource_file(disk_path: &Path, pak_path: &PakPath) -> Result<LsxResource, String> {
    let s = pak_path.as_str().to_ascii_lowercase();
    if s.ends_with(".lsx") {
        parse_lsx_file_resource(disk_path, pak_path)
    } else if s.ends_with(".lsf") || s.ends_with(".lsf.lsf") {
        crate::parsers::lsf::parse_lsf_file(disk_path)
    } else {
        Err(format!(
            "Unsupported file type for merge: {}",
            pak_path.as_str()
        ))
    }
}

/// Parse an LSX (XML) file into an LsxResource.
fn parse_lsx_file_resource(disk_path: &Path, pak_path: &PakPath) -> Result<LsxResource, String> {
    let content = std::fs::read_to_string(disk_path)
        .map_err(|e| format!("Failed to read {}: {e}", disk_path.display()))?;
    crate::parsers::lsx::parse_lsx_resource(&content)
        .map_err(|e| format!("Failed to parse LSX {}: {e}", pak_path.as_str()))
}

/// Merge all regions from `source` into `target`. Regions with the same id
/// have their nodes combined; new region ids are appended.
fn merge_resource_into(target: &mut LsxResource, source: &LsxResource) {
    for src_region in &source.regions {
        if let Some(tgt_region) = target.regions.iter_mut().find(|r| r.id == src_region.id) {
            // Same region id — merge children of the wrapper node.
            // LSX structure: region → wrapper node (e.g. "root", "Templates") → children.
            // When both sides have a single wrapper node with the same id,
            // combine their children instead of duplicating the wrapper.
            if tgt_region.nodes.len() == 1
                && src_region.nodes.len() == 1
                && tgt_region.nodes[0].id == src_region.nodes[0].id
            {
                tgt_region.nodes[0]
                    .children
                    .extend(src_region.nodes[0].children.clone());
            } else {
                // Fallback: just append nodes
                tgt_region.nodes.extend(src_region.nodes.clone());
            }
        } else {
            // New region — add it
            target.regions.push(src_region.clone());
        }
    }
}

/// Serialize an `LsxResource` to LSX XML bytes.
fn write_lsx_resource_xml(resource: &LsxResource) -> Vec<u8> {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<save>\n");
    // Use a reasonable default version
    out.push_str("  <version major=\"4\" minor=\"0\" revision=\"9\" build=\"328\" />\n");

    for region in &resource.regions {
        out.push_str(&format!("  <region id=\"{}\">\n", xml_escape(&region.id)));
        for node in &region.nodes {
            write_xml_node(&mut out, node, 2);
        }
        out.push_str("  </region>\n");
    }

    out.push_str("</save>\n");
    out.into_bytes()
}

fn write_xml_node(out: &mut String, node: &LsxNode, indent: usize) {
    let pad = "  ".repeat(indent);

    let has_children = !node.children.is_empty();
    let has_attrs = !node.attributes.is_empty();

    if !has_children && !has_attrs {
        out.push_str(&format!("{pad}<node id=\"{}\" />\n", xml_escape(&node.id)));
        return;
    }

    out.push_str(&format!("{pad}<node id=\"{}\">\n", xml_escape(&node.id)));

    // Attributes
    for attr in &node.attributes {
        write_xml_attribute(out, attr, indent + 1);
    }

    // Children
    if has_children {
        out.push_str(&format!("{pad}  <children>\n"));
        for child in &node.children {
            write_xml_node(out, child, indent + 2);
        }
        out.push_str(&format!("{pad}  </children>\n"));
    }

    out.push_str(&format!("{pad}</node>\n"));
}

fn write_xml_attribute(out: &mut String, attr: &LsxNodeAttribute, indent: usize) {
    let pad = "  ".repeat(indent);

    if attr.handle.is_some() || attr.version.is_some() || !attr.arguments.is_empty() {
        // TranslatedString or TranslatedFSString — self-closing with handle/version
        let mut parts = format!(
            "{pad}<attribute id=\"{}\" type=\"{}\" ",
            xml_escape(&attr.id),
            xml_escape(&attr.attr_type),
        );
        if let Some(ref handle) = attr.handle {
            parts.push_str(&format!("handle=\"{}\" ", xml_escape(handle)));
        }
        if let Some(ver) = attr.version {
            parts.push_str(&format!("version=\"{ver}\" "));
        }
        if attr.arguments.is_empty() {
            parts.push_str("/>\n");
            out.push_str(&parts);
        } else {
            parts.push_str(">\n");
            out.push_str(&parts);
            for arg in &attr.arguments {
                out.push_str(&format!(
                    "{pad}  <argument key=\"{}\" value=\"{}\" />\n",
                    xml_escape(&arg.key),
                    xml_escape(&arg.value),
                ));
            }
            out.push_str(&format!("{pad}</attribute>\n"));
        }
    } else {
        // Simple attribute
        out.push_str(&format!(
            "{pad}<attribute id=\"{}\" type=\"{}\" value=\"{}\" />\n",
            xml_escape(&attr.id),
            xml_escape(&attr.attr_type),
            xml_escape(&attr.value),
        ));
    }
}

/// Minimal XML escaping for attribute values and text.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::LsxRegion;

    #[test]
    fn extract_region_type_action_resource() {
        assert_eq!(
            extract_region_type(
                "MailMe_DeathKnight.ActionResourceDefinitions.lsx",
                "ActionResourceDefinitions"
            ),
            Some("ActionResourceDefinitions".to_string()),
        );
    }

    #[test]
    fn extract_region_type_races() {
        assert_eq!(
            extract_region_type("Shadar-Kai.Races.lsx", "Races"),
            Some("Races".to_string()),
        );
    }

    #[test]
    fn extract_region_type_lists_spell() {
        assert_eq!(
            extract_region_type("Skeletons.SpellLists.lsx", "Lists"),
            Some("SpellLists".to_string()),
        );
    }

    #[test]
    fn extract_region_type_lists_passive() {
        assert_eq!(
            extract_region_type("CL_ClassTags.PassiveLists.lsx", "Lists"),
            Some("PassiveLists".to_string()),
        );
    }

    #[test]
    fn extract_region_type_progression_descriptions() {
        assert_eq!(
            extract_region_type("CL_Tags.ProgressionDescriptions.lsx", "Progressions"),
            Some("ProgressionDescriptions".to_string()),
        );
    }

    #[test]
    fn extract_region_type_progressions() {
        assert_eq!(
            extract_region_type("Shadar-Kai.Progressions.lsx", "Progressions"),
            Some("Progressions".to_string()),
        );
    }

    #[test]
    fn extract_region_type_plain_filename_returns_none() {
        // A plain filename without the Name.TYPE.lsx pattern
        assert_eq!(extract_region_type("Races.lsx", "Races"), None,);
    }

    #[test]
    fn extract_region_type_wrong_dir() {
        // ActionResourceDefinitions pattern should not match in Races dir
        assert_eq!(
            extract_region_type("Foo.ActionResourceDefinitions.lsx", "Races"),
            None,
        );
    }

    #[test]
    fn merge_resource_combines_same_region() {
        let mut target = LsxResource {
            regions: vec![LsxRegion {
                id: "SpellLists".into(),
                nodes: vec![LsxNode {
                    id: "root".into(),
                    key_attribute: None,
                    attributes: vec![],
                    children: vec![LsxNode {
                        id: "SpellList".into(),
                        key_attribute: None,
                        attributes: vec![LsxNodeAttribute {
                            id: "UUID".into(),
                            attr_type: "guid".into(),
                            value: "aaa".into(),
                            handle: None,
                            version: None,
                            arguments: vec![],
                        }],
                        children: vec![],
                        commented: false,
                    }],
                    commented: false,
                }],
            }],
        };

        let source = LsxResource {
            regions: vec![LsxRegion {
                id: "SpellLists".into(),
                nodes: vec![LsxNode {
                    id: "root".into(),
                    key_attribute: None,
                    attributes: vec![],
                    children: vec![LsxNode {
                        id: "SpellList".into(),
                        key_attribute: None,
                        attributes: vec![LsxNodeAttribute {
                            id: "UUID".into(),
                            attr_type: "guid".into(),
                            value: "bbb".into(),
                            handle: None,
                            version: None,
                            arguments: vec![],
                        }],
                        children: vec![],
                        commented: false,
                    }],
                    commented: false,
                }],
            }],
        };

        merge_resource_into(&mut target, &source);

        assert_eq!(target.regions.len(), 1);
        assert_eq!(target.regions[0].nodes[0].children.len(), 2);
        assert_eq!(
            target.regions[0].nodes[0].children[0].attributes[0].value,
            "aaa"
        );
        assert_eq!(
            target.regions[0].nodes[0].children[1].attributes[0].value,
            "bbb"
        );
    }

    #[test]
    fn merge_resource_adds_new_region() {
        let mut target = LsxResource {
            regions: vec![LsxRegion {
                id: "Progressions".into(),
                nodes: vec![],
            }],
        };

        let source = LsxResource {
            regions: vec![LsxRegion {
                id: "ProgressionDescriptions".into(),
                nodes: vec![],
            }],
        };

        merge_resource_into(&mut target, &source);

        assert_eq!(target.regions.len(), 2);
        assert_eq!(target.regions[0].id, "Progressions");
        assert_eq!(target.regions[1].id, "ProgressionDescriptions");
    }

    #[test]
    fn write_xml_roundtrip() {
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Races".into(),
                nodes: vec![LsxNode {
                    id: "root".into(),
                    key_attribute: None,
                    attributes: vec![],
                    children: vec![LsxNode {
                        id: "Race".into(),
                        key_attribute: None,
                        attributes: vec![LsxNodeAttribute {
                            id: "Name".into(),
                            attr_type: "FixedString".into(),
                            value: "ShadarKai".into(),
                            handle: None,
                            version: None,
                            arguments: vec![],
                        }],
                        children: vec![],
                        commented: false,
                    }],
                    commented: false,
                }],
            }],
        };

        let xml_bytes = write_lsx_resource_xml(&resource);
        let xml_str = String::from_utf8(xml_bytes).unwrap();

        // Parse it back
        let parsed = crate::parsers::lsx::parse_lsx_resource(&xml_str).unwrap();
        assert_eq!(parsed.regions.len(), 1);
        assert_eq!(parsed.regions[0].id, "Races");
        assert_eq!(parsed.regions[0].nodes[0].children.len(), 1);
        assert_eq!(
            parsed.regions[0].nodes[0].children[0].attributes[0].value,
            "ShadarKai"
        );
    }

    #[test]
    fn double_extension_fix() {
        let entries = vec![
            (
                PathBuf::from("test.lsf.lsf"),
                PakPath::parse("Public/Mod/RootTemplates/foo.lsf.lsf").unwrap(),
            ),
            (
                PathBuf::from("test.lsx"),
                PakPath::parse("Public/Mod/RootTemplates/bar.lsx").unwrap(),
            ),
        ];

        let mut plan = MergePlan {
            skip_indices: HashSet::new(),
            merged_entries: Vec::new(),
            path_rewrites: BTreeMap::new(),
        };

        fix_double_extensions(&entries, &mut plan);

        assert_eq!(plan.path_rewrites.len(), 1);
        assert_eq!(
            plan.path_rewrites[&0].as_str(),
            "Public/Mod/RootTemplates/foo.lsf"
        );
    }

    #[test]
    fn extract_region_type_class_descriptions() {
        assert_eq!(
            extract_region_type(
                "MailMe_DeathKnight.ClassDescriptions.lsx",
                "ClassDescriptions"
            ),
            Some("ClassDescriptions".to_string()),
        );
    }

    #[test]
    fn stats_comment_header_format() {
        // Verify the comment header format matches MMT convention
        let header = format!("// === {} ===\n", "CL_Passive_OneDnD_Scholar.txt");
        assert!(header.starts_with("// === "));
        assert!(header.ends_with(" ===\n"));
    }
}
