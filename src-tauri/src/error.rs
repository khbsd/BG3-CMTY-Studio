use std::collections::HashMap;

use serde::Serialize;
#[cfg(test)]
use ts_rs::TS;

use crate::pak::PakError;

/// Classifies the kind of error for programmatic frontend handling.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub enum ErrorKind {
    NotFound,
    InvalidInput,
    IoError,
    ParseError,
    CacheError,
    SecurityViolation,
    TaskPanicked,
    Timeout,
    Internal,
}

/// Structured, serializable error type for IPC commands.
///
/// Frontend receives `{ kind: "NotFound", message: "..." }` and can
/// distinguish error categories programmatically instead of parsing strings.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct AppError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<HashMap<String, String>>,
}

impl AppError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::NotFound,
            message: message.into(),
            context: None,
        }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidInput,
            message: message.into(),
            context: None,
        }
    }

    pub fn io_error(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::IoError,
            message: message.into(),
            context: None,
        }
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::ParseError,
            message: message.into(),
            context: None,
        }
    }

    pub fn cache_error(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::CacheError,
            message: message.into(),
            context: None,
        }
    }

    pub fn security(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::SecurityViolation,
            message: message.into(),
            context: None,
        }
    }

    pub fn task_panicked(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::TaskPanicked,
            message: message.into(),
            context: None,
        }
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Timeout,
            message: message.into(),
            context: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: message.into(),
            context: None,
        }
    }

    pub fn with_context(mut self, key: &str, value: impl Into<String>) -> Self {
        self.context
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value.into());
        self
    }
}

/// Enables `?` operator on `Result<T, String>` inside functions returning `Result<T, AppError>`.
impl From<String> for AppError {
    fn from(message: String) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message,
            context: None,
        }
    }
}

impl From<&str> for AppError {
    fn from(message: &str) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: message.to_string(),
            context: None,
        }
    }
}

impl From<PakError> for AppError {
    fn from(error: PakError) -> Self {
        match error {
            PakError::Io(err) => Self::io_error(err.to_string()),
            PakError::InvalidPath(message) => Self::invalid_input(message),
            PakError::InvalidFormat(message) => Self::parse_error(message),
            PakError::UnsupportedVersion(version) => {
                Self::parse_error(format!("Unsupported pak version: {version}"))
            }
            PakError::BoundsViolation {
                start,
                len,
                source_len,
            } => Self::parse_error(format!(
                "Pak read exceeds bounds (start={start}, len={len}, source_len={source_len})"
            )),
            PakError::SizeLimitExceeded {
                context,
                size,
                limit,
            } => Self::parse_error(format!(
                "Pak size limit exceeded for {context} (size={size}, limit={limit})"
            )),
            PakError::NotFound(message) => Self::not_found(message),
            PakError::DeletedEntry(path) => {
                Self::invalid_input(format!("Pak entry is marked deleted: {path}"))
            }
            PakError::Decompression(message) => Self::parse_error(message),
            PakError::NotImplemented(message) => {
                Self::internal(format!("Pak feature not implemented: {message}"))
            }
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AppError {}
