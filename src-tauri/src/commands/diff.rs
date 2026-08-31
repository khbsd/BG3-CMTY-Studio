use crate::models::*;
use crate::parsers::lsx::split_delimited;
use std::collections::{HashMap, HashSet};

/// Extract raw attributes from an LSX entry as a simple key→value map.
fn extract_raw_attributes(entry: &LsxEntry) -> HashMap<String, String> {
    entry
        .attributes
        .iter()
        .map(|(k, a)| (k.clone(), a.value.clone()))
        .collect()
}

/// Extract attribute types from an LSX entry as key→type map (e.g. "UUID" → "guid").
fn extract_raw_attribute_types(entry: &LsxEntry) -> HashMap<String, String> {
    entry
        .attributes
        .iter()
        .map(|(k, a)| (k.clone(), a.attr_type.clone()))
        .collect()
}

/// Extract raw children groups from an LSX entry as group_id → list of object GUIDs.
fn extract_raw_children(entry: &LsxEntry) -> HashMap<String, Vec<String>> {
    entry
        .children
        .iter()
        .map(|g| {
            (
                g.group_id.clone(),
                g.entries.iter().map(|e| e.object_guid.clone()).collect(),
            )
        })
        .collect()
}

/// Diff mod LSX entries against vanilla entries for a specific section.
pub fn diff_section(
    section: Section,
    mod_entries: &[LsxEntry],
    vanilla: &HashMap<String, LsxEntry>,
    source_file: &str,
) -> Vec<DiffEntry> {
    let mut results = Vec::new();
    let allowed_node_ids = section.expected_node_ids();

    for mod_entry in mod_entries {
        // TextureAtlasInfo / IconUVList child nodes may lack UUIDs.
        // Use node_id + MapKey (when available) as synthetic identifier.
        // IconUV nodes share a node_id but each has a unique MapKey attribute.
        let effective_uuid = if mod_entry.uuid.is_empty() {
            if section == Section::TextureAtlasInfo || section == Section::IconUVList {
                let map_key = mod_entry
                    .attributes
                    .get("MapKey")
                    .map(|a| a.value.as_str())
                    .unwrap_or("");
                if map_key.is_empty() {
                    format!("__node_{}__", mod_entry.node_id)
                } else {
                    format!("__node_{}_{}__", mod_entry.node_id, map_key)
                }
            } else {
                continue;
            }
        } else {
            mod_entry.uuid.clone()
        };

        // Skip entries whose node_id doesn't match the expected type for this section
        if let Some(ids) = allowed_node_ids {
            if !ids.iter().any(|id| mod_entry.node_id == *id) {
                continue;
            }
        }

        match vanilla.get(&effective_uuid) {
            None => {
                // New entry — not in vanilla
                results.push(DiffEntry {
                    uuid: effective_uuid.clone(),
                    display_name: build_display_name(mod_entry, section),
                    source_file: source_file.to_string(),
                    entry_kind: EntryKind::New,
                    changes: vec![Change {
                        change_type: ChangeType::EntireEntryNew,
                        field: "".to_string(),
                        added_values: vec![],
                        removed_values: vec![],
                        vanilla_value: None,
                        mod_value: None,
                    }],
                    node_id: mod_entry.node_id.clone(),
                    region_id: mod_entry.region_id.clone(),
                    raw_attributes: extract_raw_attributes(mod_entry),
                    raw_attribute_types: extract_raw_attribute_types(mod_entry),
                    raw_children: extract_raw_children(mod_entry),
                    commented: mod_entry.commented,
                });
            }
            Some(vanilla_entry) => {
                let changes = match section {
                    Section::Progressions => diff_progression(mod_entry, vanilla_entry),
                    Section::Lists => diff_list(mod_entry, vanilla_entry),
                    Section::Races => diff_race(mod_entry, vanilla_entry),
                    Section::Feats => diff_feat(mod_entry, vanilla_entry),
                    Section::Origins => diff_origin(mod_entry, vanilla_entry),
                    Section::Backgrounds => diff_background(mod_entry, vanilla_entry),
                    Section::BackgroundGoals => diff_generic_fields(mod_entry, vanilla_entry),
                    Section::ActionResources => diff_action_resource(mod_entry, vanilla_entry),
                    Section::ActionResourceGroups => {
                        diff_action_resource_group(mod_entry, vanilla_entry)
                    }
                    Section::ClassDescriptions => diff_class_description(mod_entry, vanilla_entry),
                    // Extended sections use generic field-level diff
                    _ => diff_generic_fields(mod_entry, vanilla_entry),
                };

                if !changes.is_empty() {
                    results.push(DiffEntry {
                        uuid: effective_uuid.clone(),
                        display_name: build_display_name(mod_entry, section),
                        source_file: source_file.to_string(),
                        entry_kind: EntryKind::Modified,
                        changes,
                        node_id: mod_entry.node_id.clone(),
                        region_id: mod_entry.region_id.clone(),
                        raw_attributes: extract_raw_attributes(mod_entry),
                        raw_attribute_types: extract_raw_attribute_types(mod_entry),
                        raw_children: extract_raw_children(mod_entry),
                        commented: mod_entry.commented,
                    });
                }
            }
        }
    }

    results
}

/// Diff stats entries (for Spells section).
/// Includes both vanilla overrides (Modified) and brand-new entries (New).
pub fn diff_stats(
    mod_entries: &[crate::models::StatsEntry],
    vanilla: &HashMap<String, crate::models::StatsEntry>,
) -> Vec<DiffEntry> {
    let mut results = Vec::new();

    for mod_entry in mod_entries {
        let Some(vanilla_entry) = vanilla.get(&mod_entry.name) else {
            // New entry not in vanilla — include so the explorer tree shows it
            results.push(DiffEntry {
                uuid: mod_entry.name.clone(),
                display_name: mod_entry.name.clone(),
                source_file: String::new(),
                entry_kind: EntryKind::New,
                changes: vec![],
                node_id: mod_entry.entry_type.clone(),
                region_id: String::new(),
                raw_attributes: {
                    let mut attrs = mod_entry.data.clone();
                    if let Some(ref parent) = mod_entry.parent {
                        attrs.insert("Using".to_string(), parent.clone());
                    }
                    attrs
                },
                raw_attribute_types: HashMap::new(),
                raw_children: HashMap::new(),
                commented: false,
            });
            continue;
        };

        let mut changes = Vec::new();
        for (key, mod_val) in &mod_entry.data {
            let vanilla_val = vanilla_entry.data.get(key);
            match vanilla_val {
                Some(v) if v != mod_val => {
                    changes.push(Change {
                        change_type: ChangeType::SpellFieldChanged,
                        field: key.clone(),
                        added_values: vec![],
                        removed_values: vec![],
                        vanilla_value: Some(v.clone()),
                        mod_value: Some(mod_val.clone()),
                    });
                }
                None => {
                    changes.push(Change {
                        change_type: ChangeType::SpellFieldChanged,
                        field: key.clone(),
                        added_values: vec![],
                        removed_values: vec![],
                        vanilla_value: None,
                        mod_value: Some(mod_val.clone()),
                    });
                }
                _ => {}
            }
        }

        if !changes.is_empty() {
            results.push(DiffEntry {
                uuid: mod_entry.name.clone(),
                display_name: mod_entry.name.clone(),
                source_file: String::new(),
                entry_kind: EntryKind::Modified,
                changes,
                node_id: mod_entry.entry_type.clone(),
                region_id: String::new(),
                raw_attributes: {
                    let mut attrs = mod_entry.data.clone();
                    if let Some(ref parent) = mod_entry.parent {
                        attrs.insert("Using".to_string(), parent.clone());
                    }
                    attrs
                },
                raw_attribute_types: HashMap::new(),
                raw_children: HashMap::new(),
                commented: false,
            });
        }
    }

    results
}

fn build_display_name(entry: &LsxEntry, section: Section) -> String {
    match section {
        Section::Progressions => {
            let name = entry
                .attributes
                .get("Name")
                .map(|a| a.value.as_str())
                .unwrap_or("Unknown");
            let level = entry
                .attributes
                .get("Level")
                .map(|a| a.value.as_str())
                .unwrap_or("?");
            format!("{name} Lv{level}")
        }
        // Backgrounds and Origins use DisplayName which is a localization handle
        // Return the handle so the frontend can resolve it via localizationMap
        Section::Backgrounds | Section::Origins => entry
            .attributes
            .get("DisplayName")
            .map(|a| a.value.clone())
            .unwrap_or_else(|| {
                entry
                    .attributes
                    .get("Name")
                    .map(|a| a.value.clone())
                    .unwrap_or_else(|| "Unknown".to_string())
            }),
        // Lists: try Comment attribute, fallback to node_id (list type)
        Section::Lists => {
            entry
                .attributes
                .get("Comment")
                .map(|a| a.value.clone())
                .unwrap_or_else(|| {
                    // node_id contains the list type (SpellList, PassiveList, etc.)
                    if !entry.node_id.is_empty() {
                        entry.node_id.clone()
                    } else {
                        "Unknown List".to_string()
                    }
                })
        }
        _ => entry
            .attributes
            .get("Name")
            .map(|a| a.value.as_str())
            .unwrap_or("Unknown")
            .to_string(),
    }
}

// --- Per-section diff functions ---

fn diff_progression(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    // Diff delimited string fields
    for field in ["Boosts", "PassivesAdded", "PassivesRemoved"] {
        let mut field_changes = diff_delimited_field(mod_entry, vanilla_entry, field, ';');
        changes.append(&mut field_changes);
    }

    // Diff Selectors (semicolon-delimited)
    let mod_sel = get_attr(mod_entry, "Selectors").unwrap_or_default();
    let van_sel = get_attr(vanilla_entry, "Selectors").unwrap_or_default();
    let mod_items: HashSet<String> = split_delimited(&mod_sel, ';').into_iter().collect();
    let van_items: HashSet<String> = split_delimited(&van_sel, ';').into_iter().collect();

    let added: Vec<String> = mod_items.difference(&van_items).cloned().collect();
    let removed: Vec<String> = van_items.difference(&mod_items).cloned().collect();

    if !added.is_empty() {
        changes.push(Change {
            change_type: ChangeType::SelectorAdded,
            field: "Selectors".to_string(),
            added_values: added,
            removed_values: vec![],
            vanilla_value: if van_sel.is_empty() {
                None
            } else {
                Some(van_sel.clone())
            },
            mod_value: if mod_sel.is_empty() {
                None
            } else {
                Some(mod_sel.clone())
            },
        });
    }
    if !removed.is_empty() {
        changes.push(Change {
            change_type: ChangeType::SelectorRemoved,
            field: "Selectors".to_string(),
            added_values: vec![],
            removed_values: removed,
            vanilla_value: if van_sel.is_empty() {
                None
            } else {
                Some(van_sel.clone())
            },
            mod_value: if mod_sel.is_empty() {
                None
            } else {
                Some(mod_sel)
            },
        });
    }

    // Diff booleans
    for field in ["AllowImprovement", "IsMulticlass"] {
        if let Some(c) = diff_boolean_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    // Diff SubClasses children
    let mut child_changes = diff_children(mod_entry, vanilla_entry, "SubClass");
    changes.append(&mut child_changes);

    changes
}

fn diff_list(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    // Determine which field contains the items based on node_id
    let item_fields = ["Spells", "Passives", "Skills", "Abilities", "Equipment"];
    for field in item_fields {
        let mod_val = get_attr(mod_entry, field).unwrap_or_default();
        let van_val = get_attr(vanilla_entry, field).unwrap_or_default();

        if mod_val.is_empty() && van_val.is_empty() {
            continue;
        }

        // Lists can use ; or , depending on type
        let delim = if field == "Passives" || field == "Skills" {
            ','
        } else {
            ';'
        };

        let mod_items: HashSet<String> = split_delimited(&mod_val, delim).into_iter().collect();
        let van_items: HashSet<String> = split_delimited(&van_val, delim).into_iter().collect();

        let added: Vec<String> = mod_items.difference(&van_items).cloned().collect();
        let removed: Vec<String> = van_items.difference(&mod_items).cloned().collect();

        if !added.is_empty() {
            changes.push(Change {
                change_type: ChangeType::StringAdded,
                field: field.to_string(),
                added_values: added,
                removed_values: vec![],
                vanilla_value: if van_val.is_empty() {
                    None
                } else {
                    Some(van_val.clone())
                },
                mod_value: if mod_val.is_empty() {
                    None
                } else {
                    Some(mod_val.clone())
                },
            });
        }
        if !removed.is_empty() {
            changes.push(Change {
                change_type: ChangeType::StringRemoved,
                field: field.to_string(),
                added_values: vec![],
                removed_values: removed,
                vanilla_value: if van_val.is_empty() {
                    None
                } else {
                    Some(van_val.clone())
                },
                mod_value: if mod_val.is_empty() {
                    None
                } else {
                    Some(mod_val)
                },
            });
        }
    }

    changes
}

fn diff_race(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();
    let child_types = [
        "EyeColors",
        "SkinColors",
        "HairColors",
        "TattooColors",
        "MakeupColors",
        "LipsMakeupColors",
        "HairGrayingColors",
        "HairHighlightColors",
        "HornColors",
        "HornTipColors",
        "Gods",
        "ExcludedGods",
        "Visuals",
    ];

    for child_type in child_types {
        let mut child_changes = diff_children(mod_entry, vanilla_entry, child_type);
        changes.append(&mut child_changes);
    }

    // Also diff Tags
    let mut tag_changes = diff_children(mod_entry, vanilla_entry, "Tags");
    changes.append(&mut tag_changes);

    changes
}

fn diff_feat(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    // Delimited fields
    {
        let field = "PassivesAdded";
        let mut field_changes = diff_delimited_field(mod_entry, vanilla_entry, field, ';');
        changes.append(&mut field_changes);
    }

    // Selectors
    let mod_sel = get_attr(mod_entry, "Selectors").unwrap_or_default();
    let van_sel = get_attr(vanilla_entry, "Selectors").unwrap_or_default();
    let mod_items: HashSet<String> = split_delimited(&mod_sel, ';').into_iter().collect();
    let van_items: HashSet<String> = split_delimited(&van_sel, ';').into_iter().collect();

    let added: Vec<String> = mod_items.difference(&van_items).cloned().collect();
    let removed: Vec<String> = van_items.difference(&mod_items).cloned().collect();

    if !added.is_empty() {
        changes.push(Change {
            change_type: ChangeType::SelectorAdded,
            field: "Selectors".to_string(),
            added_values: added,
            removed_values: vec![],
            vanilla_value: None,
            mod_value: None,
        });
    }
    if !removed.is_empty() {
        changes.push(Change {
            change_type: ChangeType::SelectorRemoved,
            field: "Selectors".to_string(),
            added_values: vec![],
            removed_values: removed,
            vanilla_value: None,
            mod_value: None,
        });
    }

    // Booleans
    for field in ["AllowImprovement", "CanBeTakenMultipleTimes"] {
        if let Some(c) = diff_boolean_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    changes
}

fn diff_origin(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    // Delimited string fields
    {
        let field = "Passives";
        let mut field_changes = diff_delimited_field(mod_entry, vanilla_entry, field, ';');
        changes.append(&mut field_changes);
    }

    // Scalar fields
    for field in [
        "BodyShape",
        "BodyType",
        "VoiceTableUUID",
        "AppearanceLocked",
    ] {
        if let Some(c) = diff_scalar_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    // Children (Tags)
    for child_type in ["AppearanceTags", "ReallyTags"] {
        let mut child_changes = diff_children(mod_entry, vanilla_entry, child_type);
        changes.append(&mut child_changes);
    }

    changes
}

fn diff_background(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    {
        let field = "Passives";
        let mut field_changes = diff_delimited_field(mod_entry, vanilla_entry, field, ';');
        changes.append(&mut field_changes);
    }

    for field in ["Hidden"] {
        if let Some(c) = diff_boolean_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    let mut tag_changes = diff_children(mod_entry, vanilla_entry, "Tags");
    changes.append(&mut tag_changes);

    changes
}

fn diff_action_resource(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    for field in ["Name", "MaxLevel", "ReplenishType", "DiceType", "MaxValue"] {
        if let Some(c) = diff_scalar_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    for field in [
        "IsHidden",
        "IsSpellResource",
        "PartyActionResource",
        "ShowOnActionResourcePanel",
        "UpdatesSpellPowerLevel",
    ] {
        if let Some(c) = diff_boolean_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    changes
}

fn diff_action_resource_group(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    let mod_defs = get_attr(mod_entry, "ActionResourceDefinitions").unwrap_or_default();
    let van_defs = get_attr(vanilla_entry, "ActionResourceDefinitions").unwrap_or_default();

    let mod_items: HashSet<String> = split_delimited(&mod_defs, ';').into_iter().collect();
    let van_items: HashSet<String> = split_delimited(&van_defs, ';').into_iter().collect();

    let added: Vec<String> = mod_items.difference(&van_items).cloned().collect();
    let removed: Vec<String> = van_items.difference(&mod_items).cloned().collect();

    if !added.is_empty() {
        changes.push(Change {
            change_type: ChangeType::StringAdded,
            field: "ActionResourceDefinitions".to_string(),
            added_values: added,
            removed_values: vec![],
            vanilla_value: None,
            mod_value: None,
        });
    }
    if !removed.is_empty() {
        changes.push(Change {
            change_type: ChangeType::StringRemoved,
            field: "ActionResourceDefinitions".to_string(),
            added_values: vec![],
            removed_values: removed,
            vanilla_value: None,
            mod_value: None,
        });
    }

    changes
}

fn diff_class_description(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    // Scalar fields (int + string)
    for field in [
        "BaseHp",
        "HpPerLevel",
        "SpellCastingAbility",
        "PrimaryAbility",
        "ClassHotbarColumns",
        "CommonHotbarColumns",
        "ItemsHotbarColumns",
        "LearningStrategy",
        "MulticlassSpellcasterModifier",
        "AnimationSetPriority",
        "CharacterCreationPose",
        "ClassEquipment",
        "SoundClassType",
        "ProgressionTableUUID",
        "SpellList",
        "ParentGuid",
        "Name",
    ] {
        if let Some(c) = diff_scalar_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    // Boolean fields
    for field in [
        "CanLearnSpells",
        "MustPrepareSpells",
        "HasGod",
        "IsDefaultForUseSpellAction",
    ] {
        if let Some(c) = diff_boolean_field(mod_entry, vanilla_entry, field) {
            changes.push(c);
        }
    }

    let mut tag_changes = diff_children(mod_entry, vanilla_entry, "Tags");
    changes.append(&mut tag_changes);

    changes
}

fn diff_generic_fields(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry) -> Vec<Change> {
    let mut changes = Vec::new();

    for (key, mod_attr) in &mod_entry.attributes {
        if key == "UUID" || key == "MapKey" {
            continue;
        }
        if let Some(van_attr) = vanilla_entry.attributes.get(key) {
            if mod_attr.value != van_attr.value {
                changes.push(Change {
                    change_type: ChangeType::FieldChanged,
                    field: key.clone(),
                    added_values: vec![],
                    removed_values: vec![],
                    vanilla_value: Some(van_attr.value.clone()),
                    mod_value: Some(mod_attr.value.clone()),
                });
            }
        } else {
            changes.push(Change {
                change_type: ChangeType::FieldChanged,
                field: key.clone(),
                added_values: vec![],
                removed_values: vec![],
                vanilla_value: None,
                mod_value: Some(mod_attr.value.clone()),
            });
        }
    }

    changes
}

// --- Utility functions ---

fn get_attr(entry: &LsxEntry, key: &str) -> Option<String> {
    entry.attributes.get(key).map(|a| a.value.clone())
}

fn diff_delimited_field(
    mod_entry: &LsxEntry,
    vanilla_entry: &LsxEntry,
    field: &str,
    delimiter: char,
) -> Vec<Change> {
    let mut changes = Vec::new();
    let mod_val = get_attr(mod_entry, field).unwrap_or_default();
    let van_val = get_attr(vanilla_entry, field).unwrap_or_default();

    let mod_items: HashSet<String> = split_delimited(&mod_val, delimiter).into_iter().collect();
    let van_items: HashSet<String> = split_delimited(&van_val, delimiter).into_iter().collect();

    let added: Vec<String> = mod_items.difference(&van_items).cloned().collect();
    let removed: Vec<String> = van_items.difference(&mod_items).cloned().collect();

    if !added.is_empty() {
        changes.push(Change {
            change_type: ChangeType::StringAdded,
            field: field.to_string(),
            added_values: added,
            removed_values: vec![],
            vanilla_value: if van_val.is_empty() {
                None
            } else {
                Some(van_val.clone())
            },
            mod_value: if mod_val.is_empty() {
                None
            } else {
                Some(mod_val.clone())
            },
        });
    }
    if !removed.is_empty() {
        changes.push(Change {
            change_type: ChangeType::StringRemoved,
            field: field.to_string(),
            added_values: vec![],
            removed_values: removed,
            vanilla_value: if van_val.is_empty() {
                None
            } else {
                Some(van_val)
            },
            mod_value: if mod_val.is_empty() {
                None
            } else {
                Some(mod_val)
            },
        });
    }

    changes
}

fn diff_boolean_field(
    mod_entry: &LsxEntry,
    vanilla_entry: &LsxEntry,
    field: &str,
) -> Option<Change> {
    let mod_val = get_attr(mod_entry, field)?;
    let van_val = get_attr(vanilla_entry, field).unwrap_or_default();

    if mod_val != van_val {
        Some(Change {
            change_type: ChangeType::BooleanChanged,
            field: field.to_string(),
            added_values: vec![],
            removed_values: vec![],
            vanilla_value: if van_val.is_empty() {
                None
            } else {
                Some(van_val)
            },
            mod_value: Some(mod_val),
        })
    } else {
        None
    }
}

fn diff_scalar_field(
    mod_entry: &LsxEntry,
    vanilla_entry: &LsxEntry,
    field: &str,
) -> Option<Change> {
    let mod_val = get_attr(mod_entry, field)?;
    let van_val = get_attr(vanilla_entry, field).unwrap_or_default();

    if mod_val != van_val {
        Some(Change {
            change_type: ChangeType::FieldChanged,
            field: field.to_string(),
            added_values: vec![],
            removed_values: vec![],
            vanilla_value: if van_val.is_empty() {
                None
            } else {
                Some(van_val)
            },
            mod_value: Some(mod_val),
        })
    } else {
        None
    }
}

fn diff_children(mod_entry: &LsxEntry, vanilla_entry: &LsxEntry, child_type: &str) -> Vec<Change> {
    let mut changes = Vec::new();

    let mod_guids: HashSet<String> = mod_entry
        .children
        .iter()
        .filter(|g| g.group_id == child_type || g.entries.iter().any(|e| e.node_id == child_type))
        .flat_map(|g| g.entries.iter().map(|e| e.object_guid.clone()))
        .collect();

    let van_guids: HashSet<String> = vanilla_entry
        .children
        .iter()
        .filter(|g| g.group_id == child_type || g.entries.iter().any(|e| e.node_id == child_type))
        .flat_map(|g| g.entries.iter().map(|e| e.object_guid.clone()))
        .collect();

    let added: Vec<String> = mod_guids.difference(&van_guids).cloned().collect();
    let removed: Vec<String> = van_guids.difference(&mod_guids).cloned().collect();

    if !added.is_empty() {
        changes.push(Change {
            change_type: ChangeType::ChildAdded,
            field: child_type.to_string(),
            added_values: added,
            removed_values: vec![],
            vanilla_value: None,
            mod_value: None,
        });
    }
    if !removed.is_empty() {
        changes.push(Change {
            change_type: ChangeType::ChildRemoved,
            field: child_type.to_string(),
            added_values: vec![],
            removed_values: removed,
            vanilla_value: None,
            mod_value: None,
        });
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::LsxAttribute;

    fn make_entry(uuid: &str, attrs: Vec<(&str, &str, &str)>) -> LsxEntry {
        let mut attributes = HashMap::new();
        for (key, atype, value) in attrs {
            attributes.insert(
                key.to_string(),
                LsxAttribute {
                    attr_type: atype.to_string(),
                    value: value.to_string(),
                },
            );
        }
        LsxEntry {
            uuid: uuid.to_string(),
            node_id: "Progression".to_string(),
            attributes,
            children: vec![],
            commented: false,
            region_id: String::new(),
        }
    }

    #[test]
    fn test_diff_new_entry() {
        let mod_entries = vec![make_entry(
            "new-uuid",
            vec![("Name", "LSString", "NewClass")],
        )];
        let vanilla: HashMap<String, LsxEntry> = HashMap::new();

        let results = diff_section(Section::Progressions, &mod_entries, &vanilla, "test.lsx");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_kind, EntryKind::New);
        assert_eq!(
            results[0].changes[0].change_type,
            ChangeType::EntireEntryNew
        );
    }

    #[test]
    fn test_diff_unchanged_entry() {
        let entry = make_entry(
            "uuid-1",
            vec![
                ("Name", "LSString", "Barbarian"),
                ("Level", "uint8", "1"),
                ("Boosts", "LSString", "BoostA;BoostB"),
            ],
        );
        let mod_entries = vec![entry.clone()];
        let mut vanilla = HashMap::new();
        vanilla.insert("uuid-1".to_string(), entry);

        let results = diff_section(Section::Progressions, &mod_entries, &vanilla, "test.lsx");
        assert!(
            results.is_empty(),
            "Unchanged entries should not appear in results"
        );
    }

    #[test]
    fn test_diff_added_boost() {
        let vanilla_entry = make_entry(
            "uuid-1",
            vec![
                ("Name", "LSString", "Barbarian"),
                ("Level", "uint8", "1"),
                ("Boosts", "LSString", "BoostA;BoostB"),
            ],
        );
        let mod_entry = make_entry(
            "uuid-1",
            vec![
                ("Name", "LSString", "Barbarian"),
                ("Level", "uint8", "1"),
                ("Boosts", "LSString", "BoostA;BoostB;BoostC"),
            ],
        );

        let mod_entries = vec![mod_entry];
        let mut vanilla = HashMap::new();
        vanilla.insert("uuid-1".to_string(), vanilla_entry);

        let results = diff_section(Section::Progressions, &mod_entries, &vanilla, "test.lsx");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_kind, EntryKind::Modified);

        let boost_change = results[0]
            .changes
            .iter()
            .find(|c| c.field == "Boosts" && c.change_type == ChangeType::StringAdded)
            .expect("Should have a StringAdded change for Boosts");
        assert_eq!(boost_change.added_values, vec!["BoostC"]);
    }

    #[test]
    fn test_diff_boolean_changed() {
        let vanilla_entry = make_entry(
            "uuid-1",
            vec![
                ("Name", "LSString", "Barbarian"),
                ("Level", "uint8", "1"),
                ("AllowImprovement", "bool", "false"),
            ],
        );
        let mod_entry = make_entry(
            "uuid-1",
            vec![
                ("Name", "LSString", "Barbarian"),
                ("Level", "uint8", "1"),
                ("AllowImprovement", "bool", "true"),
            ],
        );

        let mod_entries = vec![mod_entry];
        let mut vanilla = HashMap::new();
        vanilla.insert("uuid-1".to_string(), vanilla_entry);

        let results = diff_section(Section::Progressions, &mod_entries, &vanilla, "test.lsx");
        assert_eq!(results.len(), 1);

        let bool_change = results[0]
            .changes
            .iter()
            .find(|c| c.change_type == ChangeType::BooleanChanged)
            .expect("Should have a BooleanChanged change");
        assert_eq!(bool_change.field, "AllowImprovement");
        assert_eq!(bool_change.mod_value, Some("true".to_string()));
        assert_eq!(bool_change.vanilla_value, Some("false".to_string()));
    }

    #[test]
    fn test_diff_children_added() {
        let vanilla_entry = LsxEntry {
            uuid: "uuid-1".to_string(),
            node_id: "Progression".to_string(),
            region_id: String::new(),
            attributes: {
                let mut m = HashMap::new();
                m.insert(
                    "Name".to_string(),
                    LsxAttribute {
                        attr_type: "LSString".to_string(),
                        value: "Barbarian".to_string(),
                    },
                );
                m.insert(
                    "Level".to_string(),
                    LsxAttribute {
                        attr_type: "uint8".to_string(),
                        value: "3".to_string(),
                    },
                );
                m
            },
            commented: false,
            children: vec![LsxChildGroup {
                group_id: "SubClass".to_string(),
                entries: vec![LsxChildEntry {
                    node_id: "SubClass".to_string(),
                    object_guid: "existing-subclass".to_string(),
                }],
            }],
        };

        let mod_entry = LsxEntry {
            uuid: "uuid-1".to_string(),
            node_id: "Progression".to_string(),
            attributes: vanilla_entry.attributes.clone(),
            commented: false,
            region_id: String::new(),
            children: vec![LsxChildGroup {
                group_id: "SubClass".to_string(),
                entries: vec![
                    LsxChildEntry {
                        node_id: "SubClass".to_string(),
                        object_guid: "existing-subclass".to_string(),
                    },
                    LsxChildEntry {
                        node_id: "SubClass".to_string(),
                        object_guid: "new-subclass".to_string(),
                    },
                ],
            }],
        };

        let mod_entries = vec![mod_entry];
        let mut vanilla = HashMap::new();
        vanilla.insert("uuid-1".to_string(), vanilla_entry);

        let results = diff_section(Section::Progressions, &mod_entries, &vanilla, "test.lsx");
        assert_eq!(results.len(), 1);

        let child_change = results[0]
            .changes
            .iter()
            .find(|c| c.change_type == ChangeType::ChildAdded)
            .expect("Should have a ChildAdded change");
        assert_eq!(child_change.added_values, vec!["new-subclass"]);
    }

    #[test]
    fn test_diff_stats_modified() {
        let mut vanilla = HashMap::new();
        vanilla.insert(
            "Test_Spell".to_string(),
            crate::models::StatsEntry {
                name: "Test_Spell".to_string(),
                entry_type: "SpellData".to_string(),
                parent: None,
                data: {
                    let mut d = HashMap::new();
                    d.insert("Level".to_string(), "0".to_string());
                    d.insert("Damage".to_string(), "1d6".to_string());
                    d
                },
            },
        );

        let mod_entries = vec![crate::models::StatsEntry {
            name: "Test_Spell".to_string(),
            entry_type: "SpellData".to_string(),
            parent: None,
            data: {
                let mut d = HashMap::new();
                d.insert("Level".to_string(), "0".to_string());
                d.insert("Damage".to_string(), "1d8".to_string());
                d
            },
        }];

        let results = diff_stats(&mod_entries, &vanilla);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_kind, EntryKind::Modified);
        assert_eq!(results[0].changes.len(), 1);
        assert_eq!(results[0].changes[0].field, "Damage");
    }

    #[test]
    fn test_diff_stats_new_entries_included() {
        let vanilla: HashMap<String, crate::models::StatsEntry> = HashMap::new();

        let mod_entries = vec![crate::models::StatsEntry {
            name: "MyCustomSpell".to_string(),
            entry_type: "SpellData".to_string(),
            parent: Some("Projectile_MainHandAttack".to_string()),
            data: {
                let mut d = HashMap::new();
                d.insert("SpellType".to_string(), "Projectile".to_string());
                d.insert("Damage".to_string(), "2d6".to_string());
                d
            },
        }];

        let results = diff_stats(&mod_entries, &vanilla);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_kind, EntryKind::New);
        assert_eq!(results[0].display_name, "MyCustomSpell");
        assert_eq!(results[0].node_id, "SpellData");
        assert!(results[0].changes.is_empty());
        assert_eq!(results[0].raw_attributes.get("Damage").unwrap(), "2d6");
    }

    #[test]
    fn test_diff_list_items_added() {
        let vanilla_entry = LsxEntry {
            uuid: "list-uuid".to_string(),
            node_id: "SpellList".to_string(),
            region_id: String::new(),
            attributes: {
                let mut m = HashMap::new();
                m.insert(
                    "Spells".to_string(),
                    LsxAttribute {
                        attr_type: "LSString".to_string(),
                        value: "SpellA;SpellB".to_string(),
                    },
                );
                m.insert(
                    "Name".to_string(),
                    LsxAttribute {
                        attr_type: "FixedString".to_string(),
                        value: "Test List".to_string(),
                    },
                );
                m
            },
            children: vec![],
            commented: false,
        };

        let mod_entry = LsxEntry {
            uuid: "list-uuid".to_string(),
            node_id: "SpellList".to_string(),
            region_id: String::new(),
            attributes: {
                let mut m = HashMap::new();
                m.insert(
                    "Spells".to_string(),
                    LsxAttribute {
                        attr_type: "LSString".to_string(),
                        value: "SpellA;SpellB;SpellC".to_string(),
                    },
                );
                m.insert(
                    "Name".to_string(),
                    LsxAttribute {
                        attr_type: "FixedString".to_string(),
                        value: "Test List".to_string(),
                    },
                );
                m
            },
            children: vec![],
            commented: false,
        };

        let mod_entries = vec![mod_entry];
        let mut vanilla = HashMap::new();
        vanilla.insert("list-uuid".to_string(), vanilla_entry);

        let results = diff_section(Section::Lists, &mod_entries, &vanilla, "SpellLists.lsx");
        assert_eq!(results.len(), 1);

        let spell_change = results[0]
            .changes
            .iter()
            .find(|c| c.field == "Spells" && c.change_type == ChangeType::StringAdded)
            .expect("Should have a StringAdded change for Spells");
        assert_eq!(spell_change.added_values, vec!["SpellC"]);
    }
}
