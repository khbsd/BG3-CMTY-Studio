use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::pak::compression;
use crate::pak::entry::PakEntry;
use crate::pak::error::{PakError, PakResult};
use crate::pak::format::{self, PakPackageFlags};
use crate::pak::manifest::PakManifest;
use crate::pak::path::PakPath;
use crate::pak::stream::{BoundedReader, PakEntryReader};

pub struct PakPartHandle {
    pub index: u16,
    pub path: PathBuf,
    file: File,
}

pub trait PackageSource {
    fn manifest(&self) -> &PakManifest;
    fn open_entry(&self, entry: &PakEntry) -> PakResult<PakEntryReader>;
}

pub struct PakReader {
    source_path: PathBuf,
    manifest: PakManifest,
    parts: Vec<PakPartHandle>,
}

impl PakReader {
    pub fn open(path: &Path) -> PakResult<Self> {
        if !path.exists() {
            return Err(PakError::not_found(path.display().to_string()));
        }

        let parsed = format::parse_package(path)?;
        let parts = build_part_handles(path, parsed.header.num_parts)?;
        let manifest = PakManifest::new(
            parsed.header.version,
            parsed.header.flags,
            parsed
                .entries
                .into_iter()
                .map(|entry| PakEntry {
                    path: entry.path,
                    archive_part: entry.archive_part,
                    offset: entry.offset,
                    size_on_disk: entry.size_on_disk,
                    uncompressed_size: entry.uncompressed_size,
                    compression: entry.compression,
                    flags: entry.flags,
                })
                .collect(),
        );

        log::debug!("{:#?}", manifest);

        Ok(Self {
            source_path: path.to_path_buf(),
            manifest,
            parts,
        })
    }

    pub fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub fn manifest(&self) -> &PakManifest {
        &self.manifest
    }

    pub fn entries(&self) -> std::slice::Iter<'_, PakEntry> {
        self.manifest.iter()
    }

    pub fn active_entry_paths(&self) -> Vec<String> {
        self.entries()
            .filter(|entry| !entry.is_deleted())
            .map(|entry| entry.path.as_str().to_string())
            .collect()
    }

    pub fn find(&self, path: &PakPath) -> Option<&PakEntry> {
        self.manifest.get(path)
    }

    pub fn open_entry(&self, entry: &PakEntry) -> PakResult<PakEntryReader> {
        if entry.is_deleted() {
            return Err(PakError::deleted_entry(entry.path.as_str().to_string()));
        }

        if self
            .manifest
            .package_flags()
            .contains(PakPackageFlags::SOLID)
        {
            return Err(PakError::not_implemented(
                "solid pak entry decoding is not implemented yet",
            ));
        }

        let part = self
            .parts
            .iter()
            .find(|part| part.index == entry.archive_part)
            .ok_or_else(|| {
                PakError::not_found(format!(
                    "archive part {} for {}",
                    entry.archive_part,
                    entry.path.as_str()
                ))
            })?;

        let file = part.file.try_clone()?;
        let bounded = BoundedReader::new(file, entry.offset, entry.size_on_disk)?;

        if entry.is_compressed() {
            let wrapped = compression::wrap_reader(
                Box::new(bounded),
                entry.compression,
                entry.effective_size(),
            )?;
            Ok(PakEntryReader::Decompressed(wrapped))
        } else {
            Ok(PakEntryReader::Raw(bounded))
        }
    }

    pub fn open_path(&self, path: &PakPath) -> PakResult<PakEntryReader> {
        let entry = self
            .find(path)
            .ok_or_else(|| PakError::not_found(path.as_str().to_string()))?;
        self.open_entry(entry)
    }
}

impl PackageSource for PakReader {
    fn manifest(&self) -> &PakManifest {
        self.manifest()
    }

    fn open_entry(&self, entry: &PakEntry) -> PakResult<PakEntryReader> {
        PakReader::open_entry(self, entry)
    }
}

fn build_part_handles(source_path: &Path, num_parts: u16) -> PakResult<Vec<PakPartHandle>> {
    let mut parts = Vec::with_capacity(num_parts as usize);
    parts.push(PakPartHandle {
        index: 0,
        path: source_path.to_path_buf(),
        file: File::open(source_path)?,
    });

    for part in 1..num_parts {
        let path = make_part_filename(source_path, part);
        if !path.exists() {
            return Err(PakError::not_found(format!(
                "multipart pak part is missing: {}",
                path.display()
            )));
        }

        let file = File::open(&path)?;
        parts.push(PakPartHandle {
            index: part,
            path,
            file,
        });
    }

    Ok(parts)
}

fn make_part_filename(source_path: &Path, part: u16) -> PathBuf {
    let dir = source_path.parent().unwrap_or_else(|| Path::new(""));
    let stem = source_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = source_path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    dir.join(format!("{stem}_{part}{ext}"))
}

/**
 * pak readers
 */

pub fn pak_read_u8<R: Read>(reader: &mut R) -> PakResult<u8> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

pub fn pak_read_u16<R: Read>(reader: &mut R) -> PakResult<u16> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

pub fn pak_read_u32<R: Read>(reader: &mut R) -> PakResult<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn pak_read_u64<R: Read>(reader: &mut R) -> PakResult<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

/**
 * bytes readers
 */

pub fn bytes_read_u8<R: Read>(reader: &mut R) -> Result<u8, String> {
    let mut buf = [0u8; 1];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read u8: {e}"))?;
    Ok(buf[0])
}

pub fn bytes_read_i8<R: Read>(reader: &mut R) -> Result<i8, String> {
    Ok(bytes_read_u8(reader)? as i8)
}

pub fn bytes_read_u16<R: Read>(reader: &mut R) -> Result<u16, String> {
    let mut buf = [0u8; 2];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read u16: {e}"))?;
    Ok(u16::from_le_bytes(buf))
}

pub fn bytes_read_i16<R: Read>(reader: &mut R) -> Result<i16, String> {
    let mut buf = [0u8; 2];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read i16: {e}"))?;
    Ok(i16::from_le_bytes(buf))
}

pub fn bytes_read_u32<R: Read>(reader: &mut R) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read u32: {e}"))?;
    Ok(u32::from_le_bytes(buf))
}

pub fn bytes_read_u32_opt<R: Read>(reader: &mut R) -> Result<Option<u32>, String> {
    let mut buf = [0u8; 4];
    match reader.read_exact(&mut buf) {
        Ok(()) => Ok(Some(u32::from_le_bytes(buf))),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(err) => Err(format!("Failed to read u32: {err}")),
    }
}

pub fn bytes_read_i32<R: Read>(reader: &mut R) -> Result<i32, String> {
    let mut buf = [0u8; 4];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read i32: {e}"))?;
    Ok(i32::from_le_bytes(buf))
}

pub fn bytes_read_u64<R: Read>(reader: &mut R) -> Result<u64, String> {
    let mut buf = [0u8; 8];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read u64: {e}"))?;
    Ok(u64::from_le_bytes(buf))
}

pub fn bytes_read_i64<R: Read>(reader: &mut R) -> Result<i64, String> {
    let mut buf = [0u8; 8];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read i64: {e}"))?;
    Ok(i64::from_le_bytes(buf))
}

pub fn bytes_read_f32<R: Read>(reader: &mut R) -> Result<f32, String> {
    let mut buf = [0u8; 4];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read f32: {e}"))?;
    Ok(f32::from_le_bytes(buf))
}

pub fn bytes_read_f64<R: Read>(reader: &mut R) -> Result<f64, String> {
    let mut buf = [0u8; 8];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("Failed to read f64: {e}"))?;
    Ok(f64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::*;

    const TEST_SIGNATURE: &[u8; 4] = b"LSPK";
    const TEST_VERSION: u32 = 18;
    const TEST_HEADER_SIZE: usize = 36;
    const TEST_DATA_OFFSET: usize = 4 + TEST_HEADER_SIZE;
    const TEST_FILE_ENTRY_SIZE: usize = 272;

    #[test]
    fn opens_synthetic_current_bg3_package_and_reads_entry() {
        let dir = tempdir().unwrap();
        let pak_path = dir.path().join("Synthetic.pak");
        let package = build_test_package("Public/Test/Example.txt", b"hello from pak", 0, 0);
        fs::write(&pak_path, package).unwrap();

        let reader = PakReader::open(&pak_path).unwrap();
        assert_eq!(reader.manifest().len(), 1);

        let path = PakPath::parse("Public/Test/Example.txt").unwrap();
        let mut entry_reader = reader.open_path(&path).unwrap();
        let bytes = entry_reader.read_to_end_with_limit(1024).unwrap();
        assert_eq!(bytes, b"hello from pak");
        assert_eq!(
            reader.active_entry_paths(),
            vec!["Public/Test/Example.txt".to_string()]
        );
    }

    #[test]
    #[ignore = "requires Gustav.pak and English.pak copied into the workspace root"]
    fn reads_real_workspace_paks() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let gustav_path = root.join("Gustav.pak");
        let english_path = root.join("English.pak");

        assert!(gustav_path.exists(), "missing {}", gustav_path.display());
        assert!(english_path.exists(), "missing {}", english_path.display());

        let gustav = PakReader::open(&gustav_path).unwrap();
        let english = PakReader::open(&english_path).unwrap();

        assert!(!gustav.manifest().is_empty());
        assert!(!english.manifest().is_empty());
        assert!(gustav
            .entries()
            .any(|entry| entry.path.starts_with("Public/Gustav")));
        assert!(english.entries().any(|entry| entry
            .path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("loca"))));

        if !english
            .manifest()
            .package_flags()
            .contains(PakPackageFlags::SOLID)
        {
            let entry = english
                .entries()
                .find(|entry| !entry.is_deleted() && !entry.is_compressed())
                .or_else(|| english.entries().find(|entry| !entry.is_deleted()))
                .expect("expected at least one readable english entry");

            let mut reader = english.open_entry(entry).unwrap();
            let bytes = reader.read_to_end_with_limit(1024 * 1024).unwrap();
            assert!(!bytes.is_empty());
        }
    }

    fn build_test_package(path: &str, body: &[u8], flags: u8, package_flags: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(TEST_SIGNATURE);
        bytes.extend_from_slice(&TEST_VERSION.to_le_bytes());

        let file_list_offset_placeholder = bytes.len();
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        let file_list_size_placeholder = bytes.len();
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.push(package_flags);
        bytes.push(0_u8);
        bytes.extend_from_slice(&[0_u8; 16]);
        bytes.extend_from_slice(&1_u16.to_le_bytes());

        assert_eq!(bytes.len(), TEST_DATA_OFFSET);

        let entry_offset = bytes.len() as u64;
        bytes.extend_from_slice(body);

        let mut entry = vec![0_u8; TEST_FILE_ENTRY_SIZE];
        let path_bytes = path.as_bytes();
        entry[..path_bytes.len()].copy_from_slice(path_bytes);
        entry[256..260].copy_from_slice(&(entry_offset as u32).to_le_bytes());
        entry[260..262].copy_from_slice(&((entry_offset >> 32) as u16).to_le_bytes());
        entry[262] = 0;
        entry[263] = flags;
        entry[264..268].copy_from_slice(&(body.len() as u32).to_le_bytes());
        entry[268..272].copy_from_slice(&0_u32.to_le_bytes());

        let compressed = lz4_flex::block::compress(&entry);

        let file_list_offset = bytes.len() as u64;
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&compressed);

        let file_list_size = (bytes.len() as u64 - file_list_offset) as u32;
        bytes[file_list_offset_placeholder..file_list_offset_placeholder + 8]
            .copy_from_slice(&file_list_offset.to_le_bytes());
        bytes[file_list_size_placeholder..file_list_size_placeholder + 4]
            .copy_from_slice(&file_list_size.to_le_bytes());
        bytes
    }
}
