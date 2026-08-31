use std::io::Read;

use bg3_cmty_studio_lib::pak::format::{parse_package, PakCompression};
use bg3_cmty_studio_lib::pak::path::PakPath;
use bg3_cmty_studio_lib::pak::reader::PakReader;
use bg3_cmty_studio_lib::pak::writer::{PakWriter, PakWriterOptions};

fn read_entry_bytes(reader: &PakReader, path: &str) -> Vec<u8> {
    let pak_path = PakPath::parse(path).unwrap();
    let entry = reader.find(&pak_path).unwrap_or_else(|| {
        panic!("entry not found: {path}");
    });
    let mut data = Vec::new();
    reader
        .open_entry(entry)
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    data
}

#[test]
fn roundtrip_single_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions::default();
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();
    writer
        .add_bytes(&PakPath::parse("test/hello.txt").unwrap(), b"Hello, world!")
        .unwrap();
    let result = writer.finish().unwrap();
    assert_eq!(result.file_count, 1);

    let reader = PakReader::open(tmp.path()).unwrap();
    assert_eq!(reader.entries().count(), 1);
    let data = read_entry_bytes(&reader, "test/hello.txt");
    assert_eq!(data, b"Hello, world!");
}

#[test]
fn roundtrip_multi_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions::default();
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();

    let files: Vec<(&str, &[u8])> = vec![
        (
            "Mods/TestMod/meta.lsx",
            b"<?xml version=\"1.0\"?><save><region id=\"Config\"/></save>" as &[u8],
        ),
        ("Public/TestMod/Races/Races.lsx", b"<lsx data>"),
        (
            "Public/TestMod/ClassDescriptions/ClassDescriptions.lsx",
            b"<class data>",
        ),
        ("Localization/English/english.xml", b"<loca data>"),
        ("Mods/TestMod/config.yaml", b"name: TestMod"),
        ("Public/TestMod/Stats/Generated/TestMod.txt", b"stats data"),
    ];

    for (path, data) in &files {
        writer
            .add_bytes(&PakPath::parse(path).unwrap(), data)
            .unwrap();
    }

    let result = writer.finish().unwrap();
    assert_eq!(result.file_count, 6);

    let reader = PakReader::open(tmp.path()).unwrap();
    assert_eq!(reader.entries().count(), 6);

    for (path, expected) in &files {
        let data = read_entry_bytes(&reader, path);
        assert_eq!(data, *expected, "content mismatch for {path}");
    }
}

#[test]
fn roundtrip_lz4() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions {
        compression: PakCompression::Lz4,
        ..PakWriterOptions::default()
    };
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();

    // Larger data that actually compresses
    let data = "The quick brown fox jumps over the lazy dog. ".repeat(100);
    writer
        .add_bytes(&PakPath::parse("test/large.txt").unwrap(), data.as_bytes())
        .unwrap();
    let result = writer.finish().unwrap();
    assert_eq!(result.file_count, 1);

    let reader = PakReader::open(tmp.path()).unwrap();
    let entry = reader
        .find(&PakPath::parse("test/large.txt").unwrap())
        .unwrap();
    assert!(entry.is_compressed(), "entry should be compressed");
    assert_eq!(entry.compression, PakCompression::Lz4);

    let read_back = read_entry_bytes(&reader, "test/large.txt");
    assert_eq!(read_back, data.as_bytes());
}

#[test]
fn roundtrip_zlib() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions {
        compression: PakCompression::Zlib,
        ..PakWriterOptions::default()
    };
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();

    let data = "Zlib compression test data. ".repeat(100);
    writer
        .add_bytes(&PakPath::parse("test/zlib.txt").unwrap(), data.as_bytes())
        .unwrap();
    let result = writer.finish().unwrap();
    assert_eq!(result.file_count, 1);

    let reader = PakReader::open(tmp.path()).unwrap();
    let entry = reader
        .find(&PakPath::parse("test/zlib.txt").unwrap())
        .unwrap();
    assert!(entry.is_compressed(), "entry should be compressed");
    assert_eq!(entry.compression, PakCompression::Zlib);

    let read_back = read_entry_bytes(&reader, "test/zlib.txt");
    assert_eq!(read_back, data.as_bytes());
}

#[test]
fn roundtrip_none() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions {
        compression: PakCompression::None,
        ..PakWriterOptions::default()
    };
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();

    let data = b"Uncompressed data";
    writer
        .add_bytes(&PakPath::parse("test/raw.txt").unwrap(), data)
        .unwrap();
    let result = writer.finish().unwrap();
    assert_eq!(result.file_count, 1);

    let reader = PakReader::open(tmp.path()).unwrap();
    let entry = reader
        .find(&PakPath::parse("test/raw.txt").unwrap())
        .unwrap();
    assert!(!entry.is_compressed(), "entry should not be compressed");

    let read_back = read_entry_bytes(&reader, "test/raw.txt");
    assert_eq!(read_back, data);
}

#[test]
fn meta_lsx_not_converted() {
    use bg3_cmty_studio_lib::pak::convert::should_convert_to_lsf;

    let meta_path = PakPath::parse("Mods/TestMod/meta.lsx").unwrap();
    assert!(
        !should_convert_to_lsf(&meta_path),
        "meta.lsx should never be converted to LSF"
    );

    // Also verify it's stored as-is in a pak
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut writer = PakWriter::new(tmp.path(), PakWriterOptions::default()).unwrap();
    let meta_content = b"<?xml version=\"1.0\" encoding=\"utf-8\"?><save><region id=\"Config\"><node id=\"root\"/></region></save>";
    writer.add_bytes(&meta_path, meta_content).unwrap();
    writer.finish().unwrap();

    let reader = PakReader::open(tmp.path()).unwrap();
    let entry = reader.find(&PakPath::parse("Mods/TestMod/meta.lsx").unwrap());
    assert!(
        entry.is_some(),
        "meta.lsx should be present with .lsx extension"
    );

    let lsf_entry = reader.find(&PakPath::parse("Mods/TestMod/meta.lsf").unwrap());
    assert!(lsf_entry.is_none(), "meta.lsf should NOT exist");

    let data = read_entry_bytes(&reader, "Mods/TestMod/meta.lsx");
    assert_eq!(data, meta_content);
}

#[test]
fn empty_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions {
        compression: PakCompression::Lz4,
        ..PakWriterOptions::default()
    };
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();
    writer
        .add_bytes(&PakPath::parse("test/empty.txt").unwrap(), b"")
        .unwrap();
    let result = writer.finish().unwrap();
    assert_eq!(result.file_count, 1);

    let reader = PakReader::open(tmp.path()).unwrap();
    let entry = reader
        .find(&PakPath::parse("test/empty.txt").unwrap())
        .unwrap();
    assert!(
        !entry.is_compressed(),
        "empty file should be stored uncompressed"
    );
    assert_eq!(entry.effective_size(), 0);

    let data = read_entry_bytes(&reader, "test/empty.txt");
    assert!(data.is_empty());
}

#[test]
fn path_normalization() {
    let pak_path = PakPath::parse("Public\\TestMod\\Races\\Races.lsx").unwrap();
    assert_eq!(pak_path.as_str(), "Public/TestMod/Races/Races.lsx");
}

#[test]
fn priority_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions {
        priority: 42,
        ..PakWriterOptions::default()
    };
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();
    writer
        .add_bytes(&PakPath::parse("test/data.txt").unwrap(), b"priority test")
        .unwrap();
    writer.finish().unwrap();

    let parsed = parse_package(tmp.path()).unwrap();
    assert_eq!(parsed.header.priority, 42, "priority should round-trip");
}

#[test]
fn compression_fallback() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let options = PakWriterOptions {
        compression: PakCompression::Lz4,
        ..PakWriterOptions::default()
    };
    let mut writer = PakWriter::new(tmp.path(), options).unwrap();

    // Very small data — compression won't help
    let data = b"hi";
    writer
        .add_bytes(&PakPath::parse("test/tiny.txt").unwrap(), data)
        .unwrap();
    writer.finish().unwrap();

    let reader = PakReader::open(tmp.path()).unwrap();
    let entry = reader
        .find(&PakPath::parse("test/tiny.txt").unwrap())
        .unwrap();
    // When compressed >= original, falls back to None
    assert!(
        !entry.is_compressed(),
        "tiny file should fall back to uncompressed"
    );

    let read_back = read_entry_bytes(&reader, "test/tiny.txt");
    assert_eq!(read_back, data);
}

// Manual test notes (not automated):
// 1. Write a pak → open with Divine.exe / BG3 Mod Manager → verify no errors
// 2. Write a pak → place in BG3 Mods folder → verify game loads it

/// End-to-end test: create a realistic mod structure, package it, read it back.
#[test]
fn e2e_realistic_mod_structure() {
    use std::fs;

    let tmp_dir = tempfile::tempdir().unwrap();
    let mod_root = tmp_dir.path().join("TestMod");

    // Create mod directory structure
    let meta_dir = mod_root.join("Mods").join("TestMod");
    fs::create_dir_all(&meta_dir).unwrap();
    fs::write(
        meta_dir.join("meta.lsx"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<save>
  <version major="4" minor="0" revision="9" build="331"/>
  <region id="Config">
    <node id="root">
      <children>
        <node id="ModuleInfo">
          <attribute id="Folder" type="LSString" value="TestMod"/>
          <attribute id="Name" type="LSString" value="Test Mod"/>
          <attribute id="UUID" type="FixedString" value="00000000-0000-0000-0000-000000000001"/>
        </node>
      </children>
    </node>
  </region>
</save>"#,
    )
    .unwrap();

    // Create a localization file
    let loca_dir = mod_root.join("Localization").join("English");
    fs::create_dir_all(&loca_dir).unwrap();
    fs::write(
        loca_dir.join("english.xml"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<contentList>
  <content contentuid="h00000001" version="1">Hello World</content>
</contentList>"#,
    )
    .unwrap();

    // Create a data LSX file (not in an LSF-required dir)
    let races_dir = mod_root.join("Public").join("TestMod").join("Races");
    fs::create_dir_all(&races_dir).unwrap();
    fs::write(
        races_dir.join("Races.lsx"),
        r#"<?xml version="1.0" encoding="utf-8"?>
<save>
  <version major="4" minor="0" revision="9" build="331"/>
  <region id="Races">
    <node id="root">
      <children>
        <node id="Race">
          <attribute id="UUID" type="FixedString" value="00000000-0000-0000-0000-000000000002"/>
          <attribute id="Name" type="LSString" value="TestRace"/>
        </node>
      </children>
    </node>
  </region>
</save>"#,
    )
    .unwrap();

    // Package the mod
    let pak_path = tmp_dir.path().join("TestMod.pak");
    let options = PakWriterOptions {
        compression: PakCompression::Lz4,
        ..PakWriterOptions::default()
    };
    let mut writer = PakWriter::new(&pak_path, options).unwrap();

    // Walk the directory and add files (simulating cmd_package_mod logic)
    let mut file_entries: Vec<(std::path::PathBuf, PakPath)> = Vec::new();
    for entry in walkdir::WalkDir::new(&mod_root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(&mod_root).unwrap();
        let pak_path_str = relative.to_string_lossy().replace('\\', "/");
        let pak_path = PakPath::parse(&pak_path_str).unwrap();
        file_entries.push((entry.path().to_path_buf(), pak_path));
    }
    file_entries.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()));

    for (disk_path, pak_path) in &file_entries {
        writer.add_file(disk_path, pak_path).unwrap();
    }
    let result = writer.finish().unwrap();
    assert_eq!(result.file_count, 3); // meta.lsx + english.xml + Races.lsx

    // Read back and verify
    let reader = PakReader::open(&pak_path).unwrap();
    assert_eq!(reader.entries().count(), 3);

    // meta.lsx should be present as .lsx (not .lsf)
    let meta = reader.find(&PakPath::parse("Mods/TestMod/meta.lsx").unwrap());
    assert!(meta.is_some(), "meta.lsx should be present");
    let meta_data = read_entry_bytes(&reader, "Mods/TestMod/meta.lsx");
    assert!(
        String::from_utf8_lossy(&meta_data).contains("TestMod"),
        "meta.lsx content should contain mod name"
    );

    // Localization file should be present
    let _loca_data = read_entry_bytes(&reader, "Localization/English/english.xml");

    // Races LSX should be present
    let _races_data = read_entry_bytes(&reader, "Public/TestMod/Races/Races.lsx");
}
