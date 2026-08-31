use crate::models::{LsxAttribute, LsxChildEntry, LsxChildGroup, LsxEntry};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A full-fidelity representation of an LSX file in a YAML-serializable format.
/// Preserves all attributes and nested children, unlike `LsxEntry` which flattens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LsxYamlDoc {
    pub region_id: String,
    pub nodes: Vec<LsxYamlNode>,
}

/// A single node in the YAML tree (recursive).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LsxYamlNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<LsxYamlAttr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<LsxYamlNode>,
}

/// An attribute with its type and value preserved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LsxYamlAttr {
    pub id: String,
    #[serde(rename = "type")]
    pub attr_type: String,
    pub value: String,
}

// ── XML → YAML ──────────────────────────────────────────

/// Parse an LSX XML string into an LsxYamlDoc, preserving full tree structure.
pub fn lsx_to_yaml_doc(xml_content: &str) -> Result<LsxYamlDoc, String> {
    let mut reader = Reader::from_str(xml_content);
    let mut buf = Vec::new();
    let mut region_id = String::new();

    // Stack of (node_id, attributes, children) for tree building
    let mut stack: Vec<NodeBuilder> = Vec::new();
    let mut root_children: Vec<LsxYamlNode> = Vec::new();
    let mut depth: u32 = 0;
    let mut in_root_children = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match tag.as_str() {
                    "region" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"id" {
                                region_id = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                    }
                    "node" => {
                        let mut node_id = String::new();
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"id" {
                                node_id = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                        if node_id != "root" {
                            stack.push(NodeBuilder {
                                id: node_id,
                                attributes: Vec::new(),
                                children: Vec::new(),
                            });
                        }
                        depth += 1;
                    }
                    "children" => {
                        if depth == 2 {
                            in_root_children = true;
                        }
                    }
                    "attribute" => {
                        let (attr_id, attr_type, attr_value) =
                            extract_lsx_attr(e.attributes().flatten());
                        if !attr_id.is_empty() {
                            if let Some(current) = stack.last_mut() {
                                current.attributes.push(LsxYamlAttr {
                                    id: attr_id,
                                    attr_type,
                                    value: attr_value,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "attribute" {
                    let (attr_id, attr_type, attr_value) =
                        extract_lsx_attr(e.attributes().flatten());
                    if !attr_id.is_empty() {
                        if let Some(current) = stack.last_mut() {
                            current.attributes.push(LsxYamlAttr {
                                id: attr_id,
                                attr_type,
                                value: attr_value,
                            });
                        }
                    }
                } else if tag == "node" {
                    // Self-closing <node id="Scripts" /> — empty node
                    let mut node_id = String::new();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"id" {
                            node_id = String::from_utf8_lossy(&attr.value).to_string();
                        }
                    }
                    if node_id != "root" {
                        let empty_node = LsxYamlNode {
                            id: node_id,
                            attributes: Vec::new(),
                            children: Vec::new(),
                        };
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(empty_node);
                        } else {
                            root_children.push(empty_node);
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match tag.as_str() {
                    "node" => {
                        depth = depth.saturating_sub(1);
                        if let Some(builder) = stack.pop() {
                            let node = LsxYamlNode {
                                id: builder.id,
                                attributes: builder.attributes,
                                children: builder.children,
                            };
                            if let Some(parent) = stack.last_mut() {
                                parent.children.push(node);
                            } else {
                                root_children.push(node);
                            }
                        }
                    }
                    "children" => {
                        if in_root_children && depth <= 2 {
                            in_root_children = false;
                        }
                    }
                    _ => {}
                }
            }
            Err(e) => {
                return Err(format!(
                    "XML parse error at position {}: {}",
                    reader.error_position(),
                    e
                ))
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(LsxYamlDoc {
        region_id,
        nodes: root_children,
    })
}

/// Serialize an LsxYamlDoc to a YAML string.
pub fn yaml_doc_to_string(doc: &LsxYamlDoc) -> Result<String, String> {
    serde_saphyr::to_string(doc).map_err(|e| format!("YAML serialization error: {e}"))
}

/// Parse an LSX XML file content → YAML string.
pub fn lsx_to_yaml(xml_content: &str) -> Result<String, String> {
    let doc = lsx_to_yaml_doc(xml_content)?;
    yaml_doc_to_string(&doc)
}

// ── YAML → LsxEntry (for scan pipeline) ────────────────

/// Deserialize a YAML string back into an LsxYamlDoc.
pub fn yaml_to_doc(yaml_content: &str) -> Result<LsxYamlDoc, String> {
    serde_saphyr::from_str(yaml_content).map_err(|e| format!("YAML deserialization error: {e}"))
}

/// Convert a YAML doc back into `Vec<LsxEntry>` for the scan/diff pipeline.
/// Applies the same flattening rules as `parse_lsx_file`:
/// - Top-level nodes become LsxEntry objects
/// - Nested children become LsxChildGroup/LsxChildEntry
pub fn yaml_to_lsx_entries(yaml_content: &str) -> Result<Vec<LsxEntry>, String> {
    let doc = yaml_to_doc(yaml_content)?;
    let mut entries = Vec::new();

    for node in &doc.nodes {
        let entry = yaml_node_to_lsx_entry(node);
        entries.push(entry);
    }

    Ok(entries)
}

/// Convert a single top-level YAML node into an LsxEntry.
fn yaml_node_to_lsx_entry(node: &LsxYamlNode) -> LsxEntry {
    let mut attributes = HashMap::new();
    for attr in &node.attributes {
        attributes.insert(
            attr.id.clone(),
            LsxAttribute {
                attr_type: attr.attr_type.clone(),
                value: attr.value.clone(),
            },
        );
    }

    let uuid = attributes
        .get("UUID")
        .or_else(|| attributes.get("MapKey"))
        .map(|a| a.value.clone())
        .unwrap_or_default();

    // Flatten children using same rules as parse_lsx_file
    let children = flatten_children(&node.children);

    LsxEntry {
        uuid,
        node_id: node.id.clone(),
        attributes,
        children,
        commented: false,
        region_id: String::new(),
    }
}

/// Flatten nested YAML nodes into LsxChildGroup/LsxChildEntry,
/// matching the flattening behavior of parse_lsx_file.
fn flatten_children(children: &[LsxYamlNode]) -> Vec<LsxChildGroup> {
    let mut groups: Vec<LsxChildGroup> = Vec::new();

    for child in children {
        if !child.children.is_empty() {
            // Container node — promote its children with this node's id as group_id
            for grandchild in &child.children {
                let object_guid = grandchild
                    .attributes
                    .iter()
                    .find(|a| a.id == "Object")
                    .map(|a| a.value.clone())
                    .unwrap_or_default();

                let group_id = child.id.clone();
                if let Some(group) = groups.iter_mut().find(|g| g.group_id == group_id) {
                    group.entries.push(LsxChildEntry {
                        node_id: grandchild.id.clone(),
                        object_guid,
                    });
                } else {
                    groups.push(LsxChildGroup {
                        group_id,
                        entries: vec![LsxChildEntry {
                            node_id: grandchild.id.clone(),
                            object_guid,
                        }],
                    });
                }
            }
        } else {
            // Leaf child node
            let object_guid = child
                .attributes
                .iter()
                .find(|a| a.id == "Object")
                .map(|a| a.value.clone())
                .unwrap_or_default();

            let group_id = child.id.clone();
            if let Some(group) = groups.iter_mut().find(|g| g.group_id == group_id) {
                group.entries.push(LsxChildEntry {
                    node_id: child.id.clone(),
                    object_guid,
                });
            } else {
                groups.push(LsxChildGroup {
                    group_id,
                    entries: vec![LsxChildEntry {
                        node_id: child.id.clone(),
                        object_guid,
                    }],
                });
            }
        }
    }

    groups
}

/// Get the raw XML content for meta.lsx from a YAML doc (for meta parsing).
/// Since meta parsing uses raw XML features (dependencies, PublishVersion),
/// we provide direct attribute access from the YAML doc.
pub fn yaml_doc_get_meta_attributes(doc: &LsxYamlDoc) -> Option<HashMap<String, String>> {
    doc.nodes.iter().find(|n| n.id == "ModuleInfo").map(|node| {
        node.attributes
            .iter()
            .map(|a| (a.id.clone(), a.value.clone()))
            .collect()
    })
}

// ── Internal helpers ────────────────────────────────────

struct NodeBuilder {
    id: String,
    attributes: Vec<LsxYamlAttr>,
    children: Vec<LsxYamlNode>,
}

/// Extract id, type, and value from an XML attribute element's attributes.
fn extract_lsx_attr<'a>(
    attrs: impl Iterator<Item = quick_xml::events::attributes::Attribute<'a>>,
) -> (String, String, String) {
    let mut attr_id = String::new();
    let mut attr_type = String::new();
    let mut attr_value = String::new();

    for attr in attrs {
        match attr.key.as_ref() {
            b"id" => attr_id = String::from_utf8_lossy(&attr.value).to_string(),
            b"type" => attr_type = String::from_utf8_lossy(&attr.value).to_string(),
            b"value" => attr_value = String::from_utf8_lossy(&attr.value).to_string(),
            b"handle" => attr_value = String::from_utf8_lossy(&attr.value).to_string(),
            _ => {}
        }
    }

    (attr_id, attr_type, attr_value)
}

// ── Consolidated per-type YAML format ───────────────────

/// A single entry in a consolidated per-type YAML file.
/// Wraps an `LsxYamlNode` with source attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidatedEntry {
    /// Source of this entry: "Vanilla" or the mod name.
    pub source: String,
    /// The full-fidelity node tree (id, attributes, children).
    #[serde(flatten)]
    pub node: LsxYamlNode,
}

/// A consolidated YAML file containing all entries of a given type.
/// One of these per Section (e.g. Progressions.yaml, Races.yaml).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidatedFile {
    /// The Section/folder this file represents (e.g. "Progressions").
    pub section: String,
    /// All entries from all sources, tagged with their origin.
    pub entries: Vec<ConsolidatedEntry>,
}

/// Read a consolidated YAML file from disk.
///
/// Uses a raised node budget (5 million) to handle large vanilla data files
/// like Races.yaml (~2.8 MB, 100k+ lines) and CharacterCreation.yaml (~2.4 MB).
pub fn read_consolidated_file(path: &std::path::Path) -> Result<ConsolidatedFile, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let options = serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_nodes: 5_000_000,
            max_events: 10_000_000,
        },
    };
    serde_saphyr::from_str_with_options(&content, options)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))
}

/// Write a consolidated YAML file to disk.
pub fn write_consolidated_file(
    path: &std::path::Path,
    file: &ConsolidatedFile,
) -> Result<(), String> {
    let yaml =
        serde_saphyr::to_string(file).map_err(|e| format!("YAML serialization error: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }
    std::fs::write(path, yaml).map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    Ok(())
}

/// Append entries to a consolidated YAML file. If the file exists, read it first
/// and merge; otherwise create a new one.
pub fn append_to_consolidated_file(
    path: &std::path::Path,
    section: &str,
    new_entries: Vec<ConsolidatedEntry>,
) -> Result<(), String> {
    let mut file = if path.exists() {
        read_consolidated_file(path)?
    } else {
        ConsolidatedFile {
            section: section.to_string(),
            entries: Vec::new(),
        }
    };
    file.entries.extend(new_entries);
    write_consolidated_file(path, &file)
}

/// Convert all entries from an LsxYamlDoc into ConsolidatedEntry objects with a given source.
pub fn doc_to_consolidated_entries(doc: &LsxYamlDoc, source: &str) -> Vec<ConsolidatedEntry> {
    doc.nodes
        .iter()
        .map(|node| ConsolidatedEntry {
            source: source.to_string(),
            node: node.clone(),
        })
        .collect()
}

/// Convert consolidated entries back to LsxEntry objects for the scan/diff pipeline.
pub fn consolidated_to_lsx_entries(file: &ConsolidatedFile) -> Vec<LsxEntry> {
    file.entries
        .iter()
        .map(|ce| yaml_node_to_lsx_entry(&ce.node))
        .collect()
}

/// Parse an LSX XML string directly into ConsolidatedEntry objects.
/// This combines parsing + source tagging in one step for the streaming pipeline.
pub fn lsx_to_consolidated_entries(
    xml_content: &str,
    source: &str,
) -> Result<Vec<ConsolidatedEntry>, String> {
    let doc = lsx_to_yaml_doc(xml_content)?;
    Ok(doc_to_consolidated_entries(&doc, source))
}

// ── UUID Index ──────────────────────────────────────────

/// A single entry in uuid_idx.yaml, mapping a UUID to its section, source, and display name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UuidIndexEntry {
    pub section: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub node_id: String,
}

/// The uuid_idx.yaml file structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UuidIndex {
    pub entries: HashMap<String, UuidIndexEntry>,
}

/// Extract the UUID (or MapKey) and a display name from a node's attributes.
fn extract_uuid_and_name(node: &LsxYamlNode) -> Option<(String, Option<String>)> {
    let mut uuid: Option<String> = None;
    let mut name: Option<String> = None;
    for attr in &node.attributes {
        let id_lower = attr.id.to_lowercase();
        if id_lower == "uuid" || id_lower == "mapkey" {
            let v = attr.value.trim().to_string();
            if !v.is_empty() {
                uuid = Some(v);
            }
        } else if id_lower == "name" || id_lower == "displayname" || id_lower == "entryname" {
            let v = attr.value.trim().to_string();
            if !v.is_empty() && name.is_none() {
                name = Some(v);
            }
        }
    }
    uuid.map(|u| (u, name))
}

/// Build a UUID index from all consolidated YAML files in a directory.
/// Returns the index mapping UUID → {section, source, name, node_id}.
pub fn build_uuid_index(dir: &std::path::Path) -> Result<UuidIndex, String> {
    use crate::models::Section;

    let mut entries = HashMap::new();

    // Walk the directory for consolidated .yaml files
    for section in Section::all_ordered() {
        let yaml_path = dir.join(section.consolidated_filename());
        if !yaml_path.exists() {
            continue;
        }
        match read_consolidated_file(&yaml_path) {
            Ok(file) => {
                for ce in &file.entries {
                    if let Some((uuid, name)) = extract_uuid_and_name(&ce.node) {
                        entries.insert(
                            uuid,
                            UuidIndexEntry {
                                section: file.section.clone(),
                                source: ce.source.clone(),
                                name,
                                node_id: ce.node.id.clone(),
                            },
                        );
                    }
                }
            }
            Err(_) => continue,
        }
    }

    Ok(UuidIndex { entries })
}

/// Write a UUID index to a uuid_idx.yaml file.
pub fn write_uuid_index(dir: &std::path::Path) -> Result<usize, String> {
    let index = build_uuid_index(dir)?;
    let count = index.entries.len();
    let yaml = serde_saphyr::to_string(&index)
        .map_err(|e| format!("UUID index serialization error: {e}"))?;
    let path = dir.join("uuid_idx.yaml");
    std::fs::write(&path, yaml).map_err(|e| format!("Failed to write uuid_idx.yaml: {e}"))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LSX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Progressions">
        <node id="root">
            <children>
                <node id="Progression">
                    <attribute id="UUID" type="guid" value="12345678-1234-1234-1234-123456789abc"/>
                    <attribute id="Name" type="LSString" value="TestProgression"/>
                    <attribute id="Level" type="uint8" value="1"/>
                    <children>
                        <node id="SubClasses">
                            <children>
                                <node id="SubClass">
                                    <attribute id="Object" type="guid" value="aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"/>
                                </node>
                            </children>
                        </node>
                    </children>
                </node>
            </children>
        </node>
    </region>
</save>"#;

    #[test]
    fn test_lsx_to_yaml_roundtrip() {
        // Parse XML to YAML doc
        let doc = lsx_to_yaml_doc(SAMPLE_LSX).unwrap();
        assert_eq!(doc.region_id, "Progressions");
        assert_eq!(doc.nodes.len(), 1);
        assert_eq!(doc.nodes[0].id, "Progression");
        assert_eq!(doc.nodes[0].attributes.len(), 3);
        assert_eq!(doc.nodes[0].children.len(), 1); // SubClasses
        assert_eq!(doc.nodes[0].children[0].id, "SubClasses");
        assert_eq!(doc.nodes[0].children[0].children.len(), 1); // SubClass

        // Convert to YAML string and back
        let yaml_str = yaml_doc_to_string(&doc).unwrap();
        let doc2 = yaml_to_doc(&yaml_str).unwrap();
        assert_eq!(doc2.region_id, "Progressions");
        assert_eq!(doc2.nodes.len(), 1);
    }

    #[test]
    fn test_yaml_to_lsx_entries() {
        let doc = lsx_to_yaml_doc(SAMPLE_LSX).unwrap();
        let yaml_str = yaml_doc_to_string(&doc).unwrap();
        let entries = yaml_to_lsx_entries(&yaml_str).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uuid, "12345678-1234-1234-1234-123456789abc");
        assert_eq!(entries[0].node_id, "Progression");
        assert_eq!(entries[0].attributes.len(), 3);
        // SubClasses → SubClass child with object_guid
        assert_eq!(entries[0].children.len(), 1);
        assert_eq!(entries[0].children[0].group_id, "SubClasses");
        assert_eq!(
            entries[0].children[0].entries[0].object_guid,
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        );
    }

    #[test]
    fn test_consolidated_roundtrip() {
        let doc = lsx_to_yaml_doc(SAMPLE_LSX).unwrap();
        let entries = doc_to_consolidated_entries(&doc, "Vanilla");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "Vanilla");
        assert_eq!(entries[0].node.id, "Progression");

        let file = ConsolidatedFile {
            section: "Progressions".to_string(),
            entries,
        };
        let yaml = serde_saphyr::to_string(&file).unwrap();
        let file2: ConsolidatedFile = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(file2.section, "Progressions");
        assert_eq!(file2.entries.len(), 1);
        assert_eq!(file2.entries[0].source, "Vanilla");

        // Convert back to LsxEntry
        let lsx_entries = consolidated_to_lsx_entries(&file2);
        assert_eq!(lsx_entries.len(), 1);
        assert_eq!(lsx_entries[0].uuid, "12345678-1234-1234-1234-123456789abc");
    }

    #[test]
    fn test_lsx_to_consolidated_entries() {
        let entries = lsx_to_consolidated_entries(SAMPLE_LSX, "TestMod").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "TestMod");
        assert_eq!(entries[0].node.id, "Progression");
    }
}
