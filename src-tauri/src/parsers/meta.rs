use crate::models::{LsxNode, LsxResource, MetaDependency, ModMeta};
use crate::parsers::lsx::parse_lsx_file;
use crate::parsers::lsx_yaml::{LsxYamlDoc, LsxYamlNode};

/// Parse a meta.lsx file and extract mod metadata.
pub fn parse_meta_lsx(content: &str) -> Result<ModMeta, String> {
    let entries = parse_lsx_file(content)?;

    // Find the ModuleInfo node
    let module_info = entries
        .iter()
        .find(|e| e.node_id == "ModuleInfo")
        .ok_or_else(|| "No ModuleInfo node found in meta.lsx".to_string())?;

    let get_attr = |key: &str| -> String {
        module_info
            .attributes
            .get(key)
            .map(|a| a.value.clone())
            .unwrap_or_default()
    };

    let uuid = get_attr("UUID");
    let folder = get_attr("Folder");
    let name = get_attr("Name");
    let author = get_attr("Author");
    let version64_raw = get_attr("Version64");
    let version = decode_version64(&version64_raw);
    let description = get_attr("Description");
    let md5 = get_attr("MD5");
    let mod_type = get_attr("Type");
    let tags = get_attr("Tags");
    let num_players = get_attr("NumPlayers");
    let gm_template = get_attr("GMTemplate");
    let char_creation_level = get_attr("CharacterCreationLevelName");
    let lobby_level = get_attr("LobbyLevelName");
    let menu_level = get_attr("MenuLevelName");
    let startup_level = get_attr("StartupLevelName");
    let photo_booth = get_attr("PhotoBooth");
    let main_menu_bg_video = get_attr("MainMenuBackgroundVideo");

    // Extract PublishVersion from ModuleInfo's children
    let publish_version = module_info
        .children
        .iter()
        .find(|g| g.group_id == "PublishVersion")
        .and({
            // PublishVersion Version64 is lost in generic parser's LsxChildEntry,
            // so fall back to regex extraction from raw content
            None::<String>
        })
        .unwrap_or_default();

    // Extract TargetMode from ModuleInfo's children (Target child has object_guid = "Story" etc.)
    let target_mode = module_info
        .children
        .iter()
        .find(|g| g.group_id == "TargetModes")
        .and_then(|g| g.entries.first())
        .map(|e| e.object_guid.clone())
        .unwrap_or_default();

    // The generic LSX parser loses nested attribute values (PublishVersion/Version64,
    // dependency attributes), so extract them directly from the raw XML content.
    let publish_version = if publish_version.is_empty() {
        extract_publish_version(content)
    } else {
        publish_version
    };

    // Parse dependencies from the Dependencies node
    let dependencies = extract_dependencies(content);

    Ok(ModMeta {
        uuid,
        folder,
        name,
        author,
        version,
        version64: version64_raw,
        description,
        md5,
        mod_type,
        tags,
        num_players,
        gm_template,
        char_creation_level,
        lobby_level,
        menu_level,
        startup_level,
        photo_booth,
        main_menu_bg_video,
        publish_version,
        target_mode,
        dependencies,
    })
}

/// Parse a meta.yaml file (converted from meta.lsx) and extract mod metadata.
pub fn parse_meta_yaml(content: &str) -> Result<ModMeta, String> {
    let doc: LsxYamlDoc =
        serde_saphyr::from_str(content).map_err(|e| format!("Failed to parse meta.yaml: {e}"))?;

    let module_info = doc
        .nodes
        .iter()
        .find(|n| n.id == "ModuleInfo")
        .ok_or_else(|| "No ModuleInfo node found in meta.yaml".to_string())?;

    let get_attr = |key: &str| -> String {
        module_info
            .attributes
            .iter()
            .find(|a| a.id == key)
            .map(|a| a.value.clone())
            .unwrap_or_default()
    };

    let uuid = get_attr("UUID");
    let folder = get_attr("Folder");
    let name = get_attr("Name");
    let author = get_attr("Author");
    let version64_raw = get_attr("Version64");
    let version = decode_version64(&version64_raw);
    let description = get_attr("Description");
    let md5 = get_attr("MD5");
    let mod_type = get_attr("Type");
    let tags = get_attr("Tags");
    let num_players = get_attr("NumPlayers");
    let gm_template = get_attr("GMTemplate");
    let char_creation_level = get_attr("CharacterCreationLevelName");
    let lobby_level = get_attr("LobbyLevelName");
    let menu_level = get_attr("MenuLevelName");
    let startup_level = get_attr("StartupLevelName");
    let photo_booth = get_attr("PhotoBooth");
    let main_menu_bg_video = get_attr("MainMenuBackgroundVideo");

    // Extract PublishVersion from children
    let publish_version = find_child_attr(module_info, "PublishVersion", "Version64");

    // Extract TargetMode from TargetModes → Target → Object
    let target_mode = module_info
        .children
        .iter()
        .find(|c| c.id == "TargetModes")
        .and_then(|tm| tm.children.first())
        .and_then(|target| target.attributes.iter().find(|a| a.id == "Object"))
        .map(|a| a.value.clone())
        .unwrap_or_default();

    // Extract dependencies from the Dependencies node
    let dependencies = doc
        .nodes
        .iter()
        .find(|n| n.id == "Dependencies")
        .map(|deps_node| {
            deps_node
                .children
                .iter()
                .filter(|c| c.id == "ModuleShortDesc")
                .map(|child| {
                    let dep_attr = |key: &str| -> String {
                        child
                            .attributes
                            .iter()
                            .find(|a| a.id == key)
                            .map(|a| a.value.clone())
                            .unwrap_or_default()
                    };
                    MetaDependency {
                        uuid: dep_attr("UUID"),
                        name: dep_attr("Name"),
                        folder: dep_attr("Folder"),
                        md5: dep_attr("MD5"),
                        version64: dep_attr("Version64"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ModMeta {
        uuid,
        folder,
        name,
        author,
        version,
        version64: version64_raw,
        description,
        md5,
        mod_type,
        tags,
        num_players,
        gm_template,
        char_creation_level,
        lobby_level,
        menu_level,
        startup_level,
        photo_booth,
        main_menu_bg_video,
        publish_version,
        target_mode,
        dependencies,
    })
}

/// Find a child node's attribute value by child id and attribute id.
fn find_child_attr(node: &LsxYamlNode, child_id: &str, attr_id: &str) -> String {
    node.children
        .iter()
        .find(|c| c.id == child_id)
        .and_then(|c| c.attributes.iter().find(|a| a.id == attr_id))
        .map(|a| a.value.clone())
        .unwrap_or_default()
}

/// Extract mod metadata from a parsed `LsxResource` (e.g. from native binary .lsf parsing).
/// Navigates the `LsxNode` tree directly — no XML round-trip needed.
pub fn parse_meta_from_resource(resource: &LsxResource) -> Result<ModMeta, String> {
    // meta.lsx structure: region "Config" → root → children → ModuleInfo node
    let module_info = resource
        .regions
        .iter()
        .flat_map(|r| r.nodes.iter())
        .flat_map(|n| find_nodes_recursive(n, "ModuleInfo"))
        .next()
        .ok_or_else(|| "No ModuleInfo node found in meta resource".to_string())?;

    let get_attr = |key: &str| -> String {
        module_info
            .attributes
            .iter()
            .find(|a| a.id == key)
            .map(|a| a.value.clone())
            .unwrap_or_default()
    };

    let uuid = get_attr("UUID");
    let folder = get_attr("Folder");
    let name = get_attr("Name");
    let author = get_attr("Author");
    let version64_raw = get_attr("Version64");
    let version = decode_version64(&version64_raw);
    let description = get_attr("Description");
    let md5 = get_attr("MD5");
    let mod_type = get_attr("Type");
    let tags = get_attr("Tags");
    let num_players = get_attr("NumPlayers");
    let gm_template = get_attr("GMTemplate");
    let char_creation_level = get_attr("CharacterCreationLevelName");
    let lobby_level = get_attr("LobbyLevelName");
    let menu_level = get_attr("MenuLevelName");
    let startup_level = get_attr("StartupLevelName");
    let photo_booth = get_attr("PhotoBooth");
    let main_menu_bg_video = get_attr("MainMenuBackgroundVideo");

    // PublishVersion: child node "PublishVersion" → attribute "Version64"
    let publish_version = module_info
        .children
        .iter()
        .find(|c| c.id == "PublishVersion")
        .and_then(|pv| pv.attributes.iter().find(|a| a.id == "Version64"))
        .map(|a| a.value.clone())
        .unwrap_or_default();

    // TargetModes: child node "TargetModes" → first "Target" child → attribute "Object"
    let target_mode = module_info
        .children
        .iter()
        .find(|c| c.id == "TargetModes")
        .and_then(|tm| tm.children.first())
        .and_then(|target| target.attributes.iter().find(|a| a.id == "Object"))
        .map(|a| a.value.clone())
        .unwrap_or_default();

    // Dependencies: sibling node "Dependencies" → children "ModuleShortDesc"
    let dependencies = resource
        .regions
        .iter()
        .flat_map(|r| r.nodes.iter())
        .flat_map(|n| find_nodes_recursive(n, "Dependencies"))
        .next()
        .map(|deps_node| {
            deps_node
                .children
                .iter()
                .filter(|c| c.id == "ModuleShortDesc")
                .map(|child| {
                    let dep_attr = |key: &str| -> String {
                        child
                            .attributes
                            .iter()
                            .find(|a| a.id == key)
                            .map(|a| a.value.clone())
                            .unwrap_or_default()
                    };
                    MetaDependency {
                        uuid: dep_attr("UUID"),
                        name: dep_attr("Name"),
                        folder: dep_attr("Folder"),
                        md5: dep_attr("MD5"),
                        version64: dep_attr("Version64"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ModMeta {
        uuid,
        folder,
        name,
        author,
        version,
        version64: version64_raw,
        description,
        md5,
        mod_type,
        tags,
        num_players,
        gm_template,
        char_creation_level,
        lobby_level,
        menu_level,
        startup_level,
        photo_booth,
        main_menu_bg_video,
        publish_version,
        target_mode,
        dependencies,
    })
}

/// Recursively find all nodes with a given id in the tree.
fn find_nodes_recursive<'a>(node: &'a LsxNode, target_id: &str) -> Vec<&'a LsxNode> {
    let mut result = Vec::new();
    if node.id == target_id {
        result.push(node);
    }
    for child in &node.children {
        result.extend(find_nodes_recursive(child, target_id));
    }
    result
}

/// Extract PublishVersion's Version64 from raw meta.lsx XML content.
/// The generic LSX parser's LsxChildEntry doesn't preserve arbitrary attributes,
/// so we do a targeted extraction here.
fn extract_publish_version(content: &str) -> String {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(content);
    let mut buf = Vec::new();
    let mut in_publish_version = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "node" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"id" {
                            let val = String::from_utf8_lossy(&attr.value);
                            in_publish_version = val == "PublishVersion";
                        }
                    }
                } else if tag == "attribute" && in_publish_version {
                    let mut attr_id = String::new();
                    let mut attr_value = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => attr_id = String::from_utf8_lossy(&attr.value).to_string(),
                            b"value" => {
                                attr_value = String::from_utf8_lossy(&attr.value).to_string()
                            }
                            _ => {}
                        }
                    }
                    if attr_id == "Version64" {
                        return attr_value;
                    }
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "attribute" && in_publish_version {
                    let mut attr_id = String::new();
                    let mut attr_value = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => attr_id = String::from_utf8_lossy(&attr.value).to_string(),
                            b"value" => {
                                attr_value = String::from_utf8_lossy(&attr.value).to_string()
                            }
                            _ => {}
                        }
                    }
                    if attr_id == "Version64" {
                        return attr_value;
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "node" && in_publish_version {
                    in_publish_version = false;
                }
            }
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    String::new()
}

/// Extract dependency entries from the Dependencies node in raw meta.lsx XML.
/// Each ModuleShortDesc child has Folder, MD5, Name, UUID, Version64 attributes.
fn extract_dependencies(content: &str) -> Vec<MetaDependency> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_str(content);
    let mut buf = Vec::new();
    let mut deps = Vec::new();
    let mut in_dependencies = false;
    let mut in_short_desc = false;
    let mut current_attrs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "node" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"id" {
                            let val = String::from_utf8_lossy(&attr.value);
                            if val == "Dependencies" {
                                in_dependencies = true;
                            } else if in_dependencies && val == "ModuleShortDesc" {
                                in_short_desc = true;
                                current_attrs.clear();
                            }
                        }
                    }
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "attribute" && in_short_desc {
                    let mut attr_id = String::new();
                    let mut attr_value = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"id" => attr_id = String::from_utf8_lossy(&attr.value).to_string(),
                            b"value" => {
                                attr_value = String::from_utf8_lossy(&attr.value).to_string()
                            }
                            _ => {}
                        }
                    }
                    if !attr_id.is_empty() {
                        current_attrs.insert(attr_id, attr_value);
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if tag == "node" {
                    if in_short_desc {
                        deps.push(MetaDependency {
                            uuid: current_attrs.remove("UUID").unwrap_or_default(),
                            name: current_attrs.remove("Name").unwrap_or_default(),
                            folder: current_attrs.remove("Folder").unwrap_or_default(),
                            md5: current_attrs.remove("MD5").unwrap_or_default(),
                            version64: current_attrs.remove("Version64").unwrap_or_default(),
                        });
                        in_short_desc = false;
                    } else if in_dependencies {
                        in_dependencies = false;
                    }
                }
            }
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    deps
}

/// Decode a Version64 integer string into a human-readable "major.minor.revision.build" string.
/// BG3 packs version components into a 64-bit integer as:
///   major  = bits 55..32 (top 23 bits of high 32)
///   minor  = bits 31..16
///   revision = bits 15..8  (called "revision" in BG3)
///   build  = bits 7..0
/// Example: 36028797018963968 → 1.0.0.0
fn decode_version64(raw: &str) -> String {
    let v: u64 = match raw.parse() {
        Ok(n) => n,
        Err(_) => return String::new(),
    };
    if v == 0 {
        return String::new();
    }
    let major = (v >> 55) & 0x7F_FFFF; // 23 bits
    let minor = (v >> 47) & 0xFF; // 8 bits
    let revision = (v >> 31) & 0xFFFF; // 16 bits
    let build = v & 0x7FFF_FFFF; // 31 bits
    format!("{major}.{minor}.{revision}.{build}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const META_LSX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Config">
        <node id="root">
            <children>
                <node id="Dependencies">
                    <children>
                        <node id="ModuleShortDesc">
                            <attribute id="Folder" type="LSWString" value="CommunityLibrary"/>
                            <attribute id="MD5" type="LSString" value=""/>
                            <attribute id="Name" type="FixedString" value="CommunityLibrary"/>
                            <attribute id="UUID" type="FixedString" value="396c5966-09b0-40a1-af3f-93a5e9ce71c0"/>
                            <attribute id="Version64" type="int64" value="72479806502993920"/>
                        </node>
                    </children>
                </node>
                <node id="ModuleInfo">
                    <attribute id="Author" type="LSString" value="TestAuthor"/>
                    <attribute id="Description" type="LSWString" value="A test mod"/>
                    <attribute id="Folder" type="LSString" value="TestMod"/>
                    <attribute id="MD5" type="LSString" value="abc123"/>
                    <attribute id="Name" type="FixedString" value="Test Mod Name"/>
                    <attribute id="NumPlayers" type="uint8" value="4"/>
                    <attribute id="Tags" type="LSWString" value="Classes;CMTY"/>
                    <attribute id="Type" type="FixedString" value="Add-on"/>
                    <attribute id="UUID" type="FixedString" value="12345678-1234-1234-1234-123456789abc"/>
                    <attribute id="Version64" type="int64" value="36028797018963968"/>
                    <children>
                        <node id="PublishVersion">
                            <attribute id="Version64" type="int64" value="36028797018963968"/>
                        </node>
                        <node id="TargetModes">
                            <children>
                                <node id="Target">
                                    <attribute id="Object" type="FixedString" value="Story"/>
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
    fn test_parse_meta_lsx() {
        let meta = parse_meta_lsx(META_LSX).unwrap();
        assert_eq!(meta.uuid, "12345678-1234-1234-1234-123456789abc");
        assert_eq!(meta.folder, "TestMod");
        assert_eq!(meta.name, "Test Mod Name");
        assert_eq!(meta.author, "TestAuthor");
        assert_eq!(meta.version, "1.0.0.0");
        assert_eq!(meta.version64, "36028797018963968");
        assert_eq!(meta.description, "A test mod");
        assert_eq!(meta.md5, "abc123");
        assert_eq!(meta.mod_type, "Add-on");
        assert_eq!(meta.tags, "Classes;CMTY");
        assert_eq!(meta.num_players, "4");
        assert_eq!(meta.publish_version, "36028797018963968");
        assert_eq!(meta.target_mode, "Story");
    }

    #[test]
    fn test_parse_meta_lsx_dependencies() {
        let meta = parse_meta_lsx(META_LSX).unwrap();
        assert_eq!(meta.dependencies.len(), 1);
        let dep = &meta.dependencies[0];
        assert_eq!(dep.uuid, "396c5966-09b0-40a1-af3f-93a5e9ce71c0");
        assert_eq!(dep.name, "CommunityLibrary");
        assert_eq!(dep.folder, "CommunityLibrary");
        assert_eq!(dep.version64, "72479806502993920");
    }

    #[test]
    fn test_decode_version64() {
        assert_eq!(decode_version64("36028797018963968"), "1.0.0.0");
        assert_eq!(decode_version64("144115196665790673"), "4.0.4.209");
        assert_eq!(decode_version64(""), "");
        assert_eq!(decode_version64("0"), "");
        assert_eq!(decode_version64("not-a-number"), "");
    }

    #[test]
    fn test_parse_meta_lsx_missing_module_info() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<save>
    <version major="4" minor="0" revision="0" build="0"/>
    <region id="Config">
        <node id="root">
            <children>
            </children>
        </node>
    </region>
</save>"#;
        let result = parse_meta_lsx(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_meta_from_resource() {
        use crate::models::{LsxNode, LsxNodeAttribute, LsxRegion, LsxResource};

        let mk_attr = |id: &str, value: &str| LsxNodeAttribute {
            id: id.to_string(),
            attr_type: "FixedString".to_string(),
            value: value.to_string(),
            handle: None,
            version: None,
            arguments: vec![],
        };

        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Config".to_string(),
                nodes: vec![LsxNode {
                    id: "root".to_string(),
                    key_attribute: None,
                    attributes: vec![],
                    commented: false,
                    children: vec![
                        LsxNode {
                            id: "Dependencies".to_string(),
                            key_attribute: None,
                            attributes: vec![],
                            commented: false,
                            children: vec![LsxNode {
                                id: "ModuleShortDesc".to_string(),
                                key_attribute: None,
                                attributes: vec![
                                    mk_attr("Folder", "CommunityLibrary"),
                                    mk_attr("MD5", ""),
                                    mk_attr("Name", "CommunityLibrary"),
                                    mk_attr("UUID", "396c5966-09b0-40a1-af3f-93a5e9ce71c0"),
                                    mk_attr("Version64", "72479806502993920"),
                                ],
                                commented: false,
                                children: vec![],
                            }],
                        },
                        LsxNode {
                            id: "ModuleInfo".to_string(),
                            key_attribute: None,
                            attributes: vec![
                                mk_attr("Author", "TestAuthor"),
                                mk_attr("Description", "A test mod"),
                                mk_attr("Folder", "TestMod"),
                                mk_attr("MD5", "abc123"),
                                mk_attr("Name", "Test Mod Name"),
                                mk_attr("NumPlayers", "4"),
                                mk_attr("Tags", "Classes;CMTY"),
                                mk_attr("Type", "Add-on"),
                                mk_attr("UUID", "12345678-1234-1234-1234-123456789abc"),
                                mk_attr("Version64", "36028797018963968"),
                            ],
                            commented: false,
                            children: vec![
                                LsxNode {
                                    id: "PublishVersion".to_string(),
                                    key_attribute: None,
                                    attributes: vec![mk_attr("Version64", "36028797018963968")],
                                    commented: false,
                                    children: vec![],
                                },
                                LsxNode {
                                    id: "TargetModes".to_string(),
                                    key_attribute: None,
                                    attributes: vec![],
                                    commented: false,
                                    children: vec![LsxNode {
                                        id: "Target".to_string(),
                                        key_attribute: None,
                                        attributes: vec![mk_attr("Object", "Story")],
                                        commented: false,
                                        children: vec![],
                                    }],
                                },
                            ],
                        },
                    ],
                }],
            }],
        };

        let meta = parse_meta_from_resource(&resource).unwrap();
        assert_eq!(meta.uuid, "12345678-1234-1234-1234-123456789abc");
        assert_eq!(meta.folder, "TestMod");
        assert_eq!(meta.name, "Test Mod Name");
        assert_eq!(meta.author, "TestAuthor");
        assert_eq!(meta.version, "1.0.0.0");
        assert_eq!(meta.description, "A test mod");
        assert_eq!(meta.md5, "abc123");
        assert_eq!(meta.mod_type, "Add-on");
        assert_eq!(meta.tags, "Classes;CMTY");
        assert_eq!(meta.publish_version, "36028797018963968");
        assert_eq!(meta.target_mode, "Story");

        // Dependencies
        assert_eq!(meta.dependencies.len(), 1);
        assert_eq!(meta.dependencies[0].name, "CommunityLibrary");
        assert_eq!(
            meta.dependencies[0].uuid,
            "396c5966-09b0-40a1-af3f-93a5e9ce71c0"
        );
    }

    #[test]
    fn test_parse_meta_from_resource_missing_module_info() {
        use crate::models::{LsxNode, LsxRegion, LsxResource};
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Config".to_string(),
                nodes: vec![LsxNode {
                    id: "root".to_string(),
                    key_attribute: None,
                    attributes: vec![],
                    commented: false,
                    children: vec![],
                }],
            }],
        };
        let result = parse_meta_from_resource(&resource);
        assert!(result.is_err());
    }
}
