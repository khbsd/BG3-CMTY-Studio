use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use crate::pak::compression;
use crate::pak::error::{PakError, PakResult};
use crate::pak::path::PakPath;
use crate::pak::reader::{pak_read_u16, pak_read_u32, pak_read_u64, pak_read_u8};

const PACKAGE_SIGNATURE: u32 = 0x4B50_534C;
const CURRENT_BG3_VERSION_VALUE: u32 = 18;
const CURRENT_BG3_DATA_OFFSET: u64 = 4 + HEADER16_SIZE as u64;
const HEADER16_SIZE: usize = 36;
const FILE_ENTRY18_SIZE: usize = 272;
const FILE_NAME_LEN: usize = 256;
const MAX_PAK_FILE_COUNT: usize = 1_000_000;
const MAX_ENTRY_PATH_LEN: usize = 1024;
const MAX_FILE_LIST_COMPRESSED_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILE_LIST_DECOMPRESSED_BYTES: usize = MAX_PAK_FILE_COUNT * FILE_ENTRY18_SIZE;
const DELETION_OFFSET_SENTINEL: u64 = 0x0000_BEEF_DEAD_BEEF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PakVersion {
    Bg3Current,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PakCompression {
    None,
    Zlib,
    Lz4,
    Zstd,
    Unknown(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionLevel {
    Fast,
    Default,
    Max,
}

impl CompressionLevel {
    pub fn to_flag_bits(self) -> u8 {
        match self {
            Self::Fast => 0x10,
            Self::Default => 0x20,
            Self::Max => 0x40,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PakEntryFlags(u32);

impl PakEntryFlags {
    pub const DELETION: u32 = 0x0001;
    pub const SOLID: u32 = 0x0002;

    pub fn new(bits: u32) -> Self {
        Self(bits)
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn contains(self, flag: u32) -> bool {
        self.0 & flag == flag
    }

    pub fn with(self, flag: u32) -> Self {
        Self(self.0 | flag)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PakPackageFlags(u32);

impl PakPackageFlags {
    pub const ALLOW_MEMORY_MAPPING: u32 = 0x0002;
    pub const SOLID: u32 = 0x0004;
    pub const PRELOAD: u32 = 0x0008;

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn new(bits: u32) -> Self {
        Self(bits)
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn contains(self, flag: u32) -> bool {
        self.0 & flag == flag
    }
}

#[derive(Debug, Clone)]
pub struct RawPackageHeader {
    pub version: PakVersion,
    pub flags: PakPackageFlags,
    pub file_list_offset: u64,
    pub file_list_size: u32,
    pub file_count: usize,
    pub num_parts: u16,
    pub data_offset: u64,
    pub priority: u8,
}

#[derive(Debug, Clone)]
pub struct RawFileEntry {
    pub path: PakPath,
    pub archive_part: u16,
    pub offset: u64,
    pub size_on_disk: u64,
    pub uncompressed_size: u64,
    pub compression: PakCompression,
    pub flags: PakEntryFlags,
}

#[derive(Debug, Clone)]
pub struct RawPackageMetadata {
    pub header: RawPackageHeader,
    pub entries: Vec<RawFileEntry>,
}

impl PakCompression {
    pub fn from_method_bits(bits: u8) -> Self {
        match bits & 0x0F {
            0 => Self::None,
            1 => Self::Zlib,
            2 => Self::Lz4,
            3 => Self::Zstd,
            value => Self::Unknown(value as u32),
        }
    }

    pub fn method_bits(&self) -> u8 {
        match self {
            Self::None => 0x00,
            Self::Zlib => 0x01,
            Self::Lz4 => 0x02,
            Self::Zstd => 0x03,
            Self::Unknown(v) => *v as u8 & 0x0F,
        }
    }
}

pub fn parse_package(path: &Path) -> PakResult<RawPackageMetadata> {
    let mut file = File::open(path)?;
    let source_len = file.metadata()?.len();
    parse_from_reader(&mut file, source_len)
}

fn parse_from_reader<R: Read + Seek>(
    reader: &mut R,
    source_len: u64,
) -> PakResult<RawPackageMetadata> {
    if source_len < CURRENT_BG3_DATA_OFFSET {
        return Err(PakError::invalid_format(format!(
            "pak is too small to contain a BG3 v18 header: {source_len} bytes"
        )));
    }

    reader.seek(SeekFrom::Start(0))?;
    let signature = pak_read_u32(reader)?;
    if signature != PACKAGE_SIGNATURE {
        return Err(PakError::invalid_format(format!(
            "invalid pak signature: expected 0x{PACKAGE_SIGNATURE:08X}, got 0x{signature:08X}"
        )));
    }

    let version = pak_read_u32(reader)?;
    if version != CURRENT_BG3_VERSION_VALUE {
        return Err(PakError::unsupported_version(version));
    }

    let file_list_offset = pak_read_u64(reader)?;
    let file_list_size = pak_read_u32(reader)?;
    //log::info!(
    //    "file_list offset: {:?}, file_list size: {:?}",
    //    file_list_offset,
    //    file_list_size
    //);
    let flags = PakPackageFlags::new(pak_read_u8(reader)? as u32);
    let priority = pak_read_u8(reader)?;
    let mut md5 = [0_u8; 16];
    reader.read_exact(&mut md5)?;
    let num_parts = pak_read_u16(reader)?;

    if num_parts == 0 {
        return Err(PakError::invalid_format(
            "pak header declared zero archive parts",
        ));
    }

    let header = RawPackageHeader {
        version: PakVersion::Bg3Current,
        flags,
        file_list_offset,
        file_list_size,
        file_count: 0,
        num_parts,
        data_offset: CURRENT_BG3_DATA_OFFSET,
        priority,
    };

    let entries = parse_file_table(reader, source_len, &header)?;

    Ok(RawPackageMetadata {
        header: RawPackageHeader {
            file_count: entries.len(),
            ..header
        },
        entries,
    })
}

fn parse_file_table<R: Read + Seek>(
    reader: &mut R,
    source_len: u64,
    header: &RawPackageHeader,
) -> PakResult<Vec<RawFileEntry>> {
    let file_table_prefix_len = 8_u64;
    let start = header.file_list_offset;
    let end = start
        .checked_add(file_table_prefix_len)
        .ok_or_else(|| PakError::bounds_violation(start, file_table_prefix_len, source_len))?;
    if end > source_len {
        return Err(PakError::bounds_violation(
            start,
            file_table_prefix_len,
            source_len,
        ));
    }

    reader.seek(SeekFrom::Start(start))?;
    let file_count = pak_read_u32(reader)? as usize;
    if file_count > MAX_PAK_FILE_COUNT {
        return Err(PakError::size_limit_exceeded(
            "pak file count",
            file_count as u64,
            MAX_PAK_FILE_COUNT as u64,
        ));
    }

    let compressed_size = pak_read_u32(reader)? as usize;
    //log::info!("{:?}", compressed_size);
    if compressed_size > MAX_FILE_LIST_COMPRESSED_BYTES {
        return Err(PakError::size_limit_exceeded(
            "compressed pak file table",
            compressed_size as u64,
            MAX_FILE_LIST_COMPRESSED_BYTES as u64,
        ));
    }

    let compressed_start = start + file_table_prefix_len;
    let compressed_end = compressed_start
        .checked_add(compressed_size as u64)
        .ok_or_else(|| {
            PakError::bounds_violation(compressed_start, compressed_size as u64, source_len)
        })?;
    if compressed_end > source_len {
        return Err(PakError::bounds_violation(
            compressed_start,
            compressed_size as u64,
            source_len,
        ));
    }

    let expected_decompressed_size =
        file_count.checked_mul(FILE_ENTRY18_SIZE).ok_or_else(|| {
            PakError::size_limit_exceeded(
                "decompressed pak file table",
                u64::MAX,
                MAX_FILE_LIST_DECOMPRESSED_BYTES as u64,
            )
        })?;
    if expected_decompressed_size > MAX_FILE_LIST_DECOMPRESSED_BYTES {
        return Err(PakError::size_limit_exceeded(
            "decompressed pak file table",
            expected_decompressed_size as u64,
            MAX_FILE_LIST_DECOMPRESSED_BYTES as u64,
        ));
    }

    let mut compressed = vec![0_u8; compressed_size];
    reader.read_exact(&mut compressed)?;
    let decompressed = compression::decompress_lz4_block_bytes(
        &compressed,
        expected_decompressed_size,
        "pak file table",
    )?;
    if decompressed.len() != expected_decompressed_size {
        return Err(PakError::invalid_format(format!(
            "decompressed file table size mismatch: expected {}, got {}",
            expected_decompressed_size,
            decompressed.len()
        )));
    }

    let mut entries = Vec::with_capacity(file_count);
    let mut cursor = Cursor::new(decompressed);
    for _ in 0..file_count {
        entries.push(parse_file_entry(&mut cursor, source_len, header)?);
    }

    //for entry in &entries {
    //    log::info!("{:?}\n", entry.path);
    //}

    Ok(entries)
}

fn parse_file_entry<R: Read>(
    reader: &mut R,
    metadata_source_len: u64,
    header: &RawPackageHeader,
) -> PakResult<RawFileEntry> {
    let mut name_buf = [0_u8; FILE_NAME_LEN];
    reader.read_exact(&mut name_buf)?;
    let offset_low = pak_read_u32(reader)? as u64;
    let offset_high = pak_read_u16(reader)? as u64;
    let archive_part = pak_read_u8(reader)? as u16;
    let compression_flags = pak_read_u8(reader)?;
    let size_on_disk = pak_read_u32(reader)? as u64;
    let uncompressed_size = pak_read_u32(reader)? as u64;

    if archive_part >= header.num_parts {
        return Err(PakError::invalid_format(format!(
            "entry references missing archive part {} of {}",
            archive_part, header.num_parts
        )));
    }

    let path_end = name_buf
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(FILE_NAME_LEN);
    if path_end == 0 {
        return Err(PakError::invalid_format("entry path is empty"));
    }
    if path_end > MAX_ENTRY_PATH_LEN {
        return Err(PakError::size_limit_exceeded(
            "pak entry path length",
            path_end as u64,
            MAX_ENTRY_PATH_LEN as u64,
        ));
    }

    let raw_path = std::str::from_utf8(&name_buf[..path_end])
        .map_err(|err| PakError::invalid_format(format!("entry path is not valid UTF-8: {err}")))?;
    let path = PakPath::parse(raw_path)?;

    let offset = offset_low | (offset_high << 32);
    let deleted = (offset & 0x0000_FFFF_FFFF_FFFF) == DELETION_OFFSET_SENTINEL;

    let compression = PakCompression::from_method_bits(compression_flags);
    let mut flags = PakEntryFlags::default();
    if deleted {
        flags = flags.with(PakEntryFlags::DELETION);
    }
    if header.flags.contains(PakPackageFlags::SOLID) {
        flags = flags.with(PakEntryFlags::SOLID);
    }

    if !deleted && archive_part == 0 {
        let end = offset
            .checked_add(size_on_disk)
            .ok_or_else(|| PakError::bounds_violation(offset, size_on_disk, metadata_source_len))?;
        if end > metadata_source_len {
            return Err(PakError::bounds_violation(
                offset,
                size_on_disk,
                metadata_source_len,
            ));
        }
        if offset < header.data_offset {
            return Err(PakError::invalid_format(format!(
                "entry offset {} precedes data offset {}",
                offset, header.data_offset
            )));
        }
    }

    Ok(RawFileEntry {
        path,
        archive_part,
        offset,
        size_on_disk,
        uncompressed_size,
        compression,
        flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synthetic_current_bg3_package() {
        let package = build_test_package(
            "Public/Test/Example.txt",
            b"hello world",
            PakCompression::None,
            PakPackageFlags::empty(),
        );
        let mut cursor = Cursor::new(package);
        let source_len = cursor.get_ref().len() as u64;
        let metadata = parse_from_reader(&mut cursor, source_len).unwrap();

        assert_eq!(metadata.header.version, PakVersion::Bg3Current);
        assert_eq!(metadata.header.file_count, 1);
        assert_eq!(metadata.header.num_parts, 1);

        let entry = &metadata.entries[0];
        assert_eq!(entry.path.as_str(), "Public/Test/Example.txt");
        assert_eq!(entry.offset, CURRENT_BG3_DATA_OFFSET);
        assert_eq!(entry.size_on_disk, 11);
        assert_eq!(entry.uncompressed_size, 0);
        assert_eq!(entry.compression, PakCompression::None);
        assert!(!entry.flags.contains(PakEntryFlags::DELETION));
    }

    #[test]
    fn rejects_invalid_signature() {
        let mut bytes = vec![0_u8; CURRENT_BG3_DATA_OFFSET as usize];
        bytes[0..4].copy_from_slice(b"NOPE");
        let mut cursor = Cursor::new(bytes);
        let source_len = cursor.get_ref().len() as u64;

        assert!(matches!(
            parse_from_reader(&mut cursor, source_len),
            Err(PakError::InvalidFormat(_))
        ));
    }

    #[test]
    fn rejects_out_of_range_archive_part() {
        let mut package = build_test_package(
            "Public/Test/Example.txt",
            b"hello",
            PakCompression::Lz4,
            PakPackageFlags::empty(),
        );
        let file_list_offset = u64::from_le_bytes(package[8..16].try_into().unwrap()) as usize;
        let compressed_size = u32::from_le_bytes(
            package[file_list_offset + 4..file_list_offset + 8]
                .try_into()
                .unwrap(),
        ) as usize;
        let compressed_start = file_list_offset + 8;
        let compressed_end = compressed_start + compressed_size;
        let decompressed = compression::decompress_lz4_block_bytes(
            &package[compressed_start..compressed_end],
            FILE_ENTRY18_SIZE,
            "test file table",
        )
        .unwrap();
        let mut mutated = decompressed;
        mutated[256 + 4 + 2] = 1;

        let recompressed = lz4_flex::block::compress(&mutated);
        package[file_list_offset + 4..file_list_offset + 8]
            .copy_from_slice(&(recompressed.len() as u32).to_le_bytes());
        package.splice(compressed_start..compressed_end, recompressed);

        let mut cursor = Cursor::new(package);
        let source_len = cursor.get_ref().len() as u64;
        assert!(matches!(
            parse_from_reader(&mut cursor, source_len),
            Err(PakError::InvalidFormat(_))
        ));
    }

    fn build_test_package(
        path: &str,
        body: &[u8],
        compression: PakCompression,
        package_flags: PakPackageFlags,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LSPK");
        bytes.extend_from_slice(&CURRENT_BG3_VERSION_VALUE.to_le_bytes());

        let file_list_offset_placeholder = bytes.len();
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        let file_list_size_placeholder = bytes.len();
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.push(package_flags.bits() as u8);
        bytes.push(0_u8);
        bytes.extend_from_slice(&[0_u8; 16]);
        bytes.extend_from_slice(&1_u16.to_le_bytes());

        assert_eq!(bytes.len(), CURRENT_BG3_DATA_OFFSET as usize);

        let entry_offset = bytes.len() as u64;
        bytes.extend_from_slice(body);

        let mut entry = vec![0_u8; FILE_ENTRY18_SIZE];
        let path_bytes = path.as_bytes();
        entry[..path_bytes.len()].copy_from_slice(path_bytes);
        entry[256..260].copy_from_slice(&(entry_offset as u32).to_le_bytes());
        entry[260..262].copy_from_slice(&((entry_offset >> 32) as u16).to_le_bytes());
        entry[262] = 0;
        entry[263] = match compression {
            PakCompression::None => 0,
            PakCompression::Zlib => 1,
            PakCompression::Lz4 => 2,
            PakCompression::Zstd => 3,
            PakCompression::Unknown(value) => value as u8,
        };
        entry[264..268].copy_from_slice(&(body.len() as u32).to_le_bytes());
        entry[268..272].copy_from_slice(
            &(if compression == PakCompression::None {
                0
            } else {
                body.len() as u32
            })
            .to_le_bytes(),
        );

        let compressed_file_list = lz4_flex::block::compress(&entry);

        let file_list_offset = bytes.len() as u64;
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&(compressed_file_list.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&compressed_file_list);

        let file_list_size = (bytes.len() as u64 - file_list_offset) as u32;
        bytes[file_list_offset_placeholder..file_list_offset_placeholder + 8]
            .copy_from_slice(&file_list_offset.to_le_bytes());
        bytes[file_list_size_placeholder..file_list_size_placeholder + 4]
            .copy_from_slice(&file_list_size.to_le_bytes());

        bytes
    }
}
