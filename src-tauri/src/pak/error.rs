use std::fmt;

pub type PakResult<T> = Result<T, PakError>;

#[derive(Debug)]
pub enum PakError {
    Io(std::io::Error),
    InvalidPath(String),
    InvalidFormat(String),
    UnsupportedVersion(u32),
    BoundsViolation {
        start: u64,
        len: u64,
        source_len: u64,
    },
    SizeLimitExceeded {
        context: &'static str,
        size: u64,
        limit: u64,
    },
    NotFound(String),
    DeletedEntry(String),
    Decompression(String),
    NotImplemented(&'static str),
}

impl PakError {
    pub fn invalid_path(message: impl Into<String>) -> Self {
        Self::InvalidPath(message.into())
    }

    pub fn invalid_format(message: impl Into<String>) -> Self {
        Self::InvalidFormat(message.into())
    }

    pub fn unsupported_version(version: u32) -> Self {
        Self::UnsupportedVersion(version)
    }

    pub fn bounds_violation(start: u64, len: u64, source_len: u64) -> Self {
        Self::BoundsViolation {
            start,
            len,
            source_len,
        }
    }

    pub fn size_limit_exceeded(context: &'static str, size: u64, limit: u64) -> Self {
        Self::SizeLimitExceeded {
            context,
            size,
            limit,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn deleted_entry(path: impl Into<String>) -> Self {
        Self::DeletedEntry(path.into())
    }

    pub fn decompression(message: impl Into<String>) -> Self {
        Self::Decompression(message.into())
    }

    pub fn not_implemented(feature: &'static str) -> Self {
        Self::NotImplemented(feature)
    }
}

impl fmt::Display for PakError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::InvalidPath(msg) => write!(f, "Invalid pak path: {msg}"),
            Self::InvalidFormat(msg) => write!(f, "Invalid pak format: {msg}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "Unsupported pak version: {version}")
            }
            Self::BoundsViolation {
                start,
                len,
                source_len,
            } => write!(
                f,
                "Pak read exceeds bounds (start={start}, len={len}, source_len={source_len})"
            ),
            Self::SizeLimitExceeded {
                context,
                size,
                limit,
            } => write!(
                f,
                "Pak size limit exceeded for {context} (size={size}, limit={limit})"
            ),
            Self::NotFound(msg) => write!(f, "Pak item not found: {msg}"),
            Self::DeletedEntry(path) => write!(f, "Pak entry is marked deleted: {path}"),
            Self::Decompression(msg) => write!(f, "Pak decompression error: {msg}"),
            Self::NotImplemented(msg) => write!(f, "Pak feature not implemented: {msg}"),
        }
    }
}

impl std::error::Error for PakError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PakError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::io::ErrorKind;

    use super::*;

    #[test]
    fn helper_constructors_build_expected_variants() {
        assert!(matches!(
            PakError::invalid_path("bad path"),
            PakError::InvalidPath(message) if message == "bad path"
        ));
        assert!(matches!(
            PakError::invalid_format("bad format"),
            PakError::InvalidFormat(message) if message == "bad format"
        ));
        assert!(matches!(
            PakError::unsupported_version(18),
            PakError::UnsupportedVersion(18)
        ));
        assert!(matches!(
            PakError::not_found("missing"),
            PakError::NotFound(message) if message == "missing"
        ));
        assert!(matches!(
            PakError::deleted_entry("foo/bar.lsx"),
            PakError::DeletedEntry(path) if path == "foo/bar.lsx"
        ));
        assert!(matches!(
            PakError::decompression("bad zlib stream"),
            PakError::Decompression(message) if message == "bad zlib stream"
        ));
        assert!(matches!(
            PakError::not_implemented("solid decoding"),
            PakError::NotImplemented("solid decoding")
        ));
    }

    #[test]
    fn io_error_preserves_source() {
        let err = PakError::from(std::io::Error::new(ErrorKind::UnexpectedEof, "truncated"));

        assert_eq!(err.to_string(), "I/O error: truncated");
        assert_eq!(
            err.source().map(ToString::to_string).as_deref(),
            Some("truncated")
        );
    }

    #[test]
    fn bounds_and_size_limit_errors_are_descriptive() {
        let bounds = PakError::bounds_violation(8, 16, 20).to_string();
        assert!(bounds.contains("start=8"));
        assert!(bounds.contains("len=16"));
        assert!(bounds.contains("source_len=20"));

        let limit = PakError::size_limit_exceeded("entry materialization", 2048, 1024).to_string();
        assert!(limit.contains("entry materialization"));
        assert!(limit.contains("size=2048"));
        assert!(limit.contains("limit=1024"));
    }
}
