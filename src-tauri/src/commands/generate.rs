use crate::models::*;
use std::collections::HashMap;

// =========================================================================
// Section comments (YAML only)
// =========================================================================

fn section_comment(section: Section) -> String {
    let desc = match section {
        Section::Races => "Configure race entries",
        Section::Progressions => "Configure progression entries",
        Section::Lists => "Configure list entries (spell lists, passive lists, etc.)",
        Section::Feats => "Configure feat entries",
        Section::Origins => "Configure origin entries",
        Section::Backgrounds => "Configure background entries",
        Section::BackgroundGoals => "Configure background goal entries",
        Section::ActionResources => "Configure action resource entries",
        Section::ActionResourceGroups => "Configure action resource group entries",
        Section::ClassDescriptions => "Configure class description entries",
        Section::Spells => "Configure spell entries",
        _ => return String::new(), // Non-CF sections don't appear in CF config
    };
    format!(
        "# ----------------------------------------------------------\n# {}\n# ----------------------------------------------------------\n# {}\n# ----------------------------------------------------------\n",
        section.display_name(),
        desc
    )
}

// =========================================================================
// IR builders — convert Change[] to serde_json::Value (format-agnostic)
// =========================================================================

// ── Shared IR insertion helper ──

/// Insert `items` into `obj` under `key` if non-empty. Eliminates repeated
/// `if !v.is_empty() { obj.insert(...) }` boilerplate in every IR builder.
fn insert_if_nonempty(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    items: Vec<serde_json::Value>,
) {
    if !items.is_empty() {
        obj.insert(key.into(), serde_json::Value::Array(items));
    }
}

// ── Simple key/value IR builders (booleans, fields, spell fields) ──

fn add_booleans(obj: &mut serde_json::Map<String, serde_json::Value>, changes: &[Change]) {
    let items: Vec<serde_json::Value> = changes
        .iter()
        .filter(|c| c.change_type == ChangeType::BooleanChanged)
        .map(|c| {
            serde_json::json!({
                "Key": c.field,
                "Value": c.mod_value.as_deref().unwrap_or("false") == "true"
            })
        })
        .collect();
    insert_if_nonempty(obj, "Booleans", items);
}

fn add_fields(obj: &mut serde_json::Map<String, serde_json::Value>, changes: &[Change]) {
    let items: Vec<serde_json::Value> = changes
        .iter()
        .filter(|c| c.change_type == ChangeType::FieldChanged)
        .map(|c| {
            let val_str = c.mod_value.as_deref().unwrap_or("");
            let value = if let Ok(n) = val_str.parse::<i64>() {
                serde_json::Value::Number(serde_json::Number::from(n))
            } else {
                serde_json::json!(val_str)
            };
            serde_json::json!({ "Key": c.field, "Value": value })
        })
        .collect();
    insert_if_nonempty(obj, "Fields", items);
}

fn add_spell_fields(obj: &mut serde_json::Map<String, serde_json::Value>, changes: &[Change]) {
    let items: Vec<serde_json::Value> = changes
        .iter()
        .filter(|c| c.change_type == ChangeType::SpellFieldChanged)
        .map(|c| {
            serde_json::json!({
                "Key": c.field,
                "Value": c.mod_value.as_deref().unwrap_or("")
            })
        })
        .collect();
    insert_if_nonempty(obj, "Fields", items);
}

// ── Insert/Remove action IR builders ──

/// Collect insert/remove action entries from paired ChangeType variants.
fn collect_action_values(
    changes: &[Change],
    add_type: ChangeType,
    remove_type: ChangeType,
) -> Vec<(&Change, &str, &Vec<String>)> {
    changes
        .iter()
        .filter(|c| c.change_type == add_type || c.change_type == remove_type)
        .map(|c| {
            if c.change_type == add_type {
                (c, "Insert", &c.added_values)
            } else {
                (c, "Remove", &c.removed_values)
            }
        })
        .collect()
}

fn add_selectors(obj: &mut serde_json::Map<String, serde_json::Value>, changes: &[Change]) {
    let items: Vec<serde_json::Value> = collect_action_values(
        changes,
        ChangeType::SelectorAdded,
        ChangeType::SelectorRemoved,
    )
    .into_iter()
    .flat_map(|(_c, action, values)| {
        values.iter().map(move |v| {
            serde_json::json!({
                "Action": action, "Function": v, "Overwrite": false, "UUID": "", "Params": {}
            })
        })
    })
    .collect();
    insert_if_nonempty(obj, "Selectors", items);
}

fn add_strings(obj: &mut serde_json::Map<String, serde_json::Value>, changes: &[Change]) {
    let items: Vec<serde_json::Value> =
        collect_action_values(changes, ChangeType::StringAdded, ChangeType::StringRemoved)
            .into_iter()
            .map(|(c, action, values)| {
                let str_arr: Vec<serde_json::Value> = values
                    .iter()
                    .map(|v| serde_json::Value::String(v.clone()))
                    .collect();
                serde_json::json!({ "Action": action, "Type": c.field, "Strings": str_arr })
            })
            .collect();
    insert_if_nonempty(obj, "Strings", items);
}

fn add_children(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    section: Section,
    changes: &[Change],
) {
    if section != Section::Races {
        return;
    }
    let items: Vec<serde_json::Value> =
        collect_action_values(changes, ChangeType::ChildAdded, ChangeType::ChildRemoved)
            .into_iter()
            .filter(|(c, _, _)| !c.field.contains("Tags") && c.field != "SubClass")
            .flat_map(|(c, action, values)| {
                values.iter().map(
                    move |v| serde_json::json!({ "Type": c.field, "Values": v, "Action": action }),
                )
            })
            .collect();
    insert_if_nonempty(obj, "Children", items);
}

fn add_tags(obj: &mut serde_json::Map<String, serde_json::Value>, changes: &[Change]) {
    let items: Vec<serde_json::Value> =
        collect_action_values(changes, ChangeType::ChildAdded, ChangeType::ChildRemoved)
            .into_iter()
            .filter(|(c, _, _)| c.field.contains("Tags"))
            .map(|(_c, action, values)| {
                let uuids: Vec<serde_json::Value> = values
                    .iter()
                    .map(|v| serde_json::Value::String(v.clone()))
                    .collect();
                serde_json::json!({ "UUIDs": uuids, "Action": action })
            })
            .collect();
    insert_if_nonempty(obj, "Tags", items);
}

// ── Special-case IR builders ──

fn add_subclasses(obj: &mut serde_json::Map<String, serde_json::Value>, changes: &[Change]) {
    let items: Vec<serde_json::Value> = changes
        .iter()
        .filter(|c| c.field == "SubClass" && c.change_type == ChangeType::ChildAdded)
        .flat_map(|c| {
            c.added_values
                .iter()
                .map(|uuid| serde_json::json!({ "UUID": uuid, "SubClassName": "", "Class": "" }))
        })
        .collect();
    insert_if_nonempty(obj, "Subclasses", items);
}

// =========================================================================
// IR entry builders — build serde_json::Value from auto-detected changes
// =========================================================================

/// Build IR entries from auto-detected changes.
/// Returns 1 or 2 values (Lists/ARGs with mixed add/remove split into separate entries).
fn build_entry_ir(section: Section, entry: &SelectedEntry) -> Vec<serde_json::Value> {
    match section {
        Section::Lists => build_list_ir(entry),
        Section::ActionResourceGroups => build_arg_ir(entry),
        Section::Spells => {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "StatID".into(),
                serde_json::Value::String(entry.uuid.clone()),
            );
            add_spell_fields(&mut obj, &entry.changes);
            vec![serde_json::Value::Object(obj)]
        }
        _ => {
            let mut obj = serde_json::Map::new();
            obj.insert("UUID".into(), serde_json::Value::String(entry.uuid.clone()));
            add_fields(&mut obj, &entry.changes);
            add_booleans(&mut obj, &entry.changes);
            add_selectors(&mut obj, &entry.changes);
            add_strings(&mut obj, &entry.changes);
            add_children(&mut obj, section, &entry.changes);
            add_tags(&mut obj, &entry.changes);
            add_subclasses(&mut obj, &entry.changes);
            vec![serde_json::Value::Object(obj)]
        }
    }
}

/// Build IR entries for a Lists SelectedEntry.
/// May return 2 entries if there are both additions and removals.
fn build_list_ir(entry: &SelectedEntry) -> Vec<serde_json::Value> {
    let list_type = entry.list_type.as_deref().unwrap_or("SpellList");
    let is_new = entry
        .changes
        .iter()
        .any(|c| c.change_type == ChangeType::EntireEntryNew);

    let added: Vec<serde_json::Value> = entry
        .changes
        .iter()
        .filter(|c| c.change_type == ChangeType::StringAdded)
        .flat_map(|c| c.added_values.iter())
        .map(|v| serde_json::Value::String(v.clone()))
        .collect();
    let removed: Vec<serde_json::Value> = entry
        .changes
        .iter()
        .filter(|c| c.change_type == ChangeType::StringRemoved)
        .flat_map(|c| c.removed_values.iter())
        .map(|v| serde_json::Value::String(v.clone()))
        .collect();

    let mut result = Vec::new();

    if !added.is_empty() || is_new {
        let mut obj = serde_json::Map::new();
        obj.insert("Action".into(), serde_json::json!("Insert"));
        obj.insert("UUID".into(), serde_json::Value::String(entry.uuid.clone()));
        obj.insert("Type".into(), serde_json::json!(list_type));
        obj.insert("Items".into(), serde_json::Value::Array(added));
        result.push(serde_json::Value::Object(obj));
    }

    if !removed.is_empty() {
        let mut obj = serde_json::Map::new();
        obj.insert("Action".into(), serde_json::json!("Remove"));
        obj.insert("UUID".into(), serde_json::Value::String(entry.uuid.clone()));
        obj.insert("Type".into(), serde_json::json!(list_type));
        obj.insert("Items".into(), serde_json::Value::Array(removed));
        result.push(serde_json::Value::Object(obj));
    }

    result
}

/// Build IR entries for an ActionResourceGroups SelectedEntry.
/// May return 2 entries if there are both additions and removals.
fn build_arg_ir(entry: &SelectedEntry) -> Vec<serde_json::Value> {
    let added: Vec<serde_json::Value> = entry
        .changes
        .iter()
        .filter(|c| c.change_type == ChangeType::StringAdded)
        .flat_map(|c| c.added_values.iter())
        .map(|v| serde_json::Value::String(v.clone()))
        .collect();
    let removed: Vec<serde_json::Value> = entry
        .changes
        .iter()
        .filter(|c| c.change_type == ChangeType::StringRemoved)
        .flat_map(|c| c.removed_values.iter())
        .map(|v| serde_json::Value::String(v.clone()))
        .collect();

    let mut result = Vec::new();

    if !added.is_empty() {
        let mut obj = serde_json::Map::new();
        obj.insert("UUID".into(), serde_json::Value::String(entry.uuid.clone()));
        obj.insert("Definitions".into(), serde_json::Value::Array(added));
        obj.insert("Action".into(), serde_json::json!("Insert"));
        result.push(serde_json::Value::Object(obj));
    }

    if !removed.is_empty() {
        let mut obj = serde_json::Map::new();
        obj.insert("UUID".into(), serde_json::Value::String(entry.uuid.clone()));
        obj.insert("Definitions".into(), serde_json::Value::Array(removed));
        obj.insert("Action".into(), serde_json::json!("Remove"));
        result.push(serde_json::Value::Object(obj));
    }

    result
}

// =========================================================================
// Anchor detection
// =========================================================================

/// Detect anchor opportunities: groups of 3+ entries with identical changes.
pub fn detect_anchors(entries: &[SelectedEntry], threshold: usize) -> Vec<AnchorGroup> {
    let mut groups: HashMap<String, Vec<&SelectedEntry>> = HashMap::new();

    for entry in entries {
        // Serialize changes to a deterministic key (excluding UUID)
        let key = format!("{:?}|{:?}", entry.section, entry.changes);
        groups.entry(key).or_default().push(entry);
    }

    groups
        .into_iter()
        .filter(|(_, entries)| entries.len() >= threshold)
        .map(|(_, entries)| {
            let first = entries[0];
            let entry_count = entries.len();
            // Estimate: each full entry is ~6 lines, anchor reference is ~2 lines
            // Anchor definition is ~6 lines
            let full_lines = entry_count * 6;
            let anchored_lines = 6 + entry_count * 2;
            let lines_saved = full_lines.saturating_sub(anchored_lines);

            AnchorGroup {
                anchor_name: format!("shared_{}", first.section.region_id().to_lowercase()),
                shared_changes: first.changes.clone(),
                entry_uuids: entries.iter().map(|e| e.uuid.clone()).collect(),
                lines_saved,
            }
        })
        .collect()
}

// =========================================================================
// Manual entry support — builds CF config objects from structured field keys
// =========================================================================

/// Extract sorted unique numeric indices for a given prefix.
/// E.g., fields with keys "Selector:0:Function", "Selector:1:Action" → [0, 1]
fn get_field_indices(fields: &HashMap<String, String>, prefix: &str) -> Vec<usize> {
    let mut indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let prefix_colon = format!("{prefix}:");
    for key in fields.keys() {
        if let Some(rest) = key.strip_prefix(&prefix_colon) {
            if let Some(num_str) = rest.split(':').next() {
                if let Ok(n) = num_str.parse::<usize>() {
                    indices.insert(n);
                }
            }
        }
    }
    indices.into_iter().collect()
}

/// Split pipe-delimited UUIDs. Returns single UUID or array depending on _uuidIsArray flag.
fn parse_uuids(fields: &HashMap<String, String>) -> serde_json::Value {
    let raw = fields.get("UUID").map(|s| s.as_str()).unwrap_or("");
    let is_array = fields
        .get("_uuidIsArray")
        .map(|s| s == "true")
        .unwrap_or(false);
    let uuids: Vec<&str> = raw
        .split('|')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if is_array || uuids.len() > 1 {
        serde_json::json!(uuids)
    } else {
        serde_json::json!(uuids.first().copied().unwrap_or(""))
    }
}

/// Split semicolons into a trimmed, non-empty list.
fn split_semicolons(s: &str) -> Vec<String> {
    s.split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Build a CF config object from manual entry structured field keys.
/// Equivalent to TypeScript's `buildObjFromFields()`.
fn build_obj_from_fields(section: &Section, fields: &HashMap<String, String>) -> serde_json::Value {
    let mut obj = serde_json::Map::new();

    match section {
        Section::Lists => {
            obj.insert(
                "Action".into(),
                serde_json::json!(fields.get("Action").map(|s| s.as_str()).unwrap_or("Insert")),
            );
            let uuids = parse_uuids(fields);
            if uuids.is_array() {
                obj.insert("UUIDs".into(), uuids);
            } else {
                obj.insert("UUID".into(), uuids);
            }
            obj.insert(
                "Type".into(),
                serde_json::json!(fields
                    .get("Type")
                    .map(|s| s.as_str())
                    .unwrap_or("SpellList")),
            );
            if let Some(items) = fields.get("Items") {
                let arr = split_semicolons(items);
                if !arr.is_empty() {
                    obj.insert("Items".into(), serde_json::json!(arr));
                }
            }
            if let Some(inherit) = fields.get("Inherit") {
                let parts = split_semicolons(inherit);
                if parts.len() == 1 {
                    obj.insert("Inherit".into(), serde_json::json!(parts[0]));
                } else if !parts.is_empty() {
                    obj.insert("Inherit".into(), serde_json::json!(parts));
                }
            }
            if let Some(exclude) = fields.get("Exclude") {
                let parts = split_semicolons(exclude);
                if parts.len() == 1 {
                    obj.insert("Exclude".into(), serde_json::json!(parts[0]));
                } else if !parts.is_empty() {
                    obj.insert("Exclude".into(), serde_json::json!(parts));
                }
            }
            if let Some(mg) = fields.get("modGuid") {
                if !mg.is_empty() {
                    obj.insert("modGuid".into(), serde_json::json!(mg));
                }
            }
        }
        Section::Spells => {
            obj.insert(
                "ID".into(),
                serde_json::json!(fields.get("EntryName").map(|s| s.as_str()).unwrap_or("")),
            );
            obj.insert(
                "Action".into(),
                serde_json::json!(fields.get("Action").map(|s| s.as_str()).unwrap_or("Insert")),
            );
            for (k, v) in fields {
                if let Some(field_name) = k.strip_prefix("SpellField:") {
                    let arr = split_semicolons(v);
                    if !arr.is_empty() {
                        obj.insert(field_name.to_string(), serde_json::json!(arr));
                    }
                }
            }
        }
        Section::ActionResourceGroups => {
            let uuids = parse_uuids(fields);
            if uuids.is_array() {
                obj.insert("UUIDs".into(), uuids);
            } else {
                obj.insert("UUID".into(), uuids);
            }
            if let Some(defs) = fields.get("Definitions") {
                let arr = split_semicolons(defs);
                if !arr.is_empty() {
                    obj.insert("Definitions".into(), serde_json::json!(arr));
                }
            }
            obj.insert(
                "Action".into(),
                serde_json::json!(fields.get("Action").map(|s| s.as_str()).unwrap_or("Insert")),
            );
        }
        _ => {
            // Default: Progressions, Races, Feats, Origins, Backgrounds, etc.
            let uuids = parse_uuids(fields);
            if uuids.is_array() {
                obj.insert("UUIDs".into(), uuids);
            } else {
                obj.insert("UUID".into(), uuids);
            }
            if fields
                .get("Blacklist")
                .map(|s| s == "true")
                .unwrap_or(false)
            {
                obj.insert("Blacklist".into(), serde_json::json!(true));
            }

            // Booleans
            let bools: Vec<serde_json::Value> = fields
                .iter()
                .filter(|(k, _)| k.starts_with("Boolean:"))
                .filter_map(|(k, v)| {
                    k.strip_prefix("Boolean:")
                        .map(|key| serde_json::json!({ "Key": key, "Value": v == "true" }))
                })
                .collect();
            if !bools.is_empty() {
                obj.insert("Booleans".into(), serde_json::json!(bools));
            }

            // Fields
            let flds: Vec<serde_json::Value> = fields
                .iter()
                .filter(|(k, _)| k.starts_with("Field:"))
                .filter_map(|(k, v)| {
                    k.strip_prefix("Field:")
                        .map(|key| serde_json::json!({ "Key": key, "Value": v }))
                })
                .collect();
            if !flds.is_empty() {
                obj.insert("Fields".into(), serde_json::json!(flds));
            }

            // Selectors
            let sel_indices = get_field_indices(fields, "Selector");
            if !sel_indices.is_empty() {
                let selectors: Vec<serde_json::Value> = sel_indices
                    .iter()
                    .map(|i| {
                        let action = fields
                            .get(&format!("Selector:{i}:Action"))
                            .map(|s| s.as_str())
                            .unwrap_or("Insert");
                        let mut entry = serde_json::Map::new();
                        entry.insert("Action".into(), serde_json::json!(action));
                        entry.insert(
                            "Function".into(),
                            serde_json::json!(fields
                                .get(&format!("Selector:{i}:Function"))
                                .map(|s| s.as_str())
                                .unwrap_or("")),
                        );
                        let overwrite = fields
                            .get(&format!("Selector:{i}:Overwrite"))
                            .map(|s| s == "true")
                            .unwrap_or(false);
                        if overwrite {
                            entry.insert("Overwrite".into(), serde_json::json!(true));
                        }
                        if action == "Remove" {
                            if let Some(uuid) = fields.get(&format!("Selector:{i}:UUID")) {
                                if !uuid.is_empty() {
                                    entry.insert("UUID".into(), serde_json::json!(uuid));
                                }
                            }
                        } else {
                            let param_keys = [
                                "Guid",
                                "Amount",
                                "SwapAmount",
                                "SelectorId",
                                "CastingAbility",
                                "ActionResource",
                                "PrepareType",
                                "CooldownType",
                                "BonusType",
                                "Amounts",
                                "LimitToProficiency",
                            ];
                            let mut params = serde_json::Map::new();
                            for pk in &param_keys {
                                if let Some(pv) = fields.get(&format!("Selector:{i}:Param:{pk}")) {
                                    if !pv.is_empty() {
                                        if *pk == "Amounts" {
                                            let amounts: Vec<&str> = pv
                                                .split(',')
                                                .map(|s| s.trim())
                                                .filter(|s| !s.is_empty())
                                                .collect();
                                            params
                                                .insert(pk.to_string(), serde_json::json!(amounts));
                                        } else {
                                            params.insert(pk.to_string(), serde_json::json!(pv));
                                        }
                                    }
                                }
                            }
                            entry.insert("Params".into(), serde_json::Value::Object(params));
                        }
                        if let Some(mg) = fields.get(&format!("Selector:{i}:modGuid")) {
                            if !mg.is_empty() {
                                entry.insert("modGuid".into(), serde_json::json!(mg));
                            }
                        }
                        serde_json::Value::Object(entry)
                    })
                    .collect();
                obj.insert("Selectors".into(), serde_json::json!(selectors));
            }

            // Strings
            let str_indices = get_field_indices(fields, "String");
            if !str_indices.is_empty() {
                let strings: Vec<serde_json::Value> = str_indices
                    .iter()
                    .map(|i| {
                        let mut entry = serde_json::Map::new();
                        entry.insert(
                            "Action".into(),
                            serde_json::json!(fields
                                .get(&format!("String:{i}:Action"))
                                .map(|s| s.as_str())
                                .unwrap_or("Insert")),
                        );
                        entry.insert(
                            "Type".into(),
                            serde_json::json!(fields
                                .get(&format!("String:{i}:Type"))
                                .map(|s| s.as_str())
                                .unwrap_or("Boosts")),
                        );
                        let values = fields
                            .get(&format!("String:{i}:Values"))
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        entry.insert(
                            "Strings".into(),
                            serde_json::json!(split_semicolons(values)),
                        );
                        if let Some(mg) = fields.get(&format!("String:{i}:modGuid")) {
                            if !mg.is_empty() {
                                entry.insert("modGuid".into(), serde_json::json!(mg));
                            }
                        }
                        serde_json::Value::Object(entry)
                    })
                    .collect();
                obj.insert("Strings".into(), serde_json::json!(strings));
            }

            // Children (Races only)
            if *section == Section::Races {
                let child_indices = get_field_indices(fields, "Child");
                if !child_indices.is_empty() {
                    let children: Vec<serde_json::Value> = child_indices.iter().map(|i| {
                        let values_str = fields.get(&format!("Child:{i}:Values")).map(|s| s.as_str()).unwrap_or("");
                        serde_json::json!({
                            "Type": fields.get(&format!("Child:{i}:Type")).map(|s| s.as_str()).unwrap_or("EyeColors"),
                            "Values": split_semicolons(values_str),
                            "Action": fields.get(&format!("Child:{i}:Action")).map(|s| s.as_str()).unwrap_or("Insert"),
                        })
                    }).collect();
                    obj.insert("Children".into(), serde_json::json!(children));
                }
            }

            // Tags
            let tag_indices = get_field_indices(fields, "Tag");
            if !tag_indices.is_empty() {
                let tags: Vec<serde_json::Value> = tag_indices
                    .iter()
                    .map(|i| {
                        let uuids_str = fields
                            .get(&format!("Tag:{i}:UUIDs"))
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let mut entry = serde_json::Map::new();
                        entry.insert(
                            "UUIDs".into(),
                            serde_json::json!(split_semicolons(uuids_str)),
                        );
                        entry.insert(
                            "Action".into(),
                            serde_json::json!(fields
                                .get(&format!("Tag:{i}:Action"))
                                .map(|s| s.as_str())
                                .unwrap_or("Insert")),
                        );
                        entry.insert(
                            "Type".into(),
                            serde_json::json!(fields
                                .get(&format!("Tag:{i}:Type"))
                                .map(|s| s.as_str())
                                .unwrap_or("Tags")),
                        );
                        if let Some(mg) = fields.get(&format!("Tag:{i}:modGuid")) {
                            if !mg.is_empty() {
                                entry.insert("modGuid".into(), serde_json::json!(mg));
                            }
                        }
                        serde_json::Value::Object(entry)
                    })
                    .collect();
                obj.insert("Tags".into(), serde_json::json!(tags));
            }

            // Subclasses
            let sub_indices = get_field_indices(fields, "Subclass");
            if !sub_indices.is_empty() {
                let subs: Vec<serde_json::Value> = sub_indices
                    .iter()
                    .map(|i| {
                        let mut entry = serde_json::Map::new();
                        entry.insert(
                            "Action".into(),
                            serde_json::json!(fields
                                .get(&format!("Subclass:{i}:Action"))
                                .map(|s| s.as_str())
                                .unwrap_or("Remove")),
                        );
                        entry.insert(
                            "UUID".into(),
                            serde_json::json!(fields
                                .get(&format!("Subclass:{i}:UUID"))
                                .map(|s| s.as_str())
                                .unwrap_or("")),
                        );
                        if let Some(mg) = fields.get(&format!("Subclass:{i}:modGuid")) {
                            if !mg.is_empty() {
                                entry.insert("modGuid".into(), serde_json::json!(mg));
                            }
                        }
                        serde_json::Value::Object(entry)
                    })
                    .collect();
                obj.insert("Subclasses".into(), serde_json::json!(subs));
            }

            // Entry-level modGuid
            if let Some(mg) = fields.get("modGuid") {
                if !mg.is_empty() {
                    obj.insert("modGuid".into(), serde_json::json!(mg));
                }
            }
        }
    }

    serde_json::Value::Object(obj)
}

/// Split a List entry that has mixed insert+remove items into two separate field sets.
fn split_mixed_list_entry(
    fields: &HashMap<String, String>,
) -> Option<(HashMap<String, String>, HashMap<String, String>)> {
    if fields.get("_hasRemovals").map(|s| s.as_str()) != Some("true") {
        return None;
    }
    let uuid = fields.get("UUID").cloned().unwrap_or_default();
    let list_type = fields
        .get("Type")
        .cloned()
        .unwrap_or_else(|| "SpellList".to_string());
    let name = fields.get("Name").cloned();

    let mut insert = HashMap::new();
    insert.insert("UUID".into(), uuid.clone());
    insert.insert("Type".into(), list_type.clone());
    insert.insert("Action".into(), "Insert".into());
    insert.insert(
        "Items".into(),
        fields.get("Items").cloned().unwrap_or_default(),
    );
    if let Some(n) = &name {
        insert.insert("Name".into(), n.clone());
    }

    let mut remove = HashMap::new();
    remove.insert("UUID".into(), uuid);
    remove.insert("Type".into(), list_type);
    remove.insert("Action".into(), "Remove".into());
    remove.insert(
        "Items".into(),
        fields.get("_removedItems").cloned().unwrap_or_default(),
    );
    if let Some(n) = &name {
        remove.insert("Name".into(), n.clone());
    }

    Some((insert, remove))
}

// =========================================================================
// YAML rendering via serde_saphyr
// =========================================================================

/// Render a single serde_json::Value entry as a YAML list item with proper indentation.
/// Each entry becomes `  - key: val\n    key2: val2\n...`
fn render_entry_yaml(entry: &serde_json::Value) -> String {
    let yaml_str = serde_saphyr::to_string(entry).unwrap_or_default();
    // Strip the document separator `---\n` that serde_saphyr prepends
    let yaml_str = yaml_str.strip_prefix("---\n").unwrap_or(&yaml_str);
    let yaml_str = yaml_str.trim_end();

    let mut out = String::new();
    for (i, line) in yaml_str.lines().enumerate() {
        if i == 0 {
            out.push_str(&format!("  - {line}\n"));
        } else {
            out.push_str(&format!("    {line}\n"));
        }
    }
    out
}

// =========================================================================
// Unified preview IR
// =========================================================================

/// An intermediate entry for preview rendering, carrying the data and optional comment.
struct PreviewEntryIr {
    value: serde_json::Value,
    comment: Option<String>,
}

/// Build the ordered list of (section, entries) for preview, handling auto-entries,
/// manual entries, and auto-entry overrides. Shared by both YAML and JSON preview.
fn build_preview_sections(
    entries: &[SelectedEntry],
    manual_entries: &[ManualEntry],
    auto_entry_overrides: &HashMap<String, HashMap<String, String>>,
) -> Vec<(Section, Vec<PreviewEntryIr>)> {
    let mut auto_by_section: HashMap<Section, Vec<&SelectedEntry>> = HashMap::new();
    for entry in entries {
        auto_by_section
            .entry(entry.section)
            .or_default()
            .push(entry);
    }
    let mut manual_by_section: HashMap<Section, Vec<&ManualEntry>> = HashMap::new();
    for me in manual_entries {
        if let Some(section) = Section::parse_name(&me.section) {
            manual_by_section.entry(section).or_default().push(me);
        }
    }

    let mut result = Vec::new();

    for section in Section::cf_ordered() {
        let section_auto = auto_by_section.get(section);
        let section_manual = manual_by_section.get(section);
        if section_auto.is_none() && section_manual.is_none() {
            continue;
        }

        let mut ir_entries = Vec::new();

        // Auto entries
        if let Some(autos) = section_auto {
            for entry in autos {
                let override_key = format!("{}::{}", section.region_id(), entry.uuid);
                if let Some(override_fields) = auto_entry_overrides.get(&override_key) {
                    // Overridden auto entry — render from fields
                    for val in build_ir_from_fields_with_split(section, override_fields) {
                        ir_entries.push(PreviewEntryIr {
                            value: val,
                            comment: None,
                        });
                    }
                } else {
                    // Standard auto entry
                    for val in build_entry_ir(*section, entry) {
                        ir_entries.push(PreviewEntryIr {
                            value: val,
                            comment: None,
                        });
                    }
                }
            }
        }

        // Manual entries
        if let Some(manuals) = section_manual {
            for me in manuals {
                let comment = me.comment.clone();
                let entries = build_ir_from_fields_with_split(section, &me.fields);
                for (i, val) in entries.into_iter().enumerate() {
                    ir_entries.push(PreviewEntryIr {
                        value: val,
                        // Only attach comment to the first entry of a split pair
                        comment: if i == 0 { comment.clone() } else { None },
                    });
                }
            }
        }

        if !ir_entries.is_empty() {
            result.push((*section, ir_entries));
        }
    }

    result
}

/// Build IR from manual fields, handling mixed list splits.
fn build_ir_from_fields_with_split(
    section: &Section,
    fields: &HashMap<String, String>,
) -> Vec<serde_json::Value> {
    if *section == Section::Lists && fields.get("_hasRemovals").map(|s| s.as_str()) == Some("true")
    {
        if let Some((insert, remove)) = split_mixed_list_entry(fields) {
            return vec![
                build_obj_from_fields(section, &insert),
                build_obj_from_fields(section, &remove),
            ];
        }
    }
    vec![build_obj_from_fields(section, fields)]
}

// =========================================================================
// Public API
// =========================================================================

/// Generate YAML config output from selected entries.
pub fn generate_yaml(entries: &[SelectedEntry], include_comments: bool) -> String {
    let mut output = String::from("FileVersion: 1\n");

    // Group entries by section
    let mut section_map: HashMap<Section, Vec<&SelectedEntry>> = HashMap::new();
    for entry in entries {
        section_map.entry(entry.section).or_default().push(entry);
    }

    // Output in canonical order (CF-eligible sections only)
    for section in Section::cf_ordered() {
        if let Some(section_entries) = section_map.get(section) {
            output.push('\n');
            if include_comments {
                output.push_str(&section_comment(*section));
            }
            output.push_str(&format!("{}:\n", section.region_id()));

            for entry in section_entries {
                for ir_val in build_entry_ir(*section, entry) {
                    output.push_str(&render_entry_yaml(&ir_val));
                }
            }
        }
    }

    output
}

/// Generate JSON config output from selected entries.
pub fn generate_json(entries: &[SelectedEntry]) -> Result<String, String> {
    let mut root = serde_json::Map::new();
    root.insert(
        "FileVersion".to_string(),
        serde_json::Value::Number(serde_json::Number::from(1)),
    );

    // Group entries by section
    let mut section_map: HashMap<Section, Vec<&SelectedEntry>> = HashMap::new();
    for entry in entries {
        section_map.entry(entry.section).or_default().push(entry);
    }

    for section in Section::cf_ordered() {
        if let Some(section_entries) = section_map.get(section) {
            let arr: Vec<serde_json::Value> = section_entries
                .iter()
                .flat_map(|e| build_entry_ir(*section, e))
                .collect();
            root.insert(
                section.region_id().to_string(),
                serde_json::Value::Array(arr),
            );
        }
    }

    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// Generate YAML preview including manual entries and auto-entry overrides.
pub fn generate_yaml_preview(
    entries: &[SelectedEntry],
    manual_entries: &[ManualEntry],
    auto_entry_overrides: &HashMap<String, HashMap<String, String>>,
    _include_comments: bool,
    enable_section_comments: bool,
    enable_entry_comments: bool,
) -> String {
    let sections = build_preview_sections(entries, manual_entries, auto_entry_overrides);
    let mut output = String::from("FileVersion: 1\n");

    for (section, ir_entries) in &sections {
        if enable_section_comments {
            output.push_str(&format!(
                "\n# ── {} ({}) ──\n",
                section.display_name(),
                ir_entries.len()
            ));
        } else {
            output.push('\n');
        }
        output.push_str(&format!("{}:\n", section.region_id()));

        for entry_ir in ir_entries {
            if enable_entry_comments {
                if let Some(comment) = &entry_ir.comment {
                    if !comment.is_empty() {
                        output.push_str(&format!("  # {comment}\n"));
                    }
                }
            }
            output.push_str(&render_entry_yaml(&entry_ir.value));
        }
    }

    output
}

/// Generate JSON preview including manual entries and auto-entry overrides.
pub fn generate_json_preview(
    entries: &[SelectedEntry],
    manual_entries: &[ManualEntry],
    auto_entry_overrides: &HashMap<String, HashMap<String, String>>,
) -> Result<String, String> {
    let sections = build_preview_sections(entries, manual_entries, auto_entry_overrides);

    let mut root = serde_json::Map::new();
    root.insert(
        "FileVersion".into(),
        serde_json::Value::Number(serde_json::Number::from(1)),
    );

    for (section, ir_entries) in &sections {
        let arr: Vec<serde_json::Value> = ir_entries.iter().map(|e| e.value.clone()).collect();
        if !arr.is_empty() {
            root.insert(
                section.region_id().to_string(),
                serde_json::Value::Array(arr),
            );
        }
    }

    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_progression_entry(uuid: &str, changes: Vec<Change>) -> SelectedEntry {
        SelectedEntry {
            section: Section::Progressions,
            uuid: uuid.to_string(),
            display_name: "Test".to_string(),
            changes,
            manual: false,
            list_type: None,
        }
    }

    #[test]
    fn test_generate_yaml_file_version() {
        let yaml = generate_yaml(&[], true);
        assert!(yaml.contains("FileVersion: 1"));
    }

    #[test]
    fn test_generate_yaml_progression_with_selector() {
        let entries = vec![make_progression_entry(
            "test-uuid",
            vec![Change {
                change_type: ChangeType::SelectorAdded,
                field: "Selectors".to_string(),
                added_values: vec!["SelectPassives(abc-123)".to_string()],
                removed_values: vec![],
                vanilla_value: None,
                mod_value: None,
            }],
        )];

        let yaml = generate_yaml(&entries, false);
        assert!(yaml.contains("Progressions:"));
        assert!(yaml.contains("test-uuid"));
        assert!(yaml.contains("Insert"));
        assert!(yaml.contains("SelectPassives(abc-123)"));
    }

    #[test]
    fn test_generate_yaml_progression_with_boolean() {
        let entries = vec![make_progression_entry(
            "bool-uuid",
            vec![Change {
                change_type: ChangeType::BooleanChanged,
                field: "AllowImprovement".to_string(),
                added_values: vec![],
                removed_values: vec![],
                vanilla_value: Some("false".to_string()),
                mod_value: Some("true".to_string()),
            }],
        )];

        let yaml = generate_yaml(&entries, false);
        assert!(yaml.contains("Booleans:"));
        assert!(yaml.contains("AllowImprovement"));
        assert!(yaml.contains("true"));
    }

    #[test]
    fn test_generate_yaml_progression_with_strings() {
        let entries = vec![make_progression_entry(
            "str-uuid",
            vec![Change {
                change_type: ChangeType::StringAdded,
                field: "Boosts".to_string(),
                added_values: vec!["NewBoost1".to_string(), "NewBoost2".to_string()],
                removed_values: vec![],
                vanilla_value: None,
                mod_value: None,
            }],
        )];

        let yaml = generate_yaml(&entries, false);
        assert!(yaml.contains("Strings:"));
        assert!(yaml.contains("Insert"));
        assert!(yaml.contains("Boosts"));
        assert!(yaml.contains("NewBoost1"));
        assert!(yaml.contains("NewBoost2"));
    }

    #[test]
    fn test_generate_json_basic() {
        let entries = vec![make_progression_entry(
            "json-uuid",
            vec![Change {
                change_type: ChangeType::BooleanChanged,
                field: "AllowImprovement".to_string(),
                added_values: vec![],
                removed_values: vec![],
                vanilla_value: Some("false".to_string()),
                mod_value: Some("true".to_string()),
            }],
        )];

        let json = generate_json(&entries).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["FileVersion"], 1);
        assert!(parsed["Progressions"].is_array());
        assert_eq!(parsed["Progressions"][0]["UUID"], "json-uuid");
    }

    #[test]
    fn test_generate_yaml_race_children() {
        let entries = vec![SelectedEntry {
            section: Section::Races,
            uuid: "race-uuid".to_string(),
            display_name: "Human".to_string(),
            changes: vec![Change {
                change_type: ChangeType::ChildAdded,
                field: "EyeColors".to_string(),
                added_values: vec!["eye-color-uuid".to_string()],
                removed_values: vec![],
                vanilla_value: None,
                mod_value: None,
            }],
            manual: false,
            list_type: None,
        }];

        let yaml = generate_yaml(&entries, false);
        assert!(yaml.contains("Races:"));
        assert!(yaml.contains("Children:"));
        assert!(yaml.contains("EyeColors"));
        assert!(yaml.contains("eye-color-uuid"));
        assert!(yaml.contains("Insert"));
    }

    #[test]
    fn test_detect_anchors_below_threshold() {
        let entries: Vec<SelectedEntry> = (0..2)
            .map(|i| {
                make_progression_entry(
                    &format!("uuid-{i}"),
                    vec![Change {
                        change_type: ChangeType::SelectorAdded,
                        field: "Selectors".to_string(),
                        added_values: vec!["Same".to_string()],
                        removed_values: vec![],
                        vanilla_value: None,
                        mod_value: None,
                    }],
                )
            })
            .collect();

        let anchors = detect_anchors(&entries, 3);
        assert!(anchors.is_empty());
    }

    #[test]
    fn test_detect_anchors_above_threshold() {
        let entries: Vec<SelectedEntry> = (0..5)
            .map(|i| {
                make_progression_entry(
                    &format!("uuid-{i}"),
                    vec![Change {
                        change_type: ChangeType::SelectorAdded,
                        field: "Selectors".to_string(),
                        added_values: vec!["Same".to_string()],
                        removed_values: vec![],
                        vanilla_value: None,
                        mod_value: None,
                    }],
                )
            })
            .collect();

        let anchors = detect_anchors(&entries, 3);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].entry_uuids.len(), 5);
        assert!(anchors[0].lines_saved > 0);
    }

    #[test]
    fn test_generate_yaml_list_insert() {
        let entries = vec![SelectedEntry {
            section: Section::Lists,
            uuid: "list-uuid".to_string(),
            display_name: "Test List".to_string(),
            changes: vec![Change {
                change_type: ChangeType::StringAdded,
                field: "Spells".to_string(),
                added_values: vec!["NewSpell1".to_string(), "NewSpell2".to_string()],
                removed_values: vec![],
                vanilla_value: None,
                mod_value: None,
            }],
            manual: false,
            list_type: None,
        }];

        let yaml = generate_yaml(&entries, false);
        assert!(yaml.contains("Lists:"));
        assert!(yaml.contains("Insert"));
        assert!(yaml.contains("list-uuid"));
        assert!(yaml.contains("NewSpell1"));
    }

    #[test]
    fn test_generate_yaml_list_with_removals() {
        let entries = vec![SelectedEntry {
            section: Section::Lists,
            uuid: "mixed-uuid".to_string(),
            display_name: "Mixed List".to_string(),
            changes: vec![
                Change {
                    change_type: ChangeType::StringAdded,
                    field: "Spells".to_string(),
                    added_values: vec!["AddedSpell".to_string()],
                    removed_values: vec![],
                    vanilla_value: None,
                    mod_value: None,
                },
                Change {
                    change_type: ChangeType::StringRemoved,
                    field: "Spells".to_string(),
                    added_values: vec![],
                    removed_values: vec!["RemovedSpell".to_string()],
                    vanilla_value: None,
                    mod_value: None,
                },
            ],
            manual: false,
            list_type: Some("SpellList".to_string()),
        }];

        let yaml = generate_yaml(&entries, false);
        assert!(yaml.contains("Insert"), "Should have Insert entry");
        assert!(yaml.contains("Remove"), "Should have Remove entry");
        assert!(yaml.contains("AddedSpell"));
        assert!(yaml.contains("RemovedSpell"));
    }

    #[test]
    fn test_generate_json_list_with_removals() {
        let entries = vec![SelectedEntry {
            section: Section::Lists,
            uuid: "mixed-uuid".to_string(),
            display_name: "Mixed List".to_string(),
            changes: vec![
                Change {
                    change_type: ChangeType::StringAdded,
                    field: "Spells".to_string(),
                    added_values: vec!["AddedSpell".to_string()],
                    removed_values: vec![],
                    vanilla_value: None,
                    mod_value: None,
                },
                Change {
                    change_type: ChangeType::StringRemoved,
                    field: "Spells".to_string(),
                    added_values: vec![],
                    removed_values: vec!["RemovedSpell".to_string()],
                    vanilla_value: None,
                    mod_value: None,
                },
            ],
            manual: false,
            list_type: Some("SpellList".to_string()),
        }];

        let json = generate_json(&entries).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let lists = parsed["Lists"].as_array().expect("Lists should be array");
        assert_eq!(lists.len(), 2, "Should have 2 entries (Insert + Remove)");
        assert_eq!(lists[0]["Action"], "Insert");
        assert_eq!(lists[1]["Action"], "Remove");
    }

    #[test]
    fn test_build_entry_ir_default_section() {
        let entry = make_progression_entry(
            "ir-uuid",
            vec![Change {
                change_type: ChangeType::BooleanChanged,
                field: "AllowImprovement".to_string(),
                added_values: vec![],
                removed_values: vec![],
                vanilla_value: Some("false".to_string()),
                mod_value: Some("true".to_string()),
            }],
        );

        let ir = build_entry_ir(Section::Progressions, &entry);
        assert_eq!(ir.len(), 1);
        assert_eq!(ir[0]["UUID"], "ir-uuid");
        assert!(ir[0]["Booleans"].is_array());
    }

    #[test]
    fn test_render_entry_yaml_basic() {
        let entry = serde_json::json!({
            "UUID": "test-uuid",
            "Booleans": [{ "Key": "AllowImprovement", "Value": true }]
        });
        let yaml = render_entry_yaml(&entry);
        assert!(
            yaml.starts_with("  - "),
            "Should start with list item marker"
        );
        assert!(yaml.contains("UUID"));
        assert!(yaml.contains("test-uuid"));
        assert!(yaml.contains("Booleans"));
        assert!(yaml.contains("AllowImprovement"));
    }

    #[test]
    fn test_generate_yaml_field_numeric_unquoted() {
        let entries = vec![make_progression_entry(
            "num-uuid",
            vec![Change {
                change_type: ChangeType::FieldChanged,
                field: "Level".to_string(),
                added_values: vec![],
                removed_values: vec![],
                vanilla_value: None,
                mod_value: Some("5".to_string()),
            }],
        )];

        let yaml = generate_yaml(&entries, false);
        // Numeric values should appear as numbers (not quoted strings)
        assert!(yaml.contains("5"));
    }

    #[test]
    fn test_build_preview_sections_unifies_auto_and_manual() {
        let auto = vec![make_progression_entry(
            "auto-uuid",
            vec![Change {
                change_type: ChangeType::BooleanChanged,
                field: "AllowImprovement".to_string(),
                added_values: vec![],
                removed_values: vec![],
                vanilla_value: Some("false".to_string()),
                mod_value: Some("true".to_string()),
            }],
        )];
        let manual = vec![ManualEntry {
            section: "Progressions".to_string(),
            fields: {
                let mut f = HashMap::new();
                f.insert("UUID".into(), "manual-uuid".into());
                f.insert("Boolean:AllowImprovement".into(), "true".into());
                f
            },
            comment: Some("Test comment".to_string()),
        }];

        let sections = build_preview_sections(&auto, &manual, &HashMap::new());
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, Section::Progressions);
        assert_eq!(sections[0].1.len(), 2, "Should have auto + manual entries");
        assert!(
            sections[0].1[1].comment.is_some(),
            "Manual entry should have comment"
        );
    }
}
