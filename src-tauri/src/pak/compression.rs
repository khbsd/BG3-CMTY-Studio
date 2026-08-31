use std::io::{Cursor, Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression as ZlibLevel;

use crate::pak::error::{PakError, PakResult};
use crate::pak::format::{CompressionLevel, PakCompression};

pub fn wrap_reader(
    mut reader: Box<dyn Read + Send>,
    compression: PakCompression,
    expected_size: u64,
) -> PakResult<Box<dyn Read + Send>> {
    match compression {
        PakCompression::None => Ok(reader),
        PakCompression::Zlib => Ok(Box::new(ZlibDecoder::new(reader))),
        PakCompression::Lz4 => {
            let output_size = usize::try_from(expected_size).map_err(|_| {
                PakError::size_limit_exceeded(
                    "lz4 pak entry output",
                    expected_size,
                    usize::MAX as u64,
                )
            })?;
            let mut compressed = Vec::new();
            reader.read_to_end(&mut compressed)?;
            let decompressed = decompress_lz4_block_bytes(&compressed, output_size, "pak entry")?;
            Ok(Box::new(Cursor::new(decompressed)))
        }
        PakCompression::Zstd => Err(PakError::not_implemented(
            "zstd-compressed pak entry decoding is not implemented yet",
        )),
        PakCompression::Unknown(value) => Err(PakError::decompression(format!(
            "unknown compression method: {value}"
        ))),
    }
}

pub fn decompress_lz4_block_bytes(
    compressed: &[u8],
    output_limit: usize,
    context: &'static str,
) -> PakResult<Vec<u8>> {
    lz4_flex::block::decompress(compressed, output_limit).map_err(|err| {
        PakError::decompression(format!("failed to decompress {context} as LZ4 block: {err}"))
    })
}

/// LZ4 block compress `input` bytes.
pub fn compress_lz4_block_bytes(input: &[u8]) -> Vec<u8> {
    lz4_flex::block::compress(input)
}

/// Zlib compress `input` bytes at the given level.
pub fn compress_zlib_bytes(input: &[u8], level: CompressionLevel) -> PakResult<Vec<u8>> {
    let zlib_level = match level {
        CompressionLevel::Fast => ZlibLevel::fast(),
        CompressionLevel::Default => ZlibLevel::default(),
        CompressionLevel::Max => ZlibLevel::best(),
    };
    let mut encoder = ZlibEncoder::new(Vec::new(), zlib_level);
    encoder
        .write_all(input)
        .map_err(|e| PakError::decompression(format!("zlib compress: {e}")))?;
    encoder
        .finish()
        .map_err(|e| PakError::decompression(format!("zlib compress finish: {e}")))
}

/// Compress `input` using the specified method and level.
pub fn compress_bytes(
    input: &[u8],
    method: PakCompression,
    level: CompressionLevel,
) -> PakResult<Vec<u8>> {
    match method {
        PakCompression::None => Ok(input.to_vec()),
        PakCompression::Zlib => compress_zlib_bytes(input, level),
        PakCompression::Lz4 => Ok(compress_lz4_block_bytes(input)),
        PakCompression::Zstd => Err(PakError::not_implemented(
            "zstd compression for pak entries is not yet supported",
        )),
        PakCompression::Unknown(v) => Err(PakError::decompression(format!(
            "unknown compression method {v}"
        ))),
    }
}
