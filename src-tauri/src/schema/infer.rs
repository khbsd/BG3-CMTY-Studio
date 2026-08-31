use crate::models::{LsxEntry, Section};
use serde::Serialize;
use std::collections::HashMap;

/// Schema for a single attribute on an LSX node.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct AttrSchema {
    /// The attribute name (e.g. "UUID", "Name", "Description").
    pub name: String,
    /// The LSX type (e.g. "guid", "FixedString", "TranslatedString", "int32", "bool").
    pub attr_type: String,
    /// Fraction of entries that have this attribute (0.0–1.0).
    pub frequency: f64,
    /// A few example values from vanilla data (up to 5).
    pub examples: Vec<String>,
}

/// Schema for a child group on an LSX node.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct ChildSchema {
    /// The child group_id (e.g. "SubClasses", "EyeColors").
    pub group_id: String,
    /// The node_id of child entries (e.g. "SubClass", "EyeColor").
    pub child_node_id: String,
    /// Fraction of entries that have this child group (0.0–1.0).
    pub frequency: f64,
}

/// Complete schema for one LSX node type.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct NodeSchema {
    /// The node_id this schema describes (e.g. "Progression", "Race", "God").
    pub node_id: String,
    /// Which Section this node_id belongs to.
    pub section: String,
    /// Total number of vanilla entries sampled.
    pub sample_count: usize,
    /// Attribute definitions, sorted by frequency (most common first).
    pub attributes: Vec<AttrSchema>,
    /// Child group definitions, sorted by frequency.
    pub children: Vec<ChildSchema>,
}

/// Infer schemas from vanilla LSX section data.
///
/// For each section, examines all entries and builds a NodeSchema per unique
/// `node_id`. Attributes are aggregated across all entries to determine types,
/// frequency, and example values.
pub fn infer_schemas(
    lsx_sections: &HashMap<Section, HashMap<String, LsxEntry>>,
) -> Vec<NodeSchema> {
    let mut schemas = Vec::new();

    for section in Section::all_ordered() {
        let section_entries = match lsx_sections.get(section) {
            Some(entries) if !entries.is_empty() => entries,
            _ => continue,
        };

        // Group entries by node_id
        let mut by_node_id: HashMap<&str, Vec<&crate::models::LsxEntry>> = HashMap::new();
        for entry in section_entries.values() {
            by_node_id
                .entry(entry.node_id.as_str())
                .or_default()
                .push(entry);
        }

        for (node_id, entries) in &by_node_id {
            let total = entries.len();
            if total == 0 {
                continue;
            }

            // Collect attribute statistics
            let mut attr_counts: HashMap<&str, usize> = HashMap::new();
            let mut attr_types: HashMap<&str, &str> = HashMap::new();
            let mut attr_examples: HashMap<&str, Vec<String>> = HashMap::new();

            for entry in entries {
                for (attr_name, attr) in &entry.attributes {
                    *attr_counts.entry(attr_name.as_str()).or_insert(0) += 1;
                    attr_types
                        .entry(attr_name.as_str())
                        .or_insert(attr.attr_type.as_str());
                    let examples = attr_examples.entry(attr_name.as_str()).or_default();
                    if examples.len() < 5
                        && !attr.value.is_empty()
                        && !examples.contains(&attr.value)
                    {
                        examples.push(attr.value.clone());
                    }
                }
            }

            let mut attributes: Vec<AttrSchema> = attr_counts
                .iter()
                .map(|(name, count)| AttrSchema {
                    name: name.to_string(),
                    attr_type: attr_types.get(name).unwrap_or(&"FixedString").to_string(),
                    frequency: *count as f64 / total as f64,
                    examples: attr_examples.get(name).cloned().unwrap_or_default(),
                })
                .collect();
            // Sort: UUID first, then by frequency descending, then alphabetical
            attributes.sort_by(|a, b| {
                if a.name == "UUID" {
                    return std::cmp::Ordering::Less;
                }
                if b.name == "UUID" {
                    return std::cmp::Ordering::Greater;
                }
                b.frequency
                    .partial_cmp(&a.frequency)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.name.cmp(&b.name))
            });

            // Collect child group statistics
            let mut child_counts: HashMap<&str, usize> = HashMap::new();
            let mut child_node_ids: HashMap<&str, &str> = HashMap::new();

            for entry in entries {
                for group in &entry.children {
                    *child_counts.entry(group.group_id.as_str()).or_insert(0) += 1;
                    if let Some(first) = group.entries.first() {
                        child_node_ids
                            .entry(group.group_id.as_str())
                            .or_insert(first.node_id.as_str());
                    }
                }
            }

            let mut children: Vec<ChildSchema> = child_counts
                .iter()
                .map(|(group_id, count)| ChildSchema {
                    group_id: group_id.to_string(),
                    child_node_id: child_node_ids.get(group_id).unwrap_or(&"").to_string(),
                    frequency: *count as f64 / total as f64,
                })
                .collect();
            children.sort_by(|a, b| {
                b.frequency
                    .partial_cmp(&a.frequency)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.group_id.cmp(&b.group_id))
            });

            schemas.push(NodeSchema {
                node_id: node_id.to_string(),
                section: format!("{section:?}"),
                sample_count: total,
                attributes,
                children,
            });
        }
    }

    schemas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LsxAttribute, LsxChildEntry, LsxChildGroup};

    fn make_entry(uuid: &str, node_id: &str, attrs: Vec<(&str, &str, &str)>) -> LsxEntry {
        let mut attributes = HashMap::new();
        for (name, attr_type, value) in attrs {
            attributes.insert(
                name.to_string(),
                LsxAttribute {
                    attr_type: attr_type.to_string(),
                    value: value.to_string(),
                },
            );
        }
        LsxEntry {
            uuid: uuid.to_string(),
            node_id: node_id.to_string(),
            attributes,
            children: vec![],
            commented: false,
            region_id: String::new(),
        }
    }

    #[test]
    fn test_infer_schemas_basic() {
        let mut lsx_sections = HashMap::new();
        let mut gods = HashMap::new();
        gods.insert(
            "uuid-1".to_string(),
            make_entry(
                "uuid-1",
                "God",
                vec![
                    ("UUID", "guid", "uuid-1"),
                    ("Name", "FixedString", "Selune"),
                    ("Description", "TranslatedString", "h123"),
                ],
            ),
        );
        gods.insert(
            "uuid-2".to_string(),
            make_entry(
                "uuid-2",
                "God",
                vec![
                    ("UUID", "guid", "uuid-2"),
                    ("Name", "FixedString", "Mystra"),
                    ("Tag", "guid", "tag-uuid"),
                ],
            ),
        );
        lsx_sections.insert(Section::Gods, gods);

        let schemas = infer_schemas(&lsx_sections);
        assert_eq!(schemas.len(), 1);

        let god_schema = &schemas[0];
        assert_eq!(god_schema.node_id, "God");
        assert_eq!(god_schema.sample_count, 2);

        // UUID should be first
        assert_eq!(god_schema.attributes[0].name, "UUID");
        assert_eq!(god_schema.attributes[0].attr_type, "guid");
        assert!((god_schema.attributes[0].frequency - 1.0).abs() < 0.01);

        // Name should be next (frequency 1.0)
        let name_attr = god_schema
            .attributes
            .iter()
            .find(|a| a.name == "Name")
            .unwrap();
        assert!((name_attr.frequency - 1.0).abs() < 0.01);

        // Description and Tag should have frequency 0.5
        let desc = god_schema
            .attributes
            .iter()
            .find(|a| a.name == "Description")
            .unwrap();
        assert!((desc.frequency - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_infer_schemas_with_children() {
        let mut lsx_sections = HashMap::new();
        let mut races = HashMap::new();
        races.insert(
            "uuid-1".to_string(),
            LsxEntry {
                uuid: "uuid-1".to_string(),
                node_id: "Race".to_string(),
                region_id: String::new(),
                attributes: {
                    let mut m = HashMap::new();
                    m.insert(
                        "UUID".to_string(),
                        LsxAttribute {
                            attr_type: "guid".to_string(),
                            value: "uuid-1".to_string(),
                        },
                    );
                    m
                },
                commented: false,
                children: vec![LsxChildGroup {
                    group_id: "EyeColors".to_string(),
                    entries: vec![LsxChildEntry {
                        node_id: "EyeColor".to_string(),
                        object_guid: "color-1".to_string(),
                    }],
                }],
            },
        );
        lsx_sections.insert(Section::Races, races);

        let schemas = infer_schemas(&lsx_sections);
        let race = schemas.iter().find(|s| s.node_id == "Race").unwrap();
        assert_eq!(race.children.len(), 1);
        assert_eq!(race.children[0].group_id, "EyeColors");
        assert_eq!(race.children[0].child_node_id, "EyeColor");
        assert!((race.children[0].frequency - 1.0).abs() < 0.01);
    }
}
