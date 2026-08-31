use crate::models::{LsxAttribute, LsxChildGroup, LsxEntry};
use std::collections::HashMap;

/// Generate a complete LSX XML file from a list of entries.
///
/// `region_id` is the value for `<region id="...">` (e.g., "Progressions", "Races").
/// Entries are written as direct children of the `<node id="root"><children>` element.
pub fn entries_to_lsx(entries: &[LsxEntry], region_id: &str) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<save>\n");
    out.push_str("  <version major=\"4\" minor=\"0\" revision=\"0\" build=\"49\" />\n");
    out.push_str("  <region id=\"");
    out.push_str(&xml_escape(region_id));
    out.push_str("\">\n");
    out.push_str("    <node id=\"root\">\n");
    out.push_str("      <children>\n");

    for entry in entries {
        if entry.commented {
            write_commented_entry(&mut out, entry, 8);
        } else {
            write_entry(&mut out, entry, 8);
        }
    }

    out.push_str("      </children>\n");
    out.push_str("    </node>\n");
    out.push_str("  </region>\n");
    out.push_str("</save>\n");
    out
}

/// Write a single entry node at the given indentation level.
fn write_entry(out: &mut String, entry: &LsxEntry, indent: usize) {
    let pad = " ".repeat(indent);

    out.push_str(&pad);
    out.push_str("<node id=\"");
    out.push_str(&xml_escape(&entry.node_id));
    out.push_str("\">\n");

    // Write attributes in a stable order: UUID first, then alphabetical
    let mut attr_keys: Vec<&String> = entry.attributes.keys().collect();
    attr_keys.sort_by(|a, b| {
        let a_is_uuid = a.as_str() == "UUID" || a.as_str() == "MapKey";
        let b_is_uuid = b.as_str() == "UUID" || b.as_str() == "MapKey";
        match (a_is_uuid, b_is_uuid) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });

    for key in &attr_keys {
        let attr = &entry.attributes[*key];
        write_attribute(out, key, attr, indent + 2);
    }

    // Write child groups
    if !entry.children.is_empty() {
        write_children(out, &entry.children, indent + 2);
    }

    out.push_str(&pad);
    out.push_str("</node>\n");
}

/// Write a commented-out entry: serialize the entry XML, then wrap in `<!-- ... -->`.
fn write_commented_entry(out: &mut String, entry: &LsxEntry, indent: usize) {
    // Serialize the entry as normal XML into a buffer.
    // write_entry does not inspect `commented`, so no clone needed.
    let mut buf = String::new();
    write_entry(&mut buf, entry, indent + 2);

    let pad = " ".repeat(indent);
    out.push_str(&pad);
    out.push_str("<!--\n");
    out.push_str(&buf);
    out.push_str(&pad);
    out.push_str("-->\n");
}

/// Write a single `<attribute>` element.
fn write_attribute(out: &mut String, id: &str, attr: &LsxAttribute, indent: usize) {
    let pad = " ".repeat(indent);
    out.push_str(&pad);
    out.push_str("<attribute id=\"");
    out.push_str(&xml_escape(id));
    out.push_str("\" type=\"");
    out.push_str(&xml_escape(&attr.attr_type));

    // TranslatedString uses handle= instead of value=
    if attr.attr_type == "TranslatedString" || attr.attr_type == "TranslatedFSString" {
        out.push_str("\" handle=\"");
    } else {
        out.push_str("\" value=\"");
    }
    out.push_str(&xml_escape(&attr.value));
    out.push_str("\" />\n");
}

/// Write `<children>` blocks for all child groups.
fn write_children(out: &mut String, groups: &[LsxChildGroup], indent: usize) {
    let pad = " ".repeat(indent);
    out.push_str(&pad);
    out.push_str("<children>\n");

    for group in groups {
        // Check if this is a "container" pattern (group_id differs from entry node_ids)
        // or a "leaf" pattern (group_id matches entry node_ids).
        let is_container =
            !group.entries.is_empty() && group.entries.iter().any(|e| e.node_id != group.group_id);

        if is_container {
            // Container pattern: <node id="SubClasses"><children><node id="SubClass">...</children></node>
            let inner_pad = " ".repeat(indent + 2);
            out.push_str(&inner_pad);
            out.push_str("<node id=\"");
            out.push_str(&xml_escape(&group.group_id));
            out.push_str("\">\n");

            let child_pad = " ".repeat(indent + 4);
            out.push_str(&child_pad);
            out.push_str("<children>\n");

            for child in &group.entries {
                write_child_entry(out, child, indent + 6);
            }

            out.push_str(&child_pad);
            out.push_str("</children>\n");
            out.push_str(&inner_pad);
            out.push_str("</node>\n");
        } else {
            // Leaf pattern: each entry is a direct child node
            for child in &group.entries {
                write_child_entry(out, child, indent + 2);
            }
        }
    }

    out.push_str(&pad);
    out.push_str("</children>\n");
}

/// Write a single child entry node.
fn write_child_entry(out: &mut String, child: &crate::models::LsxChildEntry, indent: usize) {
    let pad = " ".repeat(indent);
    if child.object_guid.is_empty() {
        out.push_str(&pad);
        out.push_str("<node id=\"");
        out.push_str(&xml_escape(&child.node_id));
        out.push_str("\" />\n");
    } else {
        out.push_str(&pad);
        out.push_str("<node id=\"");
        out.push_str(&xml_escape(&child.node_id));
        out.push_str("\">\n");

        let attr_pad = " ".repeat(indent + 2);
        out.push_str(&attr_pad);
        out.push_str("<attribute id=\"Object\" type=\"guid\" value=\"");
        out.push_str(&xml_escape(&child.object_guid));
        out.push_str("\" />\n");

        out.push_str(&pad);
        out.push_str("</node>\n");
    }
}

/// Escape special XML characters in attribute values.
fn xml_escape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            _ => result.push(c),
        }
    }
    result
}

/// Reconstruct an `LsxEntry` from DiffEntry field maps (raw_attributes + raw_attribute_types + raw_children).
///
/// This is the inverse of `extract_raw_attributes()` / `extract_raw_children()` in diff.rs.
/// Used to convert frontend data back to `LsxEntry` for LSX serialization.
pub fn reconstruct_lsx_entry(
    uuid: &str,
    node_id: &str,
    raw_attributes: &HashMap<String, String>,
    raw_attribute_types: &HashMap<String, String>,
    raw_children: &HashMap<String, Vec<String>>,
) -> LsxEntry {
    let attributes: HashMap<String, LsxAttribute> = raw_attributes
        .iter()
        .map(|(k, v)| {
            let attr_type = raw_attribute_types
                .get(k)
                .cloned()
                .unwrap_or_else(|| infer_attribute_type(k, v));
            (
                k.clone(),
                LsxAttribute {
                    attr_type,
                    value: v.clone(),
                },
            )
        })
        .collect();

    let children: Vec<LsxChildGroup> = raw_children
        .iter()
        .map(|(group_id, guids)| LsxChildGroup {
            group_id: group_id.clone(),
            entries: guids
                .iter()
                .map(|guid| crate::models::LsxChildEntry {
                    node_id: singularize(group_id),
                    object_guid: guid.clone(),
                })
                .collect(),
        })
        .collect();

    LsxEntry {
        uuid: uuid.to_string(),
        node_id: node_id.to_string(),
        attributes,
        children,
        commented: false,
        region_id: String::new(),
    }
}

/// Infer an LSX attribute type from the key name and value when type info is unavailable.
fn infer_attribute_type(key: &str, value: &str) -> String {
    match key {
        "UUID" | "MapKey" => "guid".to_string(),
        _ => {
            // Check if value looks like a GUID
            if value.len() == 36
                && value.chars().filter(|c| *c == '-').count() == 4
                && value.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            {
                "guid".to_string()
            } else if value == "true" || value == "false" || value == "True" || value == "False" {
                "bool".to_string()
            } else if value.parse::<i64>().is_ok() {
                "int32".to_string()
            } else {
                "FixedString".to_string()
            }
        }
    }
}

/// Simple singularization for child group IDs → child node IDs.
/// E.g., "SubClasses" → "SubClass", "EyeColors" → "EyeColor"
fn singularize(group_id: &str) -> String {
    if group_id.ends_with('s') && group_id.len() > 1 {
        group_id[..group_id.len() - 1].to_string()
    } else {
        group_id.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_entry_xml() {
        let entry = LsxEntry {
            uuid: "abc-123".to_string(),
            node_id: "Progression".to_string(),
            region_id: String::new(),
            attributes: {
                let mut map = HashMap::new();
                map.insert(
                    "UUID".to_string(),
                    LsxAttribute {
                        attr_type: "guid".to_string(),
                        value: "abc-123".to_string(),
                    },
                );
                map.insert(
                    "Name".to_string(),
                    LsxAttribute {
                        attr_type: "LSString".to_string(),
                        value: "TestProgression".to_string(),
                    },
                );
                map
            },
            children: vec![],
            commented: false,
        };

        let xml = entries_to_lsx(&[entry], "Progressions");
        assert!(xml.contains("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(xml.contains("<region id=\"Progressions\">"));
        assert!(xml.contains("<node id=\"Progression\">"));
        assert!(xml.contains("id=\"UUID\" type=\"guid\" value=\"abc-123\""));
        assert!(xml.contains("id=\"Name\" type=\"LSString\" value=\"TestProgression\""));
    }

    #[test]
    fn test_entry_with_children() {
        let entry = LsxEntry {
            uuid: "abc-123".to_string(),
            node_id: "Progression".to_string(),
            region_id: String::new(),
            attributes: {
                let mut map = HashMap::new();
                map.insert(
                    "UUID".to_string(),
                    LsxAttribute {
                        attr_type: "guid".to_string(),
                        value: "abc-123".to_string(),
                    },
                );
                map
            },
            commented: false,
            children: vec![LsxChildGroup {
                group_id: "SubClasses".to_string(),
                entries: vec![crate::models::LsxChildEntry {
                    node_id: "SubClass".to_string(),
                    object_guid: "def-456".to_string(),
                }],
            }],
        };

        let xml = entries_to_lsx(&[entry], "Progressions");
        assert!(xml.contains("<node id=\"SubClasses\">"));
        assert!(xml.contains("<node id=\"SubClass\">"));
        assert!(xml.contains("id=\"Object\" type=\"guid\" value=\"def-456\""));
    }

    #[test]
    fn test_translated_string_uses_handle() {
        let entry = LsxEntry {
            uuid: "abc-123".to_string(),
            node_id: "Origin".to_string(),
            region_id: String::new(),
            attributes: {
                let mut map = HashMap::new();
                map.insert(
                    "DisplayName".to_string(),
                    LsxAttribute {
                        attr_type: "TranslatedString".to_string(),
                        value: "h12345".to_string(),
                    },
                );
                map
            },
            children: vec![],
            commented: false,
        };

        let xml = entries_to_lsx(&[entry], "Origins");
        assert!(xml.contains("handle=\"h12345\""));
        assert!(!xml.contains("value=\"h12345\""));
    }

    #[test]
    fn test_xml_escaping() {
        let entry = LsxEntry {
            uuid: "abc-123".to_string(),
            node_id: "Test".to_string(),
            commented: false,
            region_id: String::new(),
            attributes: {
                let mut map = HashMap::new();
                map.insert(
                    "Name".to_string(),
                    LsxAttribute {
                        attr_type: "LSString".to_string(),
                        value: "A & B <C> \"D\"".to_string(),
                    },
                );
                map
            },
            children: vec![],
        };

        let xml = entries_to_lsx(&[entry], "Test");
        assert!(xml.contains("A &amp; B &lt;C&gt; &quot;D&quot;"));
    }

    #[test]
    fn test_reconstruct_lsx_entry() {
        let mut attrs = HashMap::new();
        attrs.insert("UUID".to_string(), "abc-123".to_string());
        attrs.insert("Name".to_string(), "Test".to_string());

        let mut types = HashMap::new();
        types.insert("UUID".to_string(), "guid".to_string());
        types.insert("Name".to_string(), "LSString".to_string());

        let mut children = HashMap::new();
        children.insert("SubClasses".to_string(), vec!["def-456".to_string()]);

        let entry = reconstruct_lsx_entry("abc-123", "Progression", &attrs, &types, &children);

        assert_eq!(entry.uuid, "abc-123");
        assert_eq!(entry.node_id, "Progression");
        assert_eq!(entry.attributes["UUID"].attr_type, "guid");
        assert_eq!(entry.attributes["Name"].attr_type, "LSString");
        assert_eq!(entry.children.len(), 1);
        assert_eq!(entry.children[0].group_id, "SubClasses");
        assert_eq!(entry.children[0].entries[0].node_id, "SubClasse");
        assert_eq!(entry.children[0].entries[0].object_guid, "def-456");
    }

    #[test]
    fn test_infer_attribute_type() {
        assert_eq!(infer_attribute_type("UUID", "abc"), "guid");
        assert_eq!(
            infer_attribute_type("SomeField", "12345678-1234-1234-1234-123456789012"),
            "guid"
        );
        assert_eq!(infer_attribute_type("IsNew", "true"), "bool");
        assert_eq!(infer_attribute_type("Level", "5"), "int32");
        assert_eq!(infer_attribute_type("Name", "TestName"), "FixedString");
    }

    #[test]
    fn test_commented_entry() {
        let entry = LsxEntry {
            uuid: "abc-123".to_string(),
            node_id: "Progression".to_string(),
            commented: true,
            region_id: String::new(),
            attributes: {
                let mut map = HashMap::new();
                map.insert(
                    "UUID".to_string(),
                    LsxAttribute {
                        attr_type: "guid".to_string(),
                        value: "abc-123".to_string(),
                    },
                );
                map
            },
            children: vec![],
        };

        let xml = entries_to_lsx(&[entry], "Progressions");
        assert!(xml.contains("<!--"));
        assert!(xml.contains("-->"));
        assert!(xml.contains("<node id=\"Progression\">"));
        assert!(xml.contains("id=\"UUID\" type=\"guid\" value=\"abc-123\""));
    }
}
