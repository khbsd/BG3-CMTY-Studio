use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::pak::reader::{bytes_read_u16, bytes_read_u32};

const LOCA_SIGNATURE: u32 = 0x4143_4f4c;
const HEADER_SIZE: u64 = 12;
const ENTRY_SIZE: u64 = 70;
const KEY_SIZE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaEntry {
    pub key: String,
    pub version: u16,
    pub text: String,
}

pub fn parse_loca_file(path: &Path) -> Result<Vec<LocaEntry>, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;
    parse_loca(file)
}

pub fn parse_loca<R: Read + Seek>(mut reader: R) -> Result<Vec<LocaEntry>, String> {
    let file_len = reader
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("Failed to inspect loca stream: {e}"))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("Failed to rewind loca stream: {e}"))?;

    if file_len < HEADER_SIZE {
        return Err("Loca file too small to contain a header".into());
    }

    let signature = bytes_read_u32(&mut reader)?;
    if signature != LOCA_SIGNATURE {
        return Err(format!(
            "Incorrect signature in localization file: expected {LOCA_SIGNATURE:08X}, got {signature:08X}"
        ));
    }

    let num_entries = bytes_read_u32(&mut reader)? as usize;
    let texts_offset = bytes_read_u32(&mut reader)? as u64;
    let table_end = HEADER_SIZE + ENTRY_SIZE * num_entries as u64;

    if texts_offset < table_end {
        return Err(format!(
            "Invalid loca text offset {texts_offset} before entry table end {table_end}"
        ));
    }
    if texts_offset > file_len {
        return Err(format!(
            "Invalid loca text offset {texts_offset} beyond file size {file_len}"
        ));
    }
    if table_end > file_len {
        return Err(format!(
            "Invalid loca entry table end {table_end} beyond file size {file_len}"
        ));
    }

    let mut raw_entries = Vec::with_capacity(num_entries);
    for _ in 0..num_entries {
        let mut key_bytes = [0u8; KEY_SIZE];
        reader
            .read_exact(&mut key_bytes)
            .map_err(|e| format!("Failed to read loca entry key: {e}"))?;
        let version = bytes_read_u16(&mut reader)?;
        let length = bytes_read_u32(&mut reader)? as usize;
        raw_entries.push((decode_key(&key_bytes)?, version, length));
    }

    reader
        .seek(SeekFrom::Start(texts_offset))
        .map_err(|e| format!("Failed to seek to loca text section: {e}"))?;

    let mut entries = Vec::with_capacity(num_entries);
    for (key, version, length) in raw_entries {
        let text = read_text(&mut reader, length)?;
        entries.push(LocaEntry { key, version, text });
    }

    if entries.len() != num_entries {
        return Err(format!(
            "LOCA entry count mismatch: header declared {} entries but parsed {} (possible truncation)",
            num_entries, entries.len()
        ));
    }

    Ok(entries)
}

fn decode_key(bytes: &[u8; KEY_SIZE]) -> Result<String, String> {
    let key_len = bytes.iter().position(|b| *b == 0).unwrap_or(KEY_SIZE);
    std::str::from_utf8(&bytes[..key_len])
        .map(|s| s.to_string())
        .map_err(|e| format!("Invalid UTF-8 in loca key: {e}"))
}

fn read_text<R: Read>(reader: &mut R, length: usize) -> Result<String, String> {
    if length == 0 {
        return Ok(String::new());
    }

    let mut buffer = vec![0u8; length];
    reader
        .read_exact(&mut buffer)
        .map_err(|e| format!("Failed to read loca text: {e}"))?;

    if buffer.last() == Some(&0) {
        buffer.pop();
    }

    String::from_utf8(buffer).map_err(|e| format!("Invalid UTF-8 in loca text: {e}"))
}

// ── Binary .loca writer ─────────────────────────────────────────────────────

/// Write localization entries to binary .loca format.
///
/// Format:
///   Header: signature (u32) | num_entries (u32) | texts_offset (u32)
///   Entry table: key (64 bytes, null-padded) | version (u16) | text_length (u32)
///   Text section: concatenated null-terminated UTF-8 strings
pub fn write_loca(entries: &[LocaEntry]) -> Result<Vec<u8>, String> {
    let num_entries = entries.len() as u32;
    let texts_offset = HEADER_SIZE as u32 + ENTRY_SIZE as u32 * num_entries;

    // Pre-compute text bytes (each null-terminated)
    let text_bytes: Vec<Vec<u8>> = entries
        .iter()
        .map(|e| {
            let mut bytes = e.text.as_bytes().to_vec();
            bytes.push(0); // null terminator
            bytes
        })
        .collect();

    let total_text_len: usize = text_bytes.iter().map(|b| b.len()).sum();
    let total_size = texts_offset as usize + total_text_len;
    let mut buf = Vec::with_capacity(total_size);

    // Header
    buf.extend_from_slice(&LOCA_SIGNATURE.to_le_bytes());
    buf.extend_from_slice(&num_entries.to_le_bytes());
    buf.extend_from_slice(&texts_offset.to_le_bytes());

    // Entry table
    for (entry, text_b) in entries.iter().zip(&text_bytes) {
        let mut key_bytes = [0u8; KEY_SIZE];
        let key_src = entry.key.as_bytes();
        let copy_len = key_src.len().min(KEY_SIZE);
        key_bytes[..copy_len].copy_from_slice(&key_src[..copy_len]);
        buf.extend_from_slice(&key_bytes);
        buf.extend_from_slice(&entry.version.to_le_bytes());
        buf.extend_from_slice(&(text_b.len() as u32).to_le_bytes());
    }

    // Text section
    for text_b in &text_bytes {
        buf.extend_from_slice(text_b);
    }

    Ok(buf)
}

// ── .loca.xml parser ────────────────────────────────────────────────────────

/// Parse a `.loca.xml` file (Larian localization XML format) into LocaEntry items.
///
/// Expected format:
/// ```xml
/// <contentList>
///   <content contentuid="handle" version="1">Text here</content>
/// </contentList>
/// ```
pub fn parse_loca_xml(xml: &str) -> Result<Vec<LocaEntry>, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut entries = Vec::new();
    let mut current_key: Option<String> = None;
    let mut current_version: u16 = 1;
    let mut current_text = String::new();
    let mut in_content = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) if e.name().as_ref() == b"content" => {
                current_key = None;
                current_version = 1;
                current_text.clear();
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"contentuid" => {
                            current_key = Some(String::from_utf8_lossy(&attr.value).to_string());
                        }
                        b"version" => {
                            if let Ok(v) = String::from_utf8_lossy(&attr.value).parse::<u16>() {
                                current_version = v;
                            }
                        }
                        _ => {}
                    }
                }
                in_content = true;
            }
            Ok(Event::Text(ref e)) if in_content => {
                if let Ok(text) = e.decode() {
                    current_text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(ref e)) if in_content => {
                // Resolve XML entity references (e.g. &lt; &gt; &amp; &apos; &quot;)
                // and character references (e.g. &#60; &#x3C;) back into characters.
                let name = e
                    .decode()
                    .map_err(|err| format!("Encoding error in entity ref: {err}"))?;
                match name.as_ref() {
                    "lt" => current_text.push('<'),
                    "gt" => current_text.push('>'),
                    "amp" => current_text.push('&'),
                    "apos" => current_text.push('\''),
                    "quot" => current_text.push('"'),
                    _ => {
                        if let Ok(Some(ch)) = e.resolve_char_ref() {
                            current_text.push(ch);
                        } else {
                            // Unknown entity — preserve as-is
                            current_text.push('&');
                            current_text.push_str(&name);
                            current_text.push(';');
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) if e.name().as_ref() == b"content" && in_content => {
                if let Some(key) = current_key.take() {
                    // Normalise Windows line endings — BG3 loca uses \n only
                    let text = std::mem::take(&mut current_text).replace('\r', "");
                    entries.push(LocaEntry {
                        key,
                        version: current_version,
                        text,
                    });
                }
                in_content = false;
            }
            Ok(Event::Empty(ref e)) if e.name().as_ref() == b"content" => {
                // Self-closing <content contentuid="..." version="1" />
                let mut key = None;
                let mut version: u16 = 1;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"contentuid" => {
                            key = Some(String::from_utf8_lossy(&attr.value).to_string());
                        }
                        b"version" => {
                            if let Ok(v) = String::from_utf8_lossy(&attr.value).parse::<u16>() {
                                version = v;
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(k) = key {
                    entries.push(LocaEntry {
                        key: k,
                        version,
                        text: String::new(),
                    });
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error in .loca.xml: {e}")),
            _ => {}
        }
    }

    Ok(entries)
}

/// Read a `.loca.xml` file from disk and convert it to binary `.loca` bytes.
pub fn loca_xml_to_binary(disk_path: &Path) -> Result<Vec<u8>, String> {
    let xml = std::fs::read_to_string(disk_path)
        .map_err(|e| format!("Failed to read {}: {e}", disk_path.display()))?;
    let entries = parse_loca_xml(&xml)?;
    write_loca(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pak::PakReader;
    use std::io::Cursor;
    use std::path::PathBuf;

    #[test]
    fn parses_basic_loca_resource() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&LOCA_SIGNATURE.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&(HEADER_SIZE as u32 + ENTRY_SIZE as u32 * 2).to_le_bytes());

        push_entry(&mut bytes, "h1", 1, 6);
        push_entry(&mut bytes, "h2", 7, 1);
        bytes.extend_from_slice(b"Hello\0");
        bytes.extend_from_slice(b"\0");

        let entries = parse_loca(Cursor::new(bytes)).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            LocaEntry {
                key: "h1".into(),
                version: 1,
                text: "Hello".into()
            }
        );
        assert_eq!(
            entries[1],
            LocaEntry {
                key: "h2".into(),
                version: 7,
                text: String::new()
            }
        );
    }

    #[test]
    fn rejects_bad_signature() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());

        let error = parse_loca(Cursor::new(bytes)).unwrap_err();
        assert!(error.contains("Incorrect signature"));
    }

    #[test]
    fn rejects_invalid_text_offset() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&LOCA_SIGNATURE.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&HEADER_SIZE.to_le_bytes());
        push_entry(&mut bytes, "h1", 1, 1);
        bytes.extend_from_slice(b"\0");

        let error = parse_loca(Cursor::new(bytes)).unwrap_err();
        assert!(error.contains("Invalid loca text offset"));
    }

    #[test]
    #[ignore = "requires workspace-root English.pak copied into the repository"]
    fn streams_real_workspace_loca_entries() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let pak_path = root.join("English.pak");
        assert!(pak_path.exists(), "missing {}", pak_path.display());

        let reader = PakReader::open(&pak_path).unwrap();
        let mut chosen_path = None;
        let mut chosen_entries = None;

        for entry in reader.entries() {
            if entry.is_deleted()
                || !entry
                    .path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("loca"))
            {
                continue;
            }

            let mut pak_entry_reader = reader.open_entry(entry).unwrap();
            let bytes = pak_entry_reader
                .read_to_end_with_limit(32 * 1024 * 1024)
                .unwrap();
            let entries = parse_loca(Cursor::new(bytes)).unwrap();
            if entries.is_empty() {
                continue;
            }

            chosen_path = Some(entry.path.as_str().to_string());
            chosen_entries = Some(entries);
            break;
        }

        let entry_path =
            chosen_path.expect("expected at least one non-empty .loca entry in English.pak");
        let entries = chosen_entries.unwrap();

        println!(
            "LOCA_SAMPLE entry_path={} total_entries={}",
            entry_path,
            entries.len()
        );
        for (index, entry) in entries.iter().take(12).enumerate() {
            println!(
                "LOCA_ROW {:02} key={} version={} text={}",
                index + 1,
                entry.key,
                entry.version,
                preview_text(&entry.text)
            );
        }

        assert!(!entries.is_empty(), "expected decoded localization entries");
    }

    fn push_entry(bytes: &mut Vec<u8>, key: &str, version: u16, length: u32) {
        let mut key_bytes = [0u8; KEY_SIZE];
        key_bytes[..key.len()].copy_from_slice(key.as_bytes());
        bytes.extend_from_slice(&key_bytes);
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&length.to_le_bytes());
    }

    fn preview_text(text: &str) -> String {
        const MAX_CHARS: usize = 96;

        let mut preview = text
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>();
        preview = preview.split_whitespace().collect::<Vec<_>>().join(" ");

        let char_count = preview.chars().count();
        if char_count > MAX_CHARS {
            preview.chars().take(MAX_CHARS).collect::<String>() + "..."
        } else {
            preview
        }
    }

    #[test]
    fn write_loca_roundtrips() {
        let entries = vec![
            LocaEntry {
                key: "h1234".into(),
                version: 1,
                text: "Hello World".into(),
            },
            LocaEntry {
                key: "h5678".into(),
                version: 3,
                text: "".into(),
            },
        ];
        let bytes = write_loca(&entries).unwrap();
        let parsed = parse_loca(Cursor::new(bytes)).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].key, "h1234");
        assert_eq!(parsed[0].version, 1);
        assert_eq!(parsed[0].text, "Hello World");
        assert_eq!(parsed[1].key, "h5678");
        assert_eq!(parsed[1].version, 3);
        assert_eq!(parsed[1].text, "");
    }

    #[test]
    fn parse_loca_xml_basic() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<contentList>
  <content contentuid="hABC123" version="1">DeathKnight</content>
  <content contentuid="hDEF456" version="2">Some text here</content>
</contentList>"#;
        let entries = parse_loca_xml(xml).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "hABC123");
        assert_eq!(entries[0].version, 1);
        assert_eq!(entries[0].text, "DeathKnight");
        assert_eq!(entries[1].key, "hDEF456");
        assert_eq!(entries[1].version, 2);
        assert_eq!(entries[1].text, "Some text here");
    }

    #[test]
    fn parse_loca_xml_empty_content() {
        let xml = r#"<contentList>
  <content contentuid="h000" version="1" />
</contentList>"#;
        let entries = parse_loca_xml(xml).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "");
    }

    #[test]
    fn parse_loca_xml_entity_references() {
        let xml = r#"<contentList>
  <content contentuid="h001" version="1">Use proficiency actions.&lt;br&gt;Equal to &lt;LSTag Tooltip="ProficiencyBonus"&gt;Proficiency Bonus&lt;/LSTag&gt;.</content>
  <content contentuid="h002" version="1">A &amp; B &apos;quoted&apos; &quot;text&quot;</content>
</contentList>"#;
        let entries = parse_loca_xml(xml).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].text,
            r#"Use proficiency actions.<br>Equal to <LSTag Tooltip="ProficiencyBonus">Proficiency Bonus</LSTag>."#
        );
        assert_eq!(entries[1].text, "A & B 'quoted' \"text\"");
    }

    #[test]
    fn loca_xml_entity_roundtrip() {
        let xml = r#"<contentList>
  <content contentuid="hEntTest" version="1">Text with &lt;br&gt; and &amp; entities</content>
</contentList>"#;
        let entries = parse_loca_xml(xml).unwrap();
        assert_eq!(entries[0].text, "Text with <br> and & entities");
        let binary = write_loca(&entries).unwrap();
        let parsed = parse_loca(Cursor::new(binary)).unwrap();
        assert_eq!(parsed[0].text, "Text with <br> and & entities");
    }

    #[test]
    fn loca_xml_to_binary_roundtrip() {
        let xml = r#"<contentList>
  <content contentuid="hTestKey001" version="1">Hello</content>
</contentList>"#;
        let entries = parse_loca_xml(xml).unwrap();
        let binary = write_loca(&entries).unwrap();
        let parsed = parse_loca(Cursor::new(binary)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].key, "hTestKey001");
        assert_eq!(parsed[0].text, "Hello");
    }
}
