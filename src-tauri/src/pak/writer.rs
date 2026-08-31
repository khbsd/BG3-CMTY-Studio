use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

use crate::pak::compression;
use crate::pak::error::{PakError, PakResult};
use crate::pak::format::{CompressionLevel, PakCompression, PakPackageFlags};
use crate::pak::path::PakPath;

const PACKAGE_SIGNATURE: u32 = 0x4B50_534C;
const HEADER16_SIZE: usize = 36;
const FILE_ENTRY18_SIZE: usize = 272;
const FILE_NAME_LEN: usize = 256;
const DATA_SECTION_START: u64 = 4 + HEADER16_SIZE as u64; // 40
const PAD_BYTE: u8 = 0xAD;

/// Extensions that should NOT be compressed (per LSLib convention).
const SKIP_COMPRESSION_EXTENSIONS: &[&str] = &["gts", "gtp", "wem", "bnk", "bk2"];

pub struct PakWriterOptions {
    pub compression: PakCompression,
    pub compression_level: CompressionLevel,
    pub priority: u8,
    pub flags: PakPackageFlags,
}

impl Default for PakWriterOptions {
    fn default() -> Self {
        Self {
            compression: PakCompression::Lz4,
            compression_level: CompressionLevel::Default,
            priority: 0,
            flags: PakPackageFlags::empty(),
        }
    }
}

pub struct PakWriter {
    output: BufWriter<File>,
    entries: Vec<PendingEntry>,
    md5_hasher: Md5,
    position: u64,
    options: PakWriterOptions,
    output_path: PathBuf,
}

struct PendingEntry {
    pak_path: PakPath,
    offset: u64,
    size_on_disk: u64,
    uncompressed_size: u64,
    compression: PakCompression,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PakWriteResult {
    pub output_path: String,
    pub file_count: usize,
    pub total_bytes: u64,
}

impl PakWriter {
    /// Create a new writer; writes signature + placeholder header immediately.
    pub fn new(output_path: &Path, options: PakWriterOptions) -> PakResult<Self> {
        let file = File::create(output_path)?;
        let mut output = BufWriter::new(file);

        // Write signature
        output.write_all(&PACKAGE_SIGNATURE.to_le_bytes())?;

        // Write 36-byte placeholder header (filled during finish())
        output.write_all(&[0u8; HEADER16_SIZE])?;

        Ok(Self {
            output,
            entries: Vec::new(),
            md5_hasher: Md5::new(),
            position: DATA_SECTION_START,
            options,
            output_path: output_path.to_path_buf(),
        })
    }

    /// Add a file from an in-memory buffer using archive-level compression.
    pub fn add_bytes(&mut self, pak_path: &PakPath, data: &[u8]) -> PakResult<()> {
        let method = self.effective_compression(pak_path, data);
        let level = self.options.compression_level;
        self.add_bytes_with(pak_path, data, method, level)
    }

    /// Add a file from an in-memory buffer with explicit compression override.
    pub fn add_bytes_with(
        &mut self,
        pak_path: &PakPath,
        data: &[u8],
        compression: PakCompression,
        level: CompressionLevel,
    ) -> PakResult<()> {
        // Feed uncompressed data into MD5 hasher
        self.md5_hasher.update(data);

        // Compress
        let (written_bytes, final_compression, uncompressed_size) =
            if matches!(compression, PakCompression::None) || data.is_empty() {
                (data.to_vec(), PakCompression::None, 0u64)
            } else {
                let compressed = compression::compress_bytes(data, compression, level)?;
                // Compression fallback: if compressed >= original, store as None
                if compressed.len() >= data.len() {
                    (data.to_vec(), PakCompression::None, 0u64)
                } else {
                    (compressed, compression, data.len() as u64)
                }
            };

        let offset = self.position;
        let size_on_disk = written_bytes.len() as u64;

        // Write compressed (or raw) bytes
        self.output.write_all(&written_bytes)?;
        self.position += size_on_disk;

        // Write 0xAD padding to next 64-byte boundary
        let pad = padding_needed(self.position);
        if pad > 0 {
            let pad_buf = vec![PAD_BYTE; pad];
            self.output.write_all(&pad_buf)?;
            self.position += pad as u64;
        }

        // Record entry
        self.entries.push(PendingEntry {
            pak_path: pak_path.clone(),
            offset,
            size_on_disk,
            uncompressed_size,
            compression: final_compression,
        });

        Ok(())
    }

    /// Add a file by reading from disk.
    pub fn add_file(&mut self, disk_path: &Path, pak_path: &PakPath) -> PakResult<()> {
        let data = std::fs::read(disk_path).map_err(|e| {
            PakError::Io(std::io::Error::new(
                e.kind(),
                format!("reading {}: {}", disk_path.display(), e),
            ))
        })?;
        self.add_bytes(pak_path, &data)
    }

    /// Finalize: write compressed file list, compute MD5, seek back and write real header.
    pub fn finish(mut self) -> PakResult<PakWriteResult> {
        let file_count = self.entries.len();
        let file_list_offset = self.position;

        // Serialize all entries into flat FileEntry18 buffer
        let mut flat_buf = Vec::with_capacity(file_count * FILE_ENTRY18_SIZE);
        for entry in &self.entries {
            flat_buf.extend_from_slice(&serialize_entry(entry));
        }

        // LZ4-compress the file list (always LZ4 for V18)
        let compressed_list = compression::compress_lz4_block_bytes(&flat_buf);

        // Write: file_count (u32LE) + compressed_size (u32LE) + compressed data
        self.output.write_all(&(file_count as u32).to_le_bytes())?;
        self.output
            .write_all(&(compressed_list.len() as u32).to_le_bytes())?;
        self.output.write_all(&compressed_list)?;

        let file_list_size = 4 + 4 + compressed_list.len() as u32;

        // Finalize MD5 with Larian +1 wrapping quirk
        let md5_raw = self.md5_hasher.finalize();
        let mut md5_bytes = [0u8; 16];
        for (i, &b) in md5_raw.iter().enumerate() {
            md5_bytes[i] = b.wrapping_add(1);
        }

        // Seek to offset 4 and write real header
        self.output.seek(SeekFrom::Start(4))?;

        // version = 18
        self.output.write_all(&18u32.to_le_bytes())?;
        // file_list_offset
        self.output.write_all(&file_list_offset.to_le_bytes())?;
        // file_list_size
        self.output.write_all(&file_list_size.to_le_bytes())?;
        // flags
        self.output.write_all(&[self.options.flags.bits() as u8])?;
        // priority
        self.output.write_all(&[self.options.priority])?;
        // md5
        self.output.write_all(&md5_bytes)?;
        // num_parts = 1
        self.output.write_all(&1u16.to_le_bytes())?;

        self.output.flush()?;

        let total_bytes = file_list_offset + file_list_size as u64;

        Ok(PakWriteResult {
            output_path: self.output_path.to_string_lossy().into_owned(),
            file_count,
            total_bytes,
        })
    }

    /// Determine effective compression for a file based on extension skip-list.
    fn effective_compression(&self, pak_path: &PakPath, data: &[u8]) -> PakCompression {
        if data.is_empty() {
            return PakCompression::None;
        }
        if let Some(ext) = pak_path.extension() {
            let lower = ext.to_ascii_lowercase();
            if SKIP_COMPRESSION_EXTENSIONS.iter().any(|&e| e == lower) {
                return PakCompression::None;
            }
        }
        self.options.compression
    }
}

/// Calculate padding needed to reach the next 64-byte boundary.
fn padding_needed(position: u64) -> usize {
    let align_base = position - DATA_SECTION_START;
    let remainder = (align_base % 64) as usize;
    if remainder == 0 {
        0
    } else {
        64 - remainder
    }
}

/// Serialize a PendingEntry into a 272-byte FileEntry18 buffer.
fn serialize_entry(entry: &PendingEntry) -> [u8; FILE_ENTRY18_SIZE] {
    let mut buf = [0u8; FILE_ENTRY18_SIZE];

    // Name: null-terminated UTF-8, max 256 bytes
    let name_bytes = entry.pak_path.as_str().as_bytes();
    let copy_len = name_bytes.len().min(FILE_NAME_LEN - 1);
    buf[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

    // Offset (48-bit split)
    let offset = entry.offset;
    buf[256..260].copy_from_slice(&(offset as u32).to_le_bytes());
    buf[260..262].copy_from_slice(&((offset >> 32) as u16).to_le_bytes());

    // Archive part = 0 (single-part)
    buf[262] = 0;

    // Compression flags (V18 only stores method bits in low nibble)
    buf[263] = entry.compression.method_bits();

    // Size on disk
    buf[264..268].copy_from_slice(&(entry.size_on_disk as u32).to_le_bytes());

    // Uncompressed size (0 if not compressed)
    let uncomp = if matches!(entry.compression, PakCompression::None) {
        0u32
    } else {
        entry.uncompressed_size as u32
    };
    buf[268..272].copy_from_slice(&uncomp.to_le_bytes());

    buf
}
