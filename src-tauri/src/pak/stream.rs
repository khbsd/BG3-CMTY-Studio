use std::fs::File;
use std::io::{Error, ErrorKind, Read, Seek, SeekFrom};

use crate::pak::error::{PakError, PakResult};

/**
 * this crate seems to be exclusively about reading bytes from streams
 */

pub struct BoundedReader<R> {
    inner: R,
    start: u64,
    end: u64,
    len: u64,
    position: u64,
    source_len: u64,
}

impl<R: Read + Seek> BoundedReader<R> {
    pub fn new(mut inner: R, start: u64, len: u64) -> PakResult<Self> {
        let source_len = inner.seek(SeekFrom::End(0))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| PakError::bounds_violation(start, len, source_len))?;
        if end > source_len {
            return Err(PakError::bounds_violation(start, len, source_len));
        }

        inner.seek(SeekFrom::Start(start))?;
        Ok(Self {
            inner,
            start,
            end,
            len,
            position: 0,
            source_len,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn remaining(&self) -> u64 {
        self.len.saturating_sub(self.position)
    }

    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn source_len(&self) -> u64 {
        self.source_len
    }

    pub fn source_position(&self) -> u64 {
        self.start + self.position
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    pub fn read_remaining_to_vec(&mut self, limit: usize) -> PakResult<Vec<u8>> {
        let remaining = self.remaining();
        if remaining > limit as u64 {
            return Err(PakError::size_limit_exceeded(
                "pak entry materialization",
                remaining,
                limit as u64,
            ));
        }

        let mut bytes = Vec::with_capacity(remaining as usize);
        self.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn seek_to(&mut self, next_position: u64) -> std::io::Result<u64> {
        if next_position > self.len {
            return Err(invalid_seek(next_position, self.len));
        }

        let absolute = self.start.checked_add(next_position).ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "seek overflow for bounded reader (start={}, next_position={})",
                    self.start, next_position
                ),
            )
        })?;

        self.inner.seek(SeekFrom::Start(absolute))?;
        self.position = next_position;
        Ok(self.position)
    }
}

impl<R: Read + Seek> Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.len {
            return Ok(0);
        }

        let max_read = self.remaining().min(buf.len() as u64) as usize;
        let bytes_read = self.inner.read(&mut buf[..max_read])?;
        self.position += bytes_read as u64;
        Ok(bytes_read)
    }
}

impl<R: Read + Seek> Seek for BoundedReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let next_position = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(delta) => offset_from_signed(self.position, delta, self.len)?,
            SeekFrom::End(delta) => offset_from_signed(self.len, delta, self.len)?,
        };

        self.seek_to(next_position)
    }
}

pub enum PakEntryReader {
    Raw(BoundedReader<File>),
    Decompressed(Box<dyn Read + Send>),
}

impl PakEntryReader {
    pub fn read_to_end_with_limit(&mut self, limit: usize) -> PakResult<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 8192];

        loop {
            let bytes_read = self.read(&mut chunk)?;
            if bytes_read == 0 {
                break;
            }

            let next_len = bytes.len().checked_add(bytes_read).ok_or_else(|| {
                PakError::size_limit_exceeded("pak entry materialization", u64::MAX, limit as u64)
            })?;
            if next_len > limit {
                return Err(PakError::size_limit_exceeded(
                    "pak entry materialization",
                    next_len as u64,
                    limit as u64,
                ));
            }

            bytes.extend_from_slice(&chunk[..bytes_read]);
        }

        Ok(bytes)
    }
}

impl Read for PakEntryReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Raw(reader) => reader.read(buf),
            Self::Decompressed(reader) => reader.read(buf),
        }
    }
}

fn offset_from_signed(base: u64, delta: i64, len: u64) -> std::io::Result<u64> {
    let next = i128::from(base) + i128::from(delta);
    if next < 0 || next > i128::from(len) {
        return Err(invalid_seek_display(base, delta, len));
    }

    Ok(next as u64)
}

fn invalid_seek(position: u64, len: u64) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("seek outside bounded reader range (position={position}, len={len})"),
    )
}

fn invalid_seek_display(base: u64, delta: i64, len: u64) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("seek outside bounded reader range (base={base}, delta={delta}, len={len})"),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn bounded_reader_reads_only_requested_range() {
        let data = Cursor::new(b"0123456789abcdef".to_vec());
        let mut reader = BoundedReader::new(data, 4, 6).unwrap();
        let mut buffer = [0_u8; 8];

        let bytes_read = reader.read(&mut buffer).unwrap();

        assert_eq!(bytes_read, 6);
        assert_eq!(&buffer[..6], b"456789");
        assert_eq!(reader.position(), 6);
        assert_eq!(reader.remaining(), 0);
        assert_eq!(reader.source_position(), 10);
        assert_eq!(reader.read(&mut buffer).unwrap(), 0);
    }

    #[test]
    fn bounded_reader_supports_seek_within_range() {
        let data = Cursor::new(b"abcdefghij".to_vec());
        let mut reader = BoundedReader::new(data, 2, 5).unwrap();
        let mut buffer = [0_u8; 2];

        reader.seek(SeekFrom::Start(1)).unwrap();
        reader.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"de");

        reader.seek(SeekFrom::Current(-1)).unwrap();
        reader.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"ef");

        reader.seek(SeekFrom::End(-2)).unwrap();
        reader.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"fg");
    }

    #[test]
    fn bounded_reader_rejects_out_of_bounds_ranges_and_seeks() {
        let data = Cursor::new(b"abcdef".to_vec());
        assert!(matches!(
            BoundedReader::new(data, 3, 8),
            Err(PakError::BoundsViolation { .. })
        ));

        let data = Cursor::new(b"abcdef".to_vec());
        let mut reader = BoundedReader::new(data, 1, 3).unwrap();
        assert!(reader.seek(SeekFrom::Start(4)).is_err());
        assert!(reader.seek(SeekFrom::Current(-2)).is_err());
        assert!(reader.seek(SeekFrom::End(1)).is_err());
    }

    #[test]
    fn bounded_reader_materialization_respects_limit() {
        let data = Cursor::new(b"abcdefghij".to_vec());
        let mut reader = BoundedReader::new(data, 2, 4).unwrap();

        assert_eq!(reader.read_remaining_to_vec(4).unwrap(), b"cdef");

        let data = Cursor::new(b"abcdefghij".to_vec());
        let mut reader = BoundedReader::new(data, 2, 4).unwrap();
        assert!(matches!(
            reader.read_remaining_to_vec(3),
            Err(PakError::SizeLimitExceeded { .. })
        ));
    }

    #[test]
    fn pak_entry_reader_materialization_respects_limit() {
        let mut reader = PakEntryReader::Decompressed(Box::new(Cursor::new(b"hello".to_vec())));
        assert_eq!(reader.read_to_end_with_limit(5).unwrap(), b"hello");

        let mut reader = PakEntryReader::Decompressed(Box::new(Cursor::new(b"hello".to_vec())));
        assert!(matches!(
            reader.read_to_end_with_limit(4),
            Err(PakError::SizeLimitExceeded { .. })
        ));
    }
}
