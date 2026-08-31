use crate::blocking;
use crate::error::AppError;
use std::fs;
use std::path::PathBuf;

/// Create an empty file on disk within a mod directory.
/// Creates parent directories as needed. Path traversal is blocked.
#[tauri::command]
pub async fn cmd_touch_file(mod_path: String, rel_path: String) -> Result<(), AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        let full = root.join(&rel_path);
        // Security: ensure path stays within mod directory
        let canon_root = root
            .canonicalize()
            .map_err(|e| format!("Invalid mod path: {e}"))?;
        // Create parent directories
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create directories: {e}"))?;
        }
        // Write file first (creates it on disk)
        fs::write(&full, "").map_err(|e| format!("Failed to create file: {e}"))?;
        // Canonicalize after creation and verify bounds
        let canon_full = full
            .canonicalize()
            .map_err(|e| format!("Failed to resolve path: {e}"))?;
        if !canon_full.starts_with(&canon_root) {
            // Clean up the file we just created outside bounds
            let _ = fs::remove_file(&full);
            return Err("Path traversal detected".into());
        }
        Ok(())
    })
    .await
}

/// Create a directory within a mod directory.
/// Creates parent directories as needed. Path traversal is blocked.
#[tauri::command]
pub async fn cmd_create_mod_directory(mod_path: String, rel_path: String) -> Result<(), AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        let full = root.join(&rel_path);
        let canon_root = root
            .canonicalize()
            .map_err(|e| format!("Invalid mod path: {e}"))?;
        fs::create_dir_all(&full).map_err(|e| format!("Failed to create directory: {e}"))?;
        let canon_full = full
            .canonicalize()
            .map_err(|e| format!("Invalid path: {e}"))?;
        if !canon_full.starts_with(&canon_root) {
            let _ = fs::remove_dir_all(&full);
            return Err("Path traversal detected".into());
        }
        Ok(())
    })
    .await
}

/// Move (rename) a file or directory within a mod directory.
/// Both source and destination must remain within the mod root.
#[tauri::command]
pub async fn cmd_move_mod_file(
    mod_path: String,
    src_rel_path: String,
    dest_rel_path: String,
) -> Result<(), AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        let src = root.join(&src_rel_path);
        let dest = root.join(&dest_rel_path);
        let canon_root = root
            .canonicalize()
            .map_err(|e| format!("Invalid mod path: {e}"))?;
        // Verify source is within bounds
        let canon_src = src
            .canonicalize()
            .map_err(|e| format!("Source not found: {e}"))?;
        if !canon_src.starts_with(&canon_root) {
            return Err("Path traversal detected on source".into());
        }
        // Create dest parent dirs
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Create dirs: {e}"))?;
        }
        fs::rename(&src, &dest).map_err(|e| format!("Move file: {e}"))?;
        // Verify dest is in bounds
        let canon_dest = dest
            .canonicalize()
            .map_err(|e| format!("Invalid dest path: {e}"))?;
        if !canon_dest.starts_with(&canon_root) {
            // Attempt to undo
            let _ = fs::rename(&dest, &src);
            return Err("Path traversal detected on destination".into());
        }
        Ok(())
    })
    .await
}

/// Copy a file or directory within a mod directory.
/// Both source and destination must remain within the mod root.
#[tauri::command]
pub async fn cmd_copy_mod_file(
    mod_path: String,
    src_rel_path: String,
    dest_rel_path: String,
) -> Result<(), AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        let src = root.join(&src_rel_path);
        let dest = root.join(&dest_rel_path);
        let canon_root = root
            .canonicalize()
            .map_err(|e| format!("Invalid mod path: {e}"))?;
        // Verify source is within bounds
        let canon_src = src
            .canonicalize()
            .map_err(|e| format!("Source not found: {e}"))?;
        if !canon_src.starts_with(&canon_root) {
            return Err("Path traversal detected on source".into());
        }
        // Create dest parent dirs
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Create dirs: {e}"))?;
        }
        if src.is_dir() {
            copy_dir_recursive(&src, &dest)?;
        } else {
            fs::copy(&src, &dest).map_err(|e| format!("Copy file: {e}"))?;
        }
        // Verify dest is in bounds
        let canon_dest = dest
            .canonicalize()
            .map_err(|e| format!("Invalid dest path: {e}"))?;
        if !canon_dest.starts_with(&canon_root) {
            return Err("Path traversal detected on destination".into());
        }
        Ok(())
    })
    .await
}

/// Delete a file or directory within a mod directory.
/// Path traversal is blocked. Cannot delete the mod root itself.
#[tauri::command]
pub async fn cmd_delete_mod_path(mod_path: String, rel_path: String) -> Result<(), AppError> {
    blocking(move || {
        let root = PathBuf::from(&mod_path);
        let full = root.join(&rel_path);
        let canon_root = root
            .canonicalize()
            .map_err(|e| format!("Invalid mod path: {e}"))?;
        let canon_full = full
            .canonicalize()
            .map_err(|e| format!("Path not found: {e}"))?;
        if !canon_full.starts_with(&canon_root) {
            return Err("Path traversal detected".into());
        }
        if canon_full == canon_root {
            return Err("Cannot delete mod root directory".into());
        }
        if full.is_dir() {
            fs::remove_dir_all(&full).map_err(|e| format!("Failed to delete directory: {e}"))?;
        } else if full.is_file() {
            fs::remove_file(&full).map_err(|e| format!("Failed to delete file: {e}"))?;
        } else {
            return Err("Path does not exist".into());
        }
        Ok(())
    })
    .await
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("Create dir: {e}"))?;
    for entry in fs::read_dir(src).map_err(|e| format!("Read dir: {e}"))? {
        let entry = entry.map_err(|e| format!("Dir entry: {e}"))?;
        let file_type = entry.file_type().map_err(|e| format!("File type: {e}"))?;
        let dest_child = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest_child)?;
        } else {
            fs::copy(entry.path(), &dest_child).map_err(|e| format!("Copy file: {e}"))?;
        }
    }
    Ok(())
}
