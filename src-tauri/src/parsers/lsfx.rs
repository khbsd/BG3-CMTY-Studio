use std::io::{Read, Seek, Write};
use std::path::Path;

use crate::models::LsxResource;
use crate::parsers::lsf as lsf_parser;

/// Parse a BG3 runtime `.lsfx` file into the shared resource tree.
///
/// The current implementation reuses the native LSF reader because real `.lsfx`
/// binaries are stored in the same `LSOF` container family. Keeping a dedicated
/// module boundary lets follow-up work add effect-specific semantics without
/// coupling callers directly to the generic `.lsf` parser.
pub fn parse_lsfx_file(path: &Path) -> Result<LsxResource, String> {
    lsf_parser::parse_lsf_file(path)
}

pub fn parse_lsfx<R: Read + Seek>(reader: R) -> Result<LsxResource, String> {
    lsf_parser::parse_lsf(reader)
}

/// Write an `LsxResource` as a BG3 runtime `.lsfx` binary.
pub fn write_lsfx_file(path: &Path, resource: &LsxResource) -> Result<(), String> {
    lsf_parser::write_lsf_file(path, resource)
}

/// Write an `LsxResource` as a BG3 runtime `.lsfx` binary to any writer.
pub fn write_lsfx<W: Write>(writer: &mut W, resource: &LsxResource) -> Result<(), String> {
    lsf_parser::write_lsf(writer, resource)
}

#[cfg(test)]
mod tests {
    use crate::models::LsxResource;
    use crate::pak::PakReader;
    use dotenvy::from_path;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn load_env() {
        let env_path = workspace_root().join(".env");
        if env_path.is_file() {
            let _ = from_path(&env_path);
        }
    }

    fn effects_pak_path() -> PathBuf {
        load_env();
        let data_dir = std::env::var("BG3_GAME_DATA")
            .expect("BG3_GAME_DATA not set — add it to .env or set the env var");
        PathBuf::from(data_dir).join("Effects.pak")
    }

    fn first_non_empty_lsfx_entry(reader: &PakReader) -> Option<(String, Vec<u8>)> {
        for entry in reader.entries() {
            if entry.is_deleted()
                || !entry
                    .path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lsfx"))
            {
                continue;
            }

            let mut pak_entry_reader = reader.open_entry(entry).ok()?;
            let bytes = pak_entry_reader
                .read_to_end_with_limit(64 * 1024 * 1024)
                .ok()?;
            if bytes.is_empty() {
                continue;
            }

            return Some((entry.path.as_str().to_string(), bytes));
        }

        None
    }

    #[test]
    #[ignore = "requires BG3_GAME_DATA in .env to read Effects.pak"]
    fn parses_real_lsfx_from_pak() {
        let pak_path = effects_pak_path();
        assert!(pak_path.is_file(), "missing {}", pak_path.display());

        let reader = PakReader::open(&pak_path).unwrap();
        let (entry_path, entry_bytes) = first_non_empty_lsfx_entry(&reader)
            .expect("expected at least one non-empty .lsfx entry in Effects.pak");

        let resource = crate::parsers::lsfx::parse_lsfx(Cursor::new(entry_bytes)).unwrap();
        assert!(
            !resource.regions.is_empty(),
            "expected at least one region in {entry_path}"
        );
        assert!(
            resource
                .regions
                .iter()
                .any(|region| !region.nodes.is_empty()),
            "expected at least one populated region in {entry_path}",
        );
    }

    // ── Unhappy-path tests (via lsfx → lsf delegation) ────────────

    #[test]
    fn parse_empty_bytes_returns_error() {
        let result = super::parse_lsfx(Cursor::new(Vec::<u8>::new()));
        assert!(result.is_err(), "empty input should fail");
        let err = result.unwrap_err();
        assert!(
            err.contains("too small"),
            "expected 'too small' error, got: {err}"
        );
    }

    #[test]
    fn parse_truncated_header_returns_error() {
        // Only 8 bytes — less than the 16-byte minimum header
        let result = super::parse_lsfx(Cursor::new(vec![0u8; 8]));
        assert!(result.is_err(), "truncated header should fail");
    }

    #[test]
    fn parse_wrong_magic_returns_error() {
        // 16 bytes with wrong magic signature
        let mut data = vec![0u8; 64];
        data[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        let result = super::parse_lsfx(Cursor::new(data));
        assert!(result.is_err(), "wrong magic should fail");
        let err = result.unwrap_err();
        assert!(
            err.contains("signature") || err.contains("Incorrect"),
            "expected signature error, got: {err}"
        );
    }

    #[test]
    fn parse_random_garbage_returns_error() {
        // 256 bytes of random-ish data — not a valid LSOF container
        let garbage: Vec<u8> = (0..256).map(|i| (i * 37 + 13) as u8).collect();
        let result = super::parse_lsfx(Cursor::new(garbage));
        assert!(result.is_err(), "random garbage should fail");
    }

    #[test]
    fn write_then_parse_empty_resource() {
        // An LsxResource with no regions should still round-trip through binary
        let empty = LsxResource { regions: vec![] };
        let mut buf = Vec::new();
        super::write_lsfx(&mut buf, &empty).expect("write empty resource");
        assert!(
            !buf.is_empty(),
            "even empty resource produces binary output"
        );
        let parsed = super::parse_lsfx(Cursor::new(buf)).expect("parse empty resource");
        assert!(parsed.regions.is_empty());
    }
}
