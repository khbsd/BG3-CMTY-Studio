//! Centralized IPC boundary validation.
//!
//! All input validation for Tauri commands lives here. This makes security
//! review a single-file check. Functions validate paths, filenames, folder
//! names, and file sizes before any I/O occurs.

pub mod custom_linters;
pub mod mcm_blueprint;
pub mod osiris;
pub mod se_config;

use crate::error::AppError;
use std::path::Path;

// ── File size limits (SEC-02) ──

pub const MAX_CONFIG_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
pub const MAX_LSX_FILE_SIZE: u64 = 100 * 1024 * 1024; // 100 MB
pub const MAX_LOCA_FILE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB
pub const MAX_STATS_FILE_SIZE: u64 = 20 * 1024 * 1024; // 20 MB
pub const MAX_MOD_FILE_SIZE: u64 = 2 * 1024 * 1024; // 2 MB

/// Check that a file does not exceed the given size limit.
pub fn check_file_size(path: &Path, max_bytes: u64) -> Result<(), String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("Cannot stat {}: {}", path.display(), e))?;
    if meta.len() > max_bytes {
        return Err(format!(
            "File {} is {} bytes, exceeding the {} byte limit",
            path.display(),
            meta.len(),
            max_bytes
        ));
    }
    Ok(())
}

// ── Folder name validation (extracted from cmd_get_entries_by_folder) ──

/// Validate a folder name received from the frontend.
/// Rejects path traversal, null bytes, absolute paths, separators, and drive letters.
pub fn validate_folder_name(name: &str) -> Result<(), AppError> {
    if name.contains("..") {
        return Err(AppError::security(
            "Invalid folder name: path traversal not allowed",
        ));
    }
    if name.contains('\0') {
        return Err(AppError::security(
            "Invalid folder name: null bytes not allowed",
        ));
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(AppError::security(
            "Invalid folder name: absolute paths not allowed",
        ));
    }
    if name.contains('\\') || name.contains('/') {
        return Err(AppError::security(
            "Invalid folder name: path separators not allowed",
        ));
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return Err(AppError::security(
            "Invalid folder name: absolute paths not allowed",
        ));
    }
    Ok(())
}
