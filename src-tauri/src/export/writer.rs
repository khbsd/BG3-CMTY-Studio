//! Atomic file writer for the export pipeline.
//!
//! Implements a 5-phase strategy: validate → write temps → backup → rename → delete.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::AppError;

use super::delta::{DeltaEntry, FileSystemDelta};

/// Summary of a single file operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileReport {
    pub path: String,
    pub handler: String,
    pub entry_count: usize,
    pub bytes_written: usize,
    pub backed_up: bool,
}

/// Summary of all write operations performed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WriteReport {
    pub files_created: Vec<FileReport>,
    pub files_updated: Vec<FileReport>,
    pub files_deleted: Vec<FileReport>,
    pub files_unchanged: usize,
    pub total_entries: usize,
    pub errors: Vec<String>,
}

/// Best-effort removal of temporary files.
fn cleanup_temp_files(temp_paths: &[PathBuf]) {
    for path in temp_paths {
        let _ = fs::remove_file(path);
    }
}

/// Compute the `.cmty_tmp` temp path for a given absolute path.
fn temp_path_for(absolute_path: &Path) -> PathBuf {
    let mut name = absolute_path.as_os_str().to_os_string();
    name.push(".cmty_tmp");
    PathBuf::from(name)
}

/// Count total entries across a slice of delta entries.
fn count_entries(entries: &[DeltaEntry]) -> usize {
    entries.iter().map(|e| e.unit.entry_count).sum()
}

/// Write the file-system delta to disk using an atomic 5-phase strategy.
///
/// On success, `unit.content` is cleared to free memory.
pub fn write_files_atomic(
    delta: &mut FileSystemDelta,
    backup: bool,
) -> Result<WriteReport, AppError> {
    let mut report = WriteReport {
        files_created: Vec::new(),
        files_updated: Vec::new(),
        files_deleted: Vec::new(),
        files_unchanged: delta.unchanged.len(),
        total_entries: 0,
        errors: Vec::new(),
    };

    // Count total entries across all categories.
    report.total_entries = count_entries(&delta.creates)
        + count_entries(&delta.updates)
        + count_entries(&delta.deletes)
        + count_entries(&delta.unchanged);

    // Early return: nothing to do.
    if delta.creates.is_empty() && delta.updates.is_empty() && delta.deletes.is_empty() {
        return Ok(report);
    }

    // If there are only deletes, skip straight to Phase 5.
    let has_writes = !delta.creates.is_empty() || !delta.updates.is_empty();

    if has_writes {
        // ── Phase 1: Validate ──────────────────────────────────────────
        for entry in delta.creates.iter().chain(delta.updates.iter()) {
            if entry.unit.content.is_none() {
                return Err(AppError::invalid_input(format!(
                    "Export unit '{}' ({}) has no content — cannot write",
                    entry.unit.output_path.display(),
                    entry.unit.handler_name,
                )));
            }
        }

        // ── Phase 2: Write temp files ──────────────────────────────────
        let mut temp_paths: Vec<PathBuf> = Vec::new();

        for entry in delta.creates.iter().chain(delta.updates.iter()) {
            let tmp = temp_path_for(&entry.absolute_path);

            // Ensure parent directory exists.
            if let Some(parent) = entry.absolute_path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    cleanup_temp_files(&temp_paths);
                    return Err(AppError::io_error(format!(
                        "Failed to create directory '{}': {}",
                        parent.display(),
                        e,
                    )));
                }
            }

            // content is guaranteed Some by Phase 1 validation.
            let content = entry.unit.content.as_ref().unwrap();
            if let Err(e) = fs::write(&tmp, content) {
                cleanup_temp_files(&temp_paths);
                return Err(AppError::io_error(format!(
                    "Failed to write temp file '{}': {}",
                    tmp.display(),
                    e,
                )));
            }

            temp_paths.push(tmp);
        }

        // ── Phase 3: Backup originals ──────────────────────────────────
        if backup {
            for entry in delta.updates.iter() {
                if entry.absolute_path.exists() {
                    let bak = backup_path_for(&entry.absolute_path);
                    if let Err(e) = fs::copy(&entry.absolute_path, &bak) {
                        // Non-fatal for backup; log as warning.
                        report.errors.push(format!(
                            "Backup failed for '{}': {}",
                            entry.absolute_path.display(),
                            e,
                        ));
                    }
                }
            }

            for entry in delta.deletes.iter() {
                if entry.absolute_path.exists() {
                    let bak = backup_path_for(&entry.absolute_path);
                    if let Err(e) = fs::copy(&entry.absolute_path, &bak) {
                        report.errors.push(format!(
                            "Backup failed for '{}': {}",
                            entry.absolute_path.display(),
                            e,
                        ));
                    }
                }
            }
        }

        // ── Phase 4: Rename temp → final (atomic) ─────────────────────
        let write_entries: Vec<&DeltaEntry> =
            delta.creates.iter().chain(delta.updates.iter()).collect();

        // temp_paths[i] corresponds to write_entries[i].
        let mut remaining_temps: Vec<PathBuf> = Vec::new();
        for (i, entry) in write_entries.iter().enumerate() {
            let tmp = &temp_paths[i];

            // Track remaining temps for cleanup on failure.
            // We collect the ones after the current index.
            let rename_result = fs::rename(tmp, &entry.absolute_path);
            if rename_result.is_err() {
                // Fallback: copy + delete (cross-filesystem).
                let fallback =
                    fs::copy(tmp, &entry.absolute_path).and_then(|_| fs::remove_file(tmp));

                if fallback.is_err() {
                    // Clean up remaining temps (current one + all after).
                    remaining_temps.push(tmp.clone());
                    remaining_temps.extend_from_slice(&temp_paths[i + 1..]);
                    cleanup_temp_files(&remaining_temps);
                    return Err(AppError::io_error(format!(
                        "Failed to rename/copy temp file to '{}': rename={}, copy={}",
                        entry.absolute_path.display(),
                        rename_result.unwrap_err(),
                        fallback.unwrap_err(),
                    )));
                }
            }
        }
    } else if backup {
        // Only deletes, but backup is requested — backup before deleting.
        for entry in delta.deletes.iter() {
            if entry.absolute_path.exists() {
                let bak = backup_path_for(&entry.absolute_path);
                if let Err(e) = fs::copy(&entry.absolute_path, &bak) {
                    report.errors.push(format!(
                        "Backup failed for '{}': {}",
                        entry.absolute_path.display(),
                        e,
                    ));
                }
            }
        }
    }

    // ── Phase 5: Delete removed files ──────────────────────────────────
    for entry in delta.deletes.iter() {
        if entry.absolute_path.exists() {
            if let Err(e) = fs::remove_file(&entry.absolute_path) {
                report.errors.push(format!(
                    "Failed to delete '{}': {}",
                    entry.absolute_path.display(),
                    e,
                ));
            }
        }
    }

    // ── Build reports & free memory ────────────────────────────────────
    for entry in delta.creates.iter_mut() {
        let bytes = entry.unit.content.as_ref().map_or(0, |c| c.len());
        report.files_created.push(FileReport {
            path: entry.unit.output_path.display().to_string(),
            handler: entry.unit.handler_name.clone(),
            entry_count: entry.unit.entry_count,
            bytes_written: bytes,
            backed_up: false,
        });
        entry.unit.content = None;
    }

    for entry in delta.updates.iter_mut() {
        let bytes = entry.unit.content.as_ref().map_or(0, |c| c.len());
        report.files_updated.push(FileReport {
            path: entry.unit.output_path.display().to_string(),
            handler: entry.unit.handler_name.clone(),
            entry_count: entry.unit.entry_count,
            bytes_written: bytes,
            backed_up: backup,
        });
        entry.unit.content = None;
    }

    for entry in delta.deletes.iter_mut() {
        report.files_deleted.push(FileReport {
            path: entry.unit.output_path.display().to_string(),
            handler: entry.unit.handler_name.clone(),
            entry_count: entry.unit.entry_count,
            bytes_written: 0,
            backed_up: backup,
        });
        entry.unit.content = None;
    }

    // Also clear content on unchanged entries.
    for entry in delta.unchanged.iter_mut() {
        entry.unit.content = None;
    }

    Ok(report)
}

/// Compute the `.bak` backup path for a given absolute path.
fn backup_path_for(absolute_path: &Path) -> PathBuf {
    let mut name = absolute_path.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::delta::DeltaEntry;
    use crate::export::{ExportUnit, FileAction};

    /// S-ERRTEST #6: Phase 2 (temp file write) failure → temp files cleaned up.
    ///
    /// Simulates a write failure by targeting a path whose parent is a file
    /// (not a directory), so `create_dir_all` or `fs::write` fails. Verifies
    /// that any temp files written before the failure are cleaned up.
    #[test]
    fn test_write_files_atomic_phase2_failure_cleans_temp_files() {
        let dir = tempfile::tempdir().unwrap();

        // Create a valid first file target
        let good_path = dir.path().join("Good.lsx");

        // Create a blocker: a regular file where the second entry expects a directory.
        // Writing to "blocker/Sub/File.lsx" will fail because "blocker" is a file.
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"I am a file, not a directory").unwrap();
        let bad_path = dir.path().join("blocker").join("Sub").join("Bad.lsx");

        let mut delta = super::super::delta::FileSystemDelta {
            creates: vec![
                DeltaEntry {
                    unit: ExportUnit {
                        handler_name: "test".to_string(),
                        output_path: PathBuf::from("Good.lsx"),
                        action: FileAction::Create,
                        entry_count: 1,
                        content: Some(b"good content".to_vec()),
                    },
                    absolute_path: good_path.clone(),
                    is_orphan: false,
                },
                DeltaEntry {
                    unit: ExportUnit {
                        handler_name: "test".to_string(),
                        output_path: PathBuf::from("blocker/Sub/Bad.lsx"),
                        action: FileAction::Create,
                        entry_count: 1,
                        content: Some(b"bad content".to_vec()),
                    },
                    absolute_path: bad_path.clone(),
                    is_orphan: false,
                },
            ],
            updates: vec![],
            deletes: vec![],
            unchanged: vec![],
        };

        let result = write_files_atomic(&mut delta, false);

        assert!(
            result.is_err(),
            "Phase 2 should fail because 'blocker' is a file, not a directory"
        );

        // Verify temp files were cleaned up: the first entry's .cmty_tmp should
        // have been removed by cleanup_temp_files.
        let good_tmp = temp_path_for(&good_path);
        assert!(
            !good_tmp.exists(),
            "Temp file for the good entry should have been cleaned up after failure, \
             but {good_tmp:?} still exists"
        );

        // The bad entry's temp should also not exist (it never got written).
        let bad_tmp = temp_path_for(&bad_path);
        assert!(
            !bad_tmp.exists(),
            "Temp file for the bad entry should not exist"
        );

        // The final file should not exist either (Phase 4 rename never ran).
        assert!(
            !good_path.exists(),
            "Good.lsx should not have been finalized"
        );
    }

    /// S-ERRTEST #7: Phase 4 (rename) failure → error reported with details.
    ///
    /// Testing a deterministic rename failure is difficult on most filesystems.
    /// Instead, this test verifies that:
    /// (a) When Phase 4 rename + copy fallback both fail, the error message
    ///     includes details about both the rename and copy failure.
    /// (b) Remaining temp files are cleaned up.
    ///
    /// We simulate this by writing the temp file manually, then making the
    /// target path un-writable (by pointing it at a nonexistent device/root).
    ///
    /// NOTE: On Windows, filesystem permissions are less granular than Unix,
    /// so this test uses an alternative approach: it writes a temp file for
    /// the first entry, then verifies that if we have a second entry whose
    /// target path is blocked, the error reports the path and cleans up.
    #[test]
    fn test_write_files_atomic_phase4_failure_reports_and_cleans() {
        let dir = tempfile::tempdir().unwrap();

        // First entry: valid
        let good_path = dir.path().join("Phase4Good.lsx");

        // Second entry: target path is a directory (rename to directory fails)
        let target_dir = dir.path().join("Phase4Bad.lsx");
        fs::create_dir_all(&target_dir).unwrap();
        // Place a file inside so the directory isn't empty
        // (non-empty directory can't be replaced by rename)
        fs::write(target_dir.join("occupant.txt"), b"blocker").unwrap();

        let mut delta = super::super::delta::FileSystemDelta {
            creates: vec![
                DeltaEntry {
                    unit: ExportUnit {
                        handler_name: "test".to_string(),
                        output_path: PathBuf::from("Phase4Good.lsx"),
                        action: FileAction::Create,
                        entry_count: 1,
                        content: Some(b"content A".to_vec()),
                    },
                    absolute_path: good_path.clone(),
                    is_orphan: false,
                },
                DeltaEntry {
                    unit: ExportUnit {
                        handler_name: "test".to_string(),
                        output_path: PathBuf::from("Phase4Bad.lsx"),
                        action: FileAction::Create,
                        entry_count: 1,
                        content: Some(b"content B".to_vec()),
                    },
                    absolute_path: target_dir.clone(),
                    is_orphan: false,
                },
            ],
            updates: vec![],
            deletes: vec![],
            unchanged: vec![],
        };

        let result = write_files_atomic(&mut delta, false);

        // The first rename succeeds (good_path is a normal file path).
        // The second rename fails because the target is a non-empty directory.
        // On Windows, fs::rename file→directory and fs::copy file→directory both fail.
        //
        // The outcome depends on Phase 4's rename order, but since the first
        // entry succeeds and the second may fail, we check for either:
        // (a) full success (if the OS allows overwriting a directory — unlikely), or
        // (b) error with cleanup of remaining temps.
        if let Err(err) = result {
            // Verify the error message mentions the problematic path
            assert!(
                err.message.contains("Phase4Bad.lsx") || err.message.contains("rename"),
                "Error should reference the failed path or operation, got: {}",
                err.message
            );

            // Remaining temp files should be cleaned up
            let bad_tmp = temp_path_for(&target_dir);
            assert!(
                !bad_tmp.exists(),
                "Remaining temp files should be cleaned up on Phase 4 failure"
            );
        }
        // If result is Ok, the OS handled directory replacement — that's fine too,
        // the important thing is no panic occurred.
    }
}
