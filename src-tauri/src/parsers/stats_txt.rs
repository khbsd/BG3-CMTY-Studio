use crate::models::StatsEntry;
use regex::Regex;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::LazyLock;

static RE_NEW_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^new entry "(.+)""#).expect("RE_NEW_ENTRY regex"));
static RE_NEW_EQUIPMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^new equipment "(.+)""#).expect("RE_NEW_EQUIPMENT regex"));
static RE_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^type "(.+)""#).expect("RE_TYPE regex"));
static RE_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^data "(.+)" "(.*)""#).expect("RE_DATA regex"));
static RE_USING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^using "(.+)""#).expect("RE_USING regex"));

/// Parse a Stats .txt file (Spell_*.txt, Status_*.txt) into entries.
pub fn parse_stats_file(content: &str) -> Vec<StatsEntry> {
    parse_stats_file_typed(content, None)
}

/// Parse a Stats .txt file with an optional type hint (file stem) for
/// non-standard formats (BloodTypes, ItemColor, XPData, etc.).
pub fn parse_stats_file_typed(content: &str, file_stem: Option<&str>) -> Vec<StatsEntry> {
    let mut entries = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_type = String::new();
    let mut current_parent: Option<String> = None;
    let mut current_data: HashMap<String, String> = HashMap::new();
    let mut has_standard_entries = false;

    for line in content.lines() {
        let line = line.trim();

        if line.is_empty() {
            // End of entry block
            if let Some(name) = current_name.take() {
                entries.push(StatsEntry {
                    name,
                    entry_type: std::mem::take(&mut current_type),
                    parent: current_parent.take(),
                    data: std::mem::take(&mut current_data),
                });
            }
            continue;
        }

        if let Some(caps) = RE_NEW_ENTRY.captures(line) {
            // Flush any pending entry
            if let Some(name) = current_name.take() {
                entries.push(StatsEntry {
                    name,
                    entry_type: std::mem::take(&mut current_type),
                    parent: current_parent.take(),
                    data: std::mem::take(&mut current_data),
                });
            }
            current_name = Some(caps[1].to_string());
            has_standard_entries = true;
        } else if let Some(caps) = RE_TYPE.captures(line) {
            current_type = caps[1].to_string();
        } else if let Some(caps) = RE_DATA.captures(line) {
            current_data.insert(caps[1].to_string(), caps[2].to_string());
        } else if let Some(caps) = RE_USING.captures(line) {
            current_parent = Some(caps[1].to_string());
        }
    }

    // Flush last entry
    if let Some(name) = current_name {
        entries.push(StatsEntry {
            name,
            entry_type: current_type,
            parent: current_parent,
            data: current_data,
        });
    }

    // If nothing was found with the standard parser and we have a type hint,
    // try non-standard formats.
    if entries.is_empty() && !has_standard_entries {
        if let Some(stem) = file_stem {
            return parse_nonstandard_stats(content, stem);
        }
    }

    entries
}

/// Parse non-standard stats formats: CSV entries, key-value, groups.
fn parse_nonstandard_stats(content: &str, file_stem: &str) -> Vec<StatsEntry> {
    let mut entries = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_data: HashMap<String, String> = HashMap::new();

    for line in content.lines() {
        let t = line.trim();

        if t.is_empty() || t.starts_with("//") {
            if let Some(name) = current_name.take() {
                entries.push(StatsEntry {
                    name,
                    entry_type: file_stem.to_string(),
                    parent: None,
                    data: std::mem::take(&mut current_data),
                });
            }
            continue;
        }

        // key "Name","Value" (XPData)
        if t.starts_with("key ") || t.starts_with("key\t") {
            if let Some(name) = current_name.take() {
                entries.push(StatsEntry {
                    name,
                    entry_type: file_stem.to_string(),
                    parent: None,
                    data: std::mem::take(&mut current_data),
                });
            }
            if let Some(q) = t.find('"') {
                let values = parse_csv_values(&t[q..]);
                if let Some(name) = values.first() {
                    let mut data = HashMap::new();
                    if let Some(val) = values.get(1) {
                        data.insert("Value".to_string(), val.clone());
                    }
                    entries.push(StatsEntry {
                        name: name.clone(),
                        entry_type: file_stem.to_string(),
                        parent: None,
                        data,
                    });
                }
            }
            continue;
        }

        // new <keyword> "name","v1","v2",...
        if t.starts_with("new ") {
            if let Some(name) = current_name.take() {
                entries.push(StatsEntry {
                    name,
                    entry_type: file_stem.to_string(),
                    parent: None,
                    data: std::mem::take(&mut current_data),
                });
            }
            if let Some(q) = t.find('"') {
                let values = parse_csv_values(&t[q..]);
                current_name = values.first().cloned();
                current_data.clear();
                for (i, val) in values.iter().enumerate().skip(1) {
                    current_data.insert(format!("Param{i}"), val.clone());
                }
            }
            continue;
        }

        // add <keyword> <values> (group sub-entries)
        if let Some(rest) = t.strip_prefix("add ") {
            if let Some(kw_end) = rest.find(|c: char| c.is_whitespace() || c == '"') {
                let keyword = &rest[..kw_end];
                let remainder = rest[kw_end..].trim();
                let values = parse_csv_values(remainder);
                let val_str = values.join(";");

                let col = capitalize_first(keyword);
                current_data
                    .entry(col)
                    .and_modify(|existing| {
                        existing.push('|');
                        existing.push_str(&val_str);
                    })
                    .or_insert(val_str);
            }
        }
    }

    // Flush last entry
    if let Some(name) = current_name {
        entries.push(StatsEntry {
            name,
            entry_type: file_stem.to_string(),
            parent: None,
            data: current_data,
        });
    }

    entries
}

/// Parse comma-separated values (quoted and unquoted).
fn parse_csv_values(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        if bytes[i] == b'"' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            values.push(input[start..i].to_string());
            if i < bytes.len() {
                i += 1;
            }
            while i < bytes.len() && (bytes[i] == b',' || bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
        } else if bytes[i] == b',' {
            values.push(String::new());
            i += 1;
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            values.push(input[start..i].trim().to_string());
            if i < bytes.len() {
                i += 1;
            }
        }
    }
    values
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out = c.to_uppercase().collect::<String>();
            out.push_str(chars.as_str());
            out
        }
    }
}

/// Parse an Equipment.txt file and return just the equipment set names.
/// Format: `new equipment "EQP_CC_Barbarian"`
pub fn parse_equipment_file(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            RE_NEW_EQUIPMENT
                .captures(line)
                .map(|caps| caps[1].to_string())
        })
        .collect()
}

/// Serialize Stats entries back to BG3 .txt format.
pub fn write_stats_file(entries: &[StatsEntry]) -> String {
    let mut out = String::new();
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = writeln!(out, "new entry \"{}\"", entry.name);
        let _ = writeln!(out, "type \"{}\"", entry.entry_type);
        if let Some(ref parent) = entry.parent {
            let _ = writeln!(out, "using \"{parent}\"");
        }
        // Sort data keys for deterministic output
        let mut keys: Vec<&String> = entry.data.keys().collect();
        keys.sort();
        for key in keys {
            let _ = writeln!(out, "data \"{}\" \"{}\"", key, entry.data[key]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATS_CONTENT: &str = r#"new entry "Shout_Birthsign_Fire"
type "SpellData"
data "SpellType" "Shout"
data "Cooldown" "OncePerTurn"
data "SpellProperties" "GROUND:SummonInInventory(OBJ_Birthsign_Fire,1,0,,,)"
using "Shout_Birthsign_Base"

new entry "Target_SacredFlame"
type "SpellData"
data "SpellType" "Target"
data "Level" "0"
data "Damage" "1d8"
"#;

    #[test]
    fn test_parse_stats_file() {
        let entries = parse_stats_file(STATS_CONTENT);
        assert_eq!(entries.len(), 2);

        let first = &entries[0];
        assert_eq!(first.name, "Shout_Birthsign_Fire");
        assert_eq!(first.entry_type, "SpellData");
        assert_eq!(first.parent, Some("Shout_Birthsign_Base".to_string()));
        assert_eq!(first.data.get("SpellType").unwrap(), "Shout");
        assert_eq!(first.data.get("Cooldown").unwrap(), "OncePerTurn");

        let second = &entries[1];
        assert_eq!(second.name, "Target_SacredFlame");
        assert_eq!(second.entry_type, "SpellData");
        assert_eq!(second.parent, None);
        assert_eq!(second.data.get("Level").unwrap(), "0");
        assert_eq!(second.data.get("Damage").unwrap(), "1d8");
    }

    #[test]
    fn test_parse_empty_stats() {
        let entries = parse_stats_file("");
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_single_entry_no_trailing_newline() {
        let content = r#"new entry "Test_Spell"
type "SpellData"
data "SpellType" "Shout""#;
        let entries = parse_stats_file(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Test_Spell");
        assert_eq!(entries[0].data.get("SpellType").unwrap(), "Shout");
    }

    #[test]
    fn test_parse_entry_with_using() {
        let content = r#"new entry "Child_Spell"
type "SpellData"
data "SpellType" "Target"
using "Parent_Spell"
"#;
        let entries = parse_stats_file(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].parent, Some("Parent_Spell".to_string()));
    }

    #[test]
    fn test_write_stats_file() {
        let entries = vec![
            StatsEntry {
                name: "Shout_Test".into(),
                entry_type: "SpellData".into(),
                parent: Some("Shout_Base".into()),
                data: HashMap::from([
                    ("SpellType".into(), "Shout".into()),
                    ("Cooldown".into(), "OncePerTurn".into()),
                ]),
            },
            StatsEntry {
                name: "Target_Test".into(),
                entry_type: "SpellData".into(),
                parent: None,
                data: HashMap::from([("Level".into(), "0".into())]),
            },
        ];
        let output = write_stats_file(&entries);
        assert!(output.contains(r#"new entry "Shout_Test""#));
        assert!(output.contains(r#"type "SpellData""#));
        assert!(output.contains(r#"using "Shout_Base""#));
        assert!(output.contains(r#"data "SpellType" "Shout""#));
        assert!(output.contains(r#"new entry "Target_Test""#));
        assert!(output.contains(r#"data "Level" "0""#));
        // Second entry has no parent
        let second_block = output.split("new entry \"Target_Test\"").nth(1).unwrap();
        assert!(!second_block.contains("using"));
    }

    #[test]
    fn test_write_then_parse_roundtrip() {
        let entries = vec![StatsEntry {
            name: "Roundtrip_Spell".into(),
            entry_type: "SpellData".into(),
            parent: Some("Parent_Spell".into()),
            data: HashMap::from([
                ("SpellType".into(), "Target".into()),
                ("Damage".into(), "1d8".into()),
            ]),
        }];
        let output = write_stats_file(&entries);
        let parsed = parse_stats_file(&output);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "Roundtrip_Spell");
        assert_eq!(parsed[0].entry_type, "SpellData");
        assert_eq!(parsed[0].parent, Some("Parent_Spell".into()));
        assert_eq!(parsed[0].data.get("Damage").unwrap(), "1d8");
        assert_eq!(parsed[0].data.get("SpellType").unwrap(), "Target");
    }

    #[test]
    fn test_all_regexes_compile() {
        // Force evaluation of every LazyLock<Regex> static.
        // If any pattern is invalid, this test panics at the .expect() call,
        // catching the issue in CI rather than at runtime.
        let _ = RE_NEW_ENTRY.is_match("");
        let _ = RE_NEW_EQUIPMENT.is_match("");
        let _ = RE_TYPE.is_match("");
        let _ = RE_DATA.is_match("");
        let _ = RE_USING.is_match("");
    }
}
