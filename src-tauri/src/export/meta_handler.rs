//! Export handler for `meta.lsx` — the required mod descriptor file.
//!
//! Every valid BG3 mod must include `Mods/{folder}/meta.lsx`. This handler
//! reads mod metadata from `_staging_authoring` keys (or falls back to
//! `lsx__Meta` table for imported mods and `ExportContext` defaults), then
//! renders a BG3-standard meta.lsx XML file.

use crate::error::AppError;
use crate::models::MetaDependency;

use super::{get_meta_value, ExportContext, ExportUnit, FileAction, FileTypeHandler};

/// Default version64 for BG3 mods (1.0.0.0 packed).
const DEFAULT_VERSION64: &str = "36028797018963968";

/// GustavDev base-game dependency — always included.
const GUSTAVDEV_UUID: &str = "28ac9ce2-2aba-8cda-b3b5-6e922f71b6b8";
const GUSTAVDEV_VERSION64: &str = "36028797018963968";

pub struct MetaLsxHandler;

impl FileTypeHandler for MetaLsxHandler {
    fn name(&self) -> &str {
        "MetaLsx"
    }

    fn claimed_table_prefixes(&self) -> Vec<&str> {
        vec!["lsx__Meta"]
    }

    fn claimed_meta_keys(&self) -> Vec<&str> {
        vec![
            "mod_uuid",
            "mod_name",
            "mod_author",
            "mod_description",
            "mod_version",
            "mod_folder",
            "mod_dependencies",
        ]
    }

    fn plan(&self, ctx: &ExportContext) -> Result<Vec<ExportUnit>, AppError> {
        let output_path = std::path::PathBuf::from("Mods")
            .join(&ctx.mod_folder)
            .join("meta.lsx");

        let action = if ctx.mod_path.join(&output_path).exists() {
            FileAction::Update
        } else {
            FileAction::Create
        };

        Ok(vec![ExportUnit {
            handler_name: self.name().to_string(),
            output_path,
            action,
            entry_count: 1,
            content: None,
        }])
    }

    fn render(&self, _unit: &ExportUnit, ctx: &ExportContext) -> Result<Vec<u8>, AppError> {
        let conn = &ctx.staging_conn;

        // Read metadata from _staging_authoring, fall back to ExportContext fields.
        let uuid = get_meta_value(conn, "mod_uuid")?.unwrap_or_default();
        let name = get_meta_value(conn, "mod_name")?.unwrap_or_else(|| ctx.mod_name.clone());
        let author = get_meta_value(conn, "mod_author")?.unwrap_or_default();
        let description = get_meta_value(conn, "mod_description")?.unwrap_or_default();
        let version64 =
            get_meta_value(conn, "mod_version")?.unwrap_or_else(|| DEFAULT_VERSION64.to_string());
        let folder = get_meta_value(conn, "mod_folder")?.unwrap_or_else(|| ctx.mod_folder.clone());

        // Parse dependencies from JSON array in staging, or default to GustavDev only.
        let dependencies = parse_dependencies(conn)?;

        // If staging_authoring had no UUID, try the lsx__Meta table (from imported mods).
        let (uuid, name, author, description, version64, folder) = if uuid.is_empty() {
            read_lsx_meta_fallback(
                conn,
                &uuid,
                &name,
                &author,
                &description,
                &version64,
                &folder,
            )?
        } else {
            (uuid, name, author, description, version64, folder)
        };

        let publish_version = get_meta_value(conn, "mod_publish_version")?
            .unwrap_or_else(|| DEFAULT_VERSION64.to_string());

        let xml = render_meta_lsx(
            &uuid,
            &name,
            &author,
            &description,
            &version64,
            &folder,
            &publish_version,
            &dependencies,
        );

        Ok(xml.into_bytes())
    }
}

// ---------------------------------------------------------------------------
// Dependency helpers
// ---------------------------------------------------------------------------

/// Parse dependencies from `_staging_authoring` key `mod_dependencies` (JSON array).
/// Always ensures GustavDev is present.
fn parse_dependencies(conn: &rusqlite::Connection) -> Result<Vec<MetaDependency>, AppError> {
    let mut deps: Vec<MetaDependency> = match get_meta_value(conn, "mod_dependencies")? {
        Some(json_str) if !json_str.is_empty() => {
            serde_json::from_str(&json_str).unwrap_or_default()
        }
        _ => Vec::new(),
    };

    // Ensure GustavDev is always present.
    let has_gustavdev = deps.iter().any(|d| d.uuid == GUSTAVDEV_UUID);
    if !has_gustavdev {
        deps.insert(
            0,
            MetaDependency {
                uuid: GUSTAVDEV_UUID.to_string(),
                name: "GustavDev".to_string(),
                folder: "GustavDev".to_string(),
                md5: String::new(),
                version64: GUSTAVDEV_VERSION64.to_string(),
            },
        );
    }

    Ok(deps)
}

// ---------------------------------------------------------------------------
// lsx__Meta table fallback
// ---------------------------------------------------------------------------

/// Attempt to read mod metadata from the `lsx__Meta` table (populated when a
/// meta.lsx was imported). Returns the original values when the table doesn't
/// exist or has no data.
fn read_lsx_meta_fallback(
    conn: &rusqlite::Connection,
    uuid: &str,
    name: &str,
    author: &str,
    description: &str,
    version64: &str,
    folder: &str,
) -> Result<(String, String, String, String, String, String), AppError> {
    // Check if the table exists.
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='lsx__Meta'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !table_exists {
        return Ok((
            uuid.to_string(),
            name.to_string(),
            author.to_string(),
            description.to_string(),
            version64.to_string(),
            folder.to_string(),
        ));
    }

    // Read the first row. Column names match ModuleInfo attribute IDs.
    let result = conn.query_row(
        "SELECT UUID, Name, Author, Description, Version64, Folder \
         FROM lsx__Meta LIMIT 1",
        [],
        |row| {
            Ok((
                row.get::<_, String>(0).unwrap_or_default(),
                row.get::<_, String>(1).unwrap_or_default(),
                row.get::<_, String>(2).unwrap_or_default(),
                row.get::<_, String>(3).unwrap_or_default(),
                row.get::<_, String>(4).unwrap_or_default(),
                row.get::<_, String>(5).unwrap_or_default(),
            ))
        },
    );

    match result {
        Ok((db_uuid, db_name, db_author, db_desc, db_ver, db_folder)) => Ok((
            if db_uuid.is_empty() {
                uuid.to_string()
            } else {
                db_uuid
            },
            if db_name.is_empty() {
                name.to_string()
            } else {
                db_name
            },
            if db_author.is_empty() {
                author.to_string()
            } else {
                db_author
            },
            if db_desc.is_empty() {
                description.to_string()
            } else {
                db_desc
            },
            if db_ver.is_empty() {
                version64.to_string()
            } else {
                db_ver
            },
            if db_folder.is_empty() {
                folder.to_string()
            } else {
                db_folder
            },
        )),
        // Table exists but is empty or has missing columns — use originals.
        Err(_) => Ok((
            uuid.to_string(),
            name.to_string(),
            author.to_string(),
            description.to_string(),
            version64.to_string(),
            folder.to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// XML rendering
// ---------------------------------------------------------------------------

/// Escape special XML characters in attribute values.
fn xml_escape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => result.push_str("&amp;"),
            '<' => result.push_str("&lt;"),
            '>' => result.push_str("&gt;"),
            '"' => result.push_str("&quot;"),
            '\'' => result.push_str("&apos;"),
            _ => result.push(c),
        }
    }
    result
}

/// Write a single `<attribute id="..." type="..." value="..." />` line.
fn write_attribute(out: &mut String, id: &str, attr_type: &str, value: &str, indent: usize) {
    for _ in 0..indent {
        out.push(' ');
    }
    out.push_str("<attribute id=\"");
    out.push_str(&xml_escape(id));
    out.push_str("\" type=\"");
    out.push_str(&xml_escape(attr_type));
    out.push_str("\" value=\"");
    out.push_str(&xml_escape(value));
    out.push_str("\" />\n");
}

/// Render a complete `meta.lsx` XML string from mod metadata.
#[allow(clippy::too_many_arguments)]
fn render_meta_lsx(
    uuid: &str,
    name: &str,
    author: &str,
    description: &str,
    version64: &str,
    folder: &str,
    publish_version: &str,
    dependencies: &[MetaDependency],
) -> String {
    let mut out = String::with_capacity(2048);

    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<save>\n");
    out.push_str("  <version major=\"4\" minor=\"0\" revision=\"0\" build=\"49\" />\n");
    out.push_str("  <region id=\"Config\">\n");
    out.push_str("    <node id=\"root\">\n");
    out.push_str("      <children>\n");

    // ── Dependencies ──
    out.push_str("        <node id=\"Dependencies\">\n");
    out.push_str("          <children>\n");
    for dep in dependencies {
        out.push_str("            <node id=\"ModuleShortDesc\">\n");
        write_attribute(&mut out, "Folder", "LSString", &dep.folder, 14);
        write_attribute(&mut out, "MD5", "LSString", &dep.md5, 14);
        write_attribute(&mut out, "Name", "LSString", &dep.name, 14);
        write_attribute(&mut out, "UUID", "guid", &dep.uuid, 14);
        write_attribute(&mut out, "Version64", "int64", &dep.version64, 14);
        out.push_str("            </node>\n");
    }
    out.push_str("          </children>\n");
    out.push_str("        </node>\n");

    // ── ModuleInfo ──
    out.push_str("        <node id=\"ModuleInfo\">\n");
    write_attribute(&mut out, "Author", "LSString", author, 10);
    write_attribute(
        &mut out,
        "CharacterCreationLevelName",
        "FixedString",
        "",
        10,
    );
    write_attribute(&mut out, "Description", "LSString", description, 10);
    write_attribute(&mut out, "Folder", "LSString", folder, 10);
    write_attribute(&mut out, "GMTemplate", "FixedString", "", 10);
    write_attribute(&mut out, "LobbyLevelName", "FixedString", "", 10);
    write_attribute(&mut out, "MD5", "LSString", "", 10);
    write_attribute(&mut out, "MainMenuBackgroundVideo", "FixedString", "", 10);
    write_attribute(&mut out, "MenuLevelName", "FixedString", "", 10);
    write_attribute(&mut out, "Name", "LSString", name, 10);
    write_attribute(&mut out, "NumPlayers", "uint8", "4", 10);
    write_attribute(&mut out, "PhotoBooth", "FixedString", "", 10);
    write_attribute(&mut out, "StartupLevelName", "FixedString", "", 10);
    write_attribute(&mut out, "Tags", "LSString", "", 10);
    write_attribute(&mut out, "Type", "FixedString", "Add-on", 10);
    write_attribute(&mut out, "UUID", "guid", uuid, 10);
    write_attribute(&mut out, "Version64", "int64", version64, 10);
    write_attribute(&mut out, "PublishVersion", "int64", publish_version, 10);
    out.push_str("        </node>\n");

    out.push_str("      </children>\n");
    out.push_str("    </node>\n");
    out.push_str("  </region>\n");
    out.push_str("</save>\n");

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_special_chars() {
        assert_eq!(xml_escape("hello"), "hello");
        assert_eq!(
            xml_escape("a&b<c>d\"e'f"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
        assert_eq!(xml_escape(""), "");
    }

    #[test]
    fn xml_escape_unicode_passthrough() {
        assert_eq!(xml_escape("Ünïcödé Mød"), "Ünïcödé Mød");
        assert_eq!(xml_escape("日本語"), "日本語");
    }

    #[test]
    fn render_basic_meta_lsx() {
        let deps = vec![MetaDependency {
            uuid: GUSTAVDEV_UUID.to_string(),
            name: "GustavDev".to_string(),
            folder: "GustavDev".to_string(),
            md5: String::new(),
            version64: GUSTAVDEV_VERSION64.to_string(),
        }];

        let xml = render_meta_lsx(
            "aaaabbbb-cccc-dddd-eeee-ffffffffffff",
            "TestMod",
            "TestAuthor",
            "A test mod",
            DEFAULT_VERSION64,
            "TestModFolder",
            DEFAULT_VERSION64,
            &deps,
        );

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<save>"));
        assert!(xml.contains("<region id=\"Config\">"));
        assert!(xml.contains("id=\"Dependencies\""));
        assert!(xml.contains("id=\"ModuleInfo\""));
        assert!(xml.contains("id=\"ModuleShortDesc\""));

        // Mod info fields
        assert!(xml.contains("value=\"TestMod\""));
        assert!(xml.contains("value=\"TestAuthor\""));
        assert!(xml.contains("value=\"A test mod\""));
        assert!(xml.contains("value=\"TestModFolder\""));
        assert!(xml.contains("value=\"aaaabbbb-cccc-dddd-eeee-ffffffffffff\""));
        assert!(xml.contains("value=\"Add-on\""));

        // GustavDev dependency
        assert!(xml.contains("value=\"GustavDev\""));
        assert!(xml.contains(&format!("value=\"{GUSTAVDEV_UUID}\"")));

        // All required empty-string fields present
        for field in &[
            "CharacterCreationLevelName",
            "GMTemplate",
            "LobbyLevelName",
            "PhotoBooth",
            "StartupLevelName",
            "MenuLevelName",
            "MainMenuBackgroundVideo",
        ] {
            assert!(
                xml.contains(&format!("id=\"{field}\"")),
                "missing field {field}"
            );
        }
    }

    #[test]
    fn render_xml_escaping_in_values() {
        let deps = vec![MetaDependency {
            uuid: GUSTAVDEV_UUID.to_string(),
            name: "GustavDev".to_string(),
            folder: "GustavDev".to_string(),
            md5: String::new(),
            version64: GUSTAVDEV_VERSION64.to_string(),
        }];

        let xml = render_meta_lsx(
            "00000000-0000-0000-0000-000000000000",
            "Mod <With> \"Special\" & Chars",
            "Author's \"Name\"",
            "A <description> with & symbols",
            DEFAULT_VERSION64,
            "TestFolder",
            DEFAULT_VERSION64,
            &deps,
        );

        assert!(xml.contains("value=\"Mod &lt;With&gt; &quot;Special&quot; &amp; Chars\""));
        assert!(xml.contains("value=\"Author&apos;s &quot;Name&quot;\""));
        assert!(xml.contains("value=\"A &lt;description&gt; with &amp; symbols\""));
    }

    #[test]
    fn render_multiple_dependencies() {
        let deps = vec![
            MetaDependency {
                uuid: GUSTAVDEV_UUID.to_string(),
                name: "GustavDev".to_string(),
                folder: "GustavDev".to_string(),
                md5: String::new(),
                version64: GUSTAVDEV_VERSION64.to_string(),
            },
            MetaDependency {
                uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                name: "SomeDep".to_string(),
                folder: "SomeDepFolder".to_string(),
                md5: "abc123".to_string(),
                version64: "36028797018963968".to_string(),
            },
        ];

        let xml = render_meta_lsx(
            "00000000-0000-0000-0000-000000000000",
            "MyMod",
            "Me",
            "",
            DEFAULT_VERSION64,
            "MyModFolder",
            DEFAULT_VERSION64,
            &deps,
        );

        assert!(xml.contains("value=\"GustavDev\""));
        assert!(xml.contains("value=\"SomeDep\""));
        assert!(xml.contains("value=\"SomeDepFolder\""));
        assert!(xml.contains("value=\"abc123\""));

        let desc_count = xml.matches("id=\"ModuleShortDesc\"").count();
        assert_eq!(desc_count, 2);
    }

    #[test]
    fn render_unicode_mod_metadata() {
        let deps = vec![MetaDependency {
            uuid: GUSTAVDEV_UUID.to_string(),
            name: "GustavDev".to_string(),
            folder: "GustavDev".to_string(),
            md5: String::new(),
            version64: GUSTAVDEV_VERSION64.to_string(),
        }];

        let xml = render_meta_lsx(
            "00000000-0000-0000-0000-000000000000",
            "Ünïcödé Mød Nàmé",
            "日本語の作者",
            "Описание мода на русском",
            DEFAULT_VERSION64,
            "UnicodeMod",
            DEFAULT_VERSION64,
            &deps,
        );

        assert!(xml.contains("value=\"Ünïcödé Mød Nàmé\""));
        assert!(xml.contains("value=\"日本語の作者\""));
        assert!(xml.contains("value=\"Описание мода на русском\""));
    }

    #[test]
    fn render_empty_optional_fields() {
        let deps = vec![MetaDependency {
            uuid: GUSTAVDEV_UUID.to_string(),
            name: "GustavDev".to_string(),
            folder: "GustavDev".to_string(),
            md5: String::new(),
            version64: GUSTAVDEV_VERSION64.to_string(),
        }];

        let xml = render_meta_lsx(
            "00000000-0000-0000-0000-000000000000",
            "MinimalMod",
            "",
            "",
            DEFAULT_VERSION64,
            "MinimalFolder",
            DEFAULT_VERSION64,
            &deps,
        );

        // Author and Description should be present but empty
        assert!(xml.contains("id=\"Author\" type=\"LSString\" value=\"\""));
        assert!(xml.contains("id=\"Description\" type=\"LSString\" value=\"\""));
        // MD5 always empty
        assert!(xml.contains("id=\"MD5\" type=\"LSString\" value=\"\""));
    }

    #[test]
    fn render_well_formed_xml_structure() {
        let deps = vec![MetaDependency {
            uuid: GUSTAVDEV_UUID.to_string(),
            name: "GustavDev".to_string(),
            folder: "GustavDev".to_string(),
            md5: String::new(),
            version64: GUSTAVDEV_VERSION64.to_string(),
        }];

        let xml = render_meta_lsx(
            "00000000-0000-0000-0000-000000000000",
            "Test",
            "Author",
            "Desc",
            DEFAULT_VERSION64,
            "TestFolder",
            DEFAULT_VERSION64,
            &deps,
        );

        // Verify balanced open/close tags
        assert_eq!(xml.matches("<save>").count(), 1);
        assert_eq!(xml.matches("</save>").count(), 1);
        assert_eq!(
            xml.matches("<children>").count(),
            xml.matches("</children>").count()
        );
        assert!(xml.contains("</region>"));
        assert!(xml.ends_with("</save>\n"));
    }
}
