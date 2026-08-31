use crate::models::{
    LsxAttribute, LsxChildEntry, LsxChildGroup, LsxEntry, LsxNode, LsxNodeAttribute, LsxRegion,
    LsxResource,
};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::collections::HashMap;

const MAX_NESTING_DEPTH: u32 = 2000;

/// Parse an LSX file and return all entries with their attributes and children.
pub fn parse_lsx_file(content: &str) -> Result<Vec<LsxEntry>, String> {
    let resource = parse_lsx_resource(content)?;
    Ok(resource_to_lsx_entries(&resource))
}

/// Parse an LSX file into a full-fidelity resource tree with explicit regions.
pub fn parse_lsx_resource(content: &str) -> Result<LsxResource, String> {
    let mut reader = Reader::from_str(content);
    let mut buf = Vec::new();
    let mut resource = LsxResource {
        regions: Vec::new(),
    };
    let mut current_region: Option<RegionBuilder> = None;
    let mut node_stack: Vec<NodeBuilder> = Vec::new();

    // Tracks `<node>` nesting only.
    let mut node_depth: u32 = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let tag_name = std::str::from_utf8(name.as_ref()).unwrap_or("");

                match tag_name {
                    "region" => {
                        let mut region_id = String::new();
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"id" {
                                region_id = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                        current_region = Some(RegionBuilder {
                            id: region_id,
                            nodes: Vec::new(),
                        });
                    }
                    "node" => {
                        let mut node_id = String::new();
                        let mut key_attribute = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"id" => {
                                    node_id = String::from_utf8_lossy(&attr.value).to_string();
                                }
                                b"key" => {
                                    key_attribute =
                                        Some(String::from_utf8_lossy(&attr.value).to_string());
                                }
                                _ => {}
                            }
                        }

                        node_stack.push(NodeBuilder {
                            id: node_id,
                            key_attribute,
                            attributes: Vec::new(),
                            children: Vec::new(),
                            commented: false,
                        });
                        node_depth += 1;
                        if node_depth > MAX_NESTING_DEPTH {
                            return Err(format!(
                                "LSX nesting depth exceeds {MAX_NESTING_DEPTH} limit"
                            ));
                        }
                    }
                    "attribute" => {
                        push_attribute(
                            &mut node_stack,
                            extract_attribute(e.attributes().flatten()),
                        );
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let tag_name = std::str::from_utf8(name.as_ref()).unwrap_or("");

                if tag_name == "attribute" {
                    push_attribute(&mut node_stack, extract_attribute(e.attributes().flatten()));
                } else if tag_name == "node" {
                    let mut node_id = String::new();
                    let mut key_attribute = None;
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => {
                                node_id = String::from_utf8_lossy(&attr.value).to_string();
                            }
                            b"key" => {
                                key_attribute =
                                    Some(String::from_utf8_lossy(&attr.value).to_string());
                            }
                            _ => {}
                        }
                    }

                    push_completed_node(
                        &mut node_stack,
                        &mut current_region,
                        LsxNode {
                            id: node_id,
                            key_attribute,
                            attributes: Vec::new(),
                            children: Vec::new(),
                            commented: false,
                        },
                    );
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let tag_name = std::str::from_utf8(name.as_ref()).unwrap_or("");

                match tag_name {
                    "node" => {
                        node_depth = node_depth.saturating_sub(1);
                        if let Some(builder) = node_stack.pop() {
                            push_completed_node(
                                &mut node_stack,
                                &mut current_region,
                                builder.build(),
                            );
                        }
                    }
                    "region" => {
                        if let Some(region) = current_region.take() {
                            resource.regions.push(LsxRegion {
                                id: region.id,
                                nodes: region.nodes,
                            });
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
            Ok(Event::Comment(ref comment)) => {
                let text = std::str::from_utf8(comment.as_ref()).unwrap_or("").trim();
                // Only attempt parsing if the comment looks like XML node content
                if text.contains("<node") || text.contains("<attribute") {
                    for node in parse_commented_nodes(text)? {
                        push_completed_node(&mut node_stack, &mut current_region, node);
                    }
                }
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(resource)
}

/// Convert a full resource tree back into the legacy flattened entry representation.
pub fn resource_to_lsx_entries(resource: &LsxResource) -> Vec<LsxEntry> {
    resource
        .regions
        .iter()
        .flat_map(|region| {
            flatten_region_entries(&region.nodes)
                .into_iter()
                .map(|mut e| {
                    e.region_id = region.id.clone();
                    e
                })
        })
        .collect()
}

fn flatten_region_entries(nodes: &[LsxNode]) -> Vec<LsxEntry> {
    let mut entries = Vec::new();
    for node in nodes {
        if node.id == "root" {
            entries.extend(flatten_region_entries(&node.children));
        } else {
            entries.push(node_to_lsx_entry(node));
        }
    }
    entries
}

/// Split a delimited LSString value into individual items.
pub fn split_delimited(value: &str, delimiter: char) -> Vec<String> {
    value
        .split(delimiter)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Get the delimiter for a given section/field combination.
pub fn get_delimiter(region: &str) -> char {
    match region {
        "PassiveLists" | "SkillLists" => ',',
        _ => ';',
    }
}

fn node_to_lsx_entry(node: &LsxNode) -> LsxEntry {
    let attributes: HashMap<String, LsxAttribute> = node
        .attributes
        .iter()
        .map(|attr| {
            (
                attr.id.clone(),
                LsxAttribute {
                    attr_type: attr.attr_type.clone(),
                    value: attr.handle.clone().unwrap_or_else(|| attr.value.clone()),
                },
            )
        })
        .collect();

    let uuid = attributes
        .get("UUID")
        .or_else(|| attributes.get("MapKey"))
        .map(|a| a.value.clone())
        .unwrap_or_default();

    LsxEntry {
        uuid,
        node_id: node.id.clone(),
        attributes,
        children: flatten_child_nodes(&node.children),
        commented: node.commented,
        region_id: String::new(),
    }
}

fn flatten_child_nodes(children: &[LsxNode]) -> Vec<LsxChildGroup> {
    let mut groups: Vec<LsxChildGroup> = Vec::new();

    for child in children {
        if child.children.is_empty() {
            push_child_group_entry(
                &mut groups,
                child.id.clone(),
                LsxChildEntry {
                    node_id: child.id.clone(),
                    object_guid: node_attribute_value(child, "Object").unwrap_or_default(),
                },
            );
            continue;
        }

        for entry in collect_leaf_child_entries(&child.children) {
            push_child_group_entry(&mut groups, child.id.clone(), entry);
        }
    }

    groups
}

fn collect_leaf_child_entries(nodes: &[LsxNode]) -> Vec<LsxChildEntry> {
    let mut entries = Vec::new();

    for node in nodes {
        if node.children.is_empty() {
            entries.push(LsxChildEntry {
                node_id: node.id.clone(),
                object_guid: node_attribute_value(node, "Object").unwrap_or_default(),
            });
        } else {
            entries.extend(collect_leaf_child_entries(&node.children));
        }
    }

    entries
}

fn push_child_group_entry(groups: &mut Vec<LsxChildGroup>, group_id: String, entry: LsxChildEntry) {
    if let Some(group) = groups.iter_mut().find(|g| g.group_id == group_id) {
        group.entries.push(entry);
    } else {
        groups.push(LsxChildGroup {
            group_id,
            entries: vec![entry],
        });
    }
}

fn node_attribute_value(node: &LsxNode, id: &str) -> Option<String> {
    node.attributes
        .iter()
        .find(|attr| attr.id == id)
        .map(|attr| attr.value.clone())
}

fn extract_attribute<'a, I>(attributes: I) -> Option<LsxNodeAttribute>
where
    I: Iterator<Item = quick_xml::events::attributes::Attribute<'a>>,
{
    let mut attr_id = String::new();
    let mut attr_type = String::new();
    let mut attr_value = String::new();
    let mut handle = None;
    let mut version = None;

    for attr in attributes {
        match attr.key.as_ref() {
            b"id" => {
                attr_id = String::from_utf8_lossy(&attr.value).to_string();
            }
            b"type" => {
                attr_type = String::from_utf8_lossy(&attr.value).to_string();
            }
            b"value" | b"handle" => {
                let decoded = attr
                    .unescape_value()
                    .map(|v| v.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(&attr.value).to_string());
                if attr.key.as_ref() == b"handle" {
                    handle = Some(decoded.clone());
                }
                attr_value = decoded;
            }
            b"version" => {
                version = String::from_utf8_lossy(&attr.value).parse::<u16>().ok();
            }
            _ => {}
        }
    }

    if attr_id.is_empty() {
        None
    } else {
        Some(LsxNodeAttribute {
            id: attr_id,
            attr_type,
            value: attr_value,
            handle,
            version,
            arguments: Vec::new(),
        })
    }
}

fn push_attribute(node_stack: &mut [NodeBuilder], attribute: Option<LsxNodeAttribute>) {
    if let (Some(current), Some(attribute)) = (node_stack.last_mut(), attribute) {
        current.attributes.push(attribute);
    }
}

fn push_completed_node(
    node_stack: &mut [NodeBuilder],
    current_region: &mut Option<RegionBuilder>,
    node: LsxNode,
) {
    if let Some(parent) = node_stack.last_mut() {
        parent.children.push(node);
    } else if let Some(region) = current_region.as_mut() {
        region.nodes.push(node);
    }
}

fn parse_commented_nodes(comment_text: &str) -> Result<Vec<LsxNode>, String> {
    let wrapped = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><save><region id="temp"><node id="root"><children>{comment_text}</children></node></region></save>"#
    );
    let mut resource = parse_lsx_resource(&wrapped)?;
    let mut nodes: Vec<LsxNode> = resource
        .regions
        .pop()
        .map(|region| {
            region
                .nodes
                .into_iter()
                .flat_map(|node| {
                    if node.id == "root" {
                        node.children
                    } else {
                        vec![node]
                    }
                })
                .collect::<Vec<LsxNode>>()
        })
        .unwrap_or_default();
    for node in &mut nodes {
        node.commented = true;
    }
    Ok(nodes)
}

struct RegionBuilder {
    id: String,
    nodes: Vec<LsxNode>,
}

struct NodeBuilder {
    id: String,
    key_attribute: Option<String>,
    attributes: Vec<LsxNodeAttribute>,
    children: Vec<LsxNode>,
    commented: bool,
}

impl NodeBuilder {
    fn build(self) -> LsxNode {
        LsxNode {
            id: self.id,
            key_attribute: self.key_attribute,
            attributes: self.attributes,
            children: self.children,
            commented: self.commented,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_LSX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Progressions">
        <node id="root">
            <children>
                <node id="Progression">
                    <attribute id="Boosts" type="LSString" value="ProficiencyBonus(SavingThrow,Strength);Proficiency(LightArmor)"/>
                    <attribute id="Level" type="uint8" value="1"/>
                    <attribute id="Name" type="LSString" value="Barbarian"/>
                    <attribute id="PassivesAdded" type="LSString" value="RageUnlock;UnarmouredDefence_Barbarian"/>
                    <attribute id="ProgressionType" type="uint8" value="0"/>
                    <attribute id="Selectors" type="LSString" value="SelectSkills(233793b3-838a-4d4e-9d68-1e0a1089aba5,2)"/>
                    <attribute id="TableUUID" type="guid" value="60bbcc97-5381-4898-bc15-908c072895de"/>
                    <attribute id="UUID" type="guid" value="a2198ee9-ea4c-468e-b6b4-22b32d37806e"/>
                </node>
            </children>
        </node>
    </region>
</save>"#;

    const LSX_WITH_CHILDREN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Progressions">
        <node id="root">
            <children>
                <node id="Progression">
                    <attribute id="Level" type="uint8" value="3"/>
                    <attribute id="Name" type="LSString" value="Barbarian"/>
                    <attribute id="ProgressionType" type="uint8" value="0"/>
                    <attribute id="UUID" type="guid" value="2b262c4c-1a66-468e-a13c-1faaf1e8a0a8"/>
                    <children>
                        <node id="SubClasses">
                            <children>
                                <node id="SubClass">
                                    <attribute id="Object" type="guid" value="5b3cd5ec-4234-434e-a6c5-da5fc01183cd"/>
                                </node>
                                <node id="SubClass">
                                    <attribute id="Object" type="guid" value="96a46341-c498-4b5e-a8fe-1b80470cc0e6"/>
                                </node>
                            </children>
                        </node>
                    </children>
                </node>
            </children>
        </node>
    </region>
</save>"#;

    const RACE_LSX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Races">
        <node id="root">
            <children>
                <node id="Race">
                    <attribute id="Name" type="FixedString" value="Human"/>
                    <attribute id="UUID" type="guid" value="0eb594cb-8820-4be6-a58d-8be7a1a98fba"/>
                    <children>
                        <node id="EyeColors">
                            <attribute id="Object" type="guid" value="aaa11111-1111-1111-1111-111111111111"/>
                        </node>
                        <node id="EyeColors">
                            <attribute id="Object" type="guid" value="bbb22222-2222-2222-2222-222222222222"/>
                        </node>
                        <node id="SkinColors">
                            <attribute id="Object" type="guid" value="ccc33333-3333-3333-3333-333333333333"/>
                        </node>
                    </children>
                </node>
            </children>
        </node>
    </region>
</save>"#;

    const SPELL_LIST_LSX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="SpellLists">
        <node id="root">
            <children>
                <node id="SpellList">
                    <attribute id="Name" type="FixedString" value="Cleric cantrips"/>
                    <attribute id="Spells" type="LSString" value="Shout_Thaumaturgy;Target_SacredFlame;Target_Guidance"/>
                    <attribute id="UUID" type="guid" value="2f43a103-5bf1-4534-b14f-663decc0c525"/>
                </node>
            </children>
        </node>
    </region>
</save>"#;

    #[test]
    fn test_parse_simple_progression() {
        let entries = parse_lsx_file(SIMPLE_LSX).unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.uuid, "a2198ee9-ea4c-468e-b6b4-22b32d37806e");
        assert_eq!(entry.node_id, "Progression");
        assert_eq!(entry.attributes.get("Name").unwrap().value, "Barbarian");
        assert_eq!(entry.attributes.get("Level").unwrap().value, "1");
        assert_eq!(
            entry.attributes.get("Boosts").unwrap().value,
            "ProficiencyBonus(SavingThrow,Strength);Proficiency(LightArmor)"
        );
    }

    #[test]
    fn test_parse_progression_with_subclasses() {
        let entries = parse_lsx_file(LSX_WITH_CHILDREN).unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.uuid, "2b262c4c-1a66-468e-a13c-1faaf1e8a0a8");

        // Should have a SubClasses child group
        assert_eq!(entry.children.len(), 1);
        let subclasses = &entry.children[0];
        assert_eq!(subclasses.group_id, "SubClasses");
        assert_eq!(subclasses.entries.len(), 2);
        assert_eq!(
            subclasses.entries[0].object_guid,
            "5b3cd5ec-4234-434e-a6c5-da5fc01183cd"
        );
        assert_eq!(
            subclasses.entries[1].object_guid,
            "96a46341-c498-4b5e-a8fe-1b80470cc0e6"
        );
    }

    #[test]
    fn test_parse_race_with_cosmetic_children() {
        let entries = parse_lsx_file(RACE_LSX).unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.uuid, "0eb594cb-8820-4be6-a58d-8be7a1a98fba");
        assert_eq!(entry.attributes.get("Name").unwrap().value, "Human");

        // Should have EyeColors and SkinColors child groups
        assert_eq!(entry.children.len(), 2);

        let eye_group = entry
            .children
            .iter()
            .find(|g| g.group_id == "EyeColors")
            .unwrap();
        assert_eq!(eye_group.entries.len(), 2);

        let skin_group = entry
            .children
            .iter()
            .find(|g| g.group_id == "SkinColors")
            .unwrap();
        assert_eq!(skin_group.entries.len(), 1);
    }

    #[test]
    fn test_parse_spell_list() {
        let entries = parse_lsx_file(SPELL_LIST_LSX).unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.uuid, "2f43a103-5bf1-4534-b14f-663decc0c525");
        assert_eq!(
            entry.attributes.get("Name").unwrap().value,
            "Cleric cantrips"
        );

        let spells_val = &entry.attributes.get("Spells").unwrap().value;
        let items = split_delimited(spells_val, ';');
        assert_eq!(
            items,
            vec!["Shout_Thaumaturgy", "Target_SacredFlame", "Target_Guidance"]
        );
    }

    #[test]
    fn test_split_delimited_semicolon() {
        let items = split_delimited("A;B;C", ';');
        assert_eq!(items, vec!["A", "B", "C"]);
    }

    #[test]
    fn test_split_delimited_comma() {
        let items = split_delimited("Passive1, Passive2, Passive3", ',');
        assert_eq!(items, vec!["Passive1", "Passive2", "Passive3"]);
    }

    #[test]
    fn test_split_delimited_empty() {
        let items = split_delimited("", ';');
        assert!(items.is_empty());
    }

    #[test]
    fn test_parse_empty_lsx() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Progressions">
        <node id="root">
            <children>
            </children>
        </node>
    </region>
</save>"#;
        let entries = parse_lsx_file(content).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_multiple_entries() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="SpellLists">
        <node id="root">
            <children>
                <node id="SpellList">
                    <attribute id="Name" type="FixedString" value="List A"/>
                    <attribute id="Spells" type="LSString" value="SpellA;SpellB"/>
                    <attribute id="UUID" type="guid" value="aaaa1111-1111-1111-1111-111111111111"/>
                </node>
                <node id="SpellList">
                    <attribute id="Name" type="FixedString" value="List B"/>
                    <attribute id="Spells" type="LSString" value="SpellC"/>
                    <attribute id="UUID" type="guid" value="bbbb2222-2222-2222-2222-222222222222"/>
                </node>
            </children>
        </node>
    </region>
</save>"#;
        let entries = parse_lsx_file(content).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].uuid, "aaaa1111-1111-1111-1111-111111111111");
        assert_eq!(entries[1].uuid, "bbbb2222-2222-2222-2222-222222222222");
    }

    #[test]
    fn test_parse_resource_preserves_regions_and_nested_attributes() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Config">
        <node id="root">
            <children>
                <node id="ModuleInfo">
                    <attribute id="UUID" type="guid" value="11111111-1111-1111-1111-111111111111"/>
                    <children>
                        <node id="PublishVersion">
                            <attribute id="Version64" type="int64" value="36028797018963968"/>
                        </node>
                    </children>
                </node>
            </children>
        </node>
    </region>
</save>"#;

        let resource = parse_lsx_resource(content).unwrap();
        assert_eq!(resource.regions.len(), 1);
        assert_eq!(resource.regions[0].id, "Config");
        assert_eq!(resource.regions[0].nodes.len(), 1);

        let root = &resource.regions[0].nodes[0];
        assert_eq!(root.id, "root");
        assert_eq!(root.children.len(), 1);

        let module_info = &root.children[0];
        assert_eq!(module_info.id, "ModuleInfo");
        assert_eq!(module_info.children.len(), 1);
        assert_eq!(module_info.children[0].id, "PublishVersion");
        assert_eq!(module_info.children[0].attributes.len(), 1);
        assert_eq!(module_info.children[0].attributes[0].id, "Version64");
        assert_eq!(
            module_info.children[0].attributes[0].value,
            "36028797018963968"
        );
    }

    #[test]
    fn test_parse_resource_preserves_multiple_regions() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="First">
        <node id="root">
            <children>
                <node id="EntryA">
                    <attribute id="UUID" type="guid" value="aaaa1111-1111-1111-1111-111111111111"/>
                </node>
            </children>
        </node>
    </region>
    <region id="Second">
        <node id="root">
            <children>
                <node id="EntryB">
                    <attribute id="UUID" type="guid" value="bbbb2222-2222-2222-2222-222222222222"/>
                </node>
            </children>
        </node>
    </region>
</save>"#;

        let resource = parse_lsx_resource(content).unwrap();
        assert_eq!(resource.regions.len(), 2);
        assert_eq!(resource.regions[0].id, "First");
        assert_eq!(resource.regions[1].id, "Second");
        assert_eq!(resource.regions[0].nodes[0].id, "root");
        assert_eq!(resource.regions[1].nodes[0].id, "root");

        let entries = resource_to_lsx_entries(&resource);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].node_id, "EntryA");
        assert_eq!(entries[1].node_id, "EntryB");
    }

    #[test]
    fn test_parse_resource_preserves_key_and_translated_metadata() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Meta">
        <node id="root">
            <children>
                <node id="Entry" key="MapKey">
                    <attribute id="DisplayName" type="TranslatedString" handle="h123" version="7"/>
                </node>
            </children>
        </node>
    </region>
</save>"#;

        let resource = parse_lsx_resource(content).unwrap();
        let root = &resource.regions[0].nodes[0];
        assert_eq!(root.id, "root");

        let entry = &root.children[0];
        assert_eq!(entry.key_attribute.as_deref(), Some("MapKey"));
        assert_eq!(entry.attributes[0].handle.as_deref(), Some("h123"));
        assert_eq!(entry.attributes[0].version, Some(7));

        let flattened = resource_to_lsx_entries(&resource);
        assert_eq!(flattened[0].attributes["DisplayName"].value, "h123");
    }

    #[test]
    fn test_attribute_value_xml_entity_unescaping() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="EffectInfo">
        <node id="root">
            <children>
                <node id="Effect">
                    <attribute id="TargetBone" type="FixedString" value="&lt;click to select&gt;"/>
                    <attribute id="Escaped" type="LSString" value="A &amp; B &lt; C &gt; D &quot;E&quot; &apos;F&apos;"/>
                </node>
            </children>
        </node>
    </region>
</save>"#;

        let resource = parse_lsx_resource(content).unwrap();
        let root = &resource.regions[0].nodes[0];
        let effect = &root.children[0];

        let target_bone = effect
            .attributes
            .iter()
            .find(|a| a.id == "TargetBone")
            .unwrap();
        assert_eq!(target_bone.value, "<click to select>");

        let escaped = effect
            .attributes
            .iter()
            .find(|a| a.id == "Escaped")
            .unwrap();
        assert_eq!(escaped.value, "A & B < C > D \"E\" 'F'");
    }
}
