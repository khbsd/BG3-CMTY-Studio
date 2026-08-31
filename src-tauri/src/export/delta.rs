use std::fs;
use std::path::Path;

use crate::error::AppError;

use super::{ExportPlan, ExportUnit, FileAction};

/// A single entry in the file-system delta, pairing an export unit with its
/// resolved absolute disk path.
#[derive(Debug)]
pub struct DeltaEntry {
    /// The export unit (with content populated after render).
    pub unit: ExportUnit,
    /// Absolute path where the file lives (or will live) on disk.
    pub absolute_path: std::path::PathBuf,
    /// Whether this was an orphan file (not claimed by any handler).
    pub is_orphan: bool,
}

/// Computed differences between the export plan and files on disk.
#[derive(Debug, Default)]
pub struct FileSystemDelta {
    /// Files to create (in plan, not on disk).
    pub creates: Vec<DeltaEntry>,
    /// Files to update (in plan, on disk, content differs).
    pub updates: Vec<DeltaEntry>,
    /// Files to delete (on disk, handler says Delete, or orphaned).
    pub deletes: Vec<DeltaEntry>,
    /// Files unchanged (in plan, on disk, content identical). Skipped during write.
    pub unchanged: Vec<DeltaEntry>,
}

impl FileSystemDelta {
    /// Total number of files that will be written or deleted.
    pub fn total_changes(&self) -> usize {
        self.creates.len() + self.updates.len() + self.deletes.len()
    }

    /// True if no changes are needed.
    pub fn is_empty(&self) -> bool {
        self.creates.is_empty() && self.updates.is_empty() && self.deletes.is_empty()
    }
}

/// Compare an export plan against the existing mod directory on disk.
///
/// This function takes a rendered plan (units should have `content` populated)
/// and determines what disk operations are needed.
///
/// Classification rules:
/// - **Create**: Unit action is Create/Update AND file does NOT exist on disk
/// - **Update**: Unit action is Create/Update AND file EXISTS on disk AND content differs
/// - **Delete**: Unit action is Delete, OR file is in orphan_files list
/// - **Unchanged**: Unit action is Create/Update AND file EXISTS AND content is identical
///
/// Content comparison uses byte-level equality (exact match).
pub fn compute_file_delta(
    plan: &mut ExportPlan,
    mod_path: &Path,
) -> Result<FileSystemDelta, AppError> {
    let mut delta = FileSystemDelta::default();

    // Drain units out of the plan so we can take ownership.
    let units: Vec<ExportUnit> = plan.units.drain(..).collect();

    for unit in units {
        let absolute_path = mod_path.join(&unit.output_path);

        match unit.action {
            FileAction::Delete => {
                delta.deletes.push(DeltaEntry {
                    unit,
                    absolute_path,
                    is_orphan: false,
                });
            }
            FileAction::Unchanged => {
                delta.unchanged.push(DeltaEntry {
                    unit,
                    absolute_path,
                    is_orphan: false,
                });
            }
            FileAction::Create | FileAction::Update => {
                classify_create_or_update(&mut delta, unit, &absolute_path);
            }
        }
    }

    // Process orphan files — only add deletes for files that actually exist.
    let orphan_files: Vec<std::path::PathBuf> = plan.orphan_files.drain(..).collect();

    for orphan_rel in orphan_files {
        let absolute_path = mod_path.join(&orphan_rel);

        if absolute_path.exists() {
            let synthetic_unit = ExportUnit {
                handler_name: "orphan".to_string(),
                output_path: orphan_rel,
                action: FileAction::Delete,
                entry_count: 0,
                content: None,
            };
            delta.deletes.push(DeltaEntry {
                unit: synthetic_unit,
                absolute_path,
                is_orphan: true,
            });
        }
    }

    Ok(delta)
}

/// Classify a Create/Update unit by comparing its content against the file on
/// disk. If the file doesn't exist → create. If content matches → unchanged.
/// Otherwise → update.
fn classify_create_or_update(delta: &mut FileSystemDelta, unit: ExportUnit, absolute_path: &Path) {
    if !absolute_path.exists() {
        delta.creates.push(DeltaEntry {
            unit,
            absolute_path: absolute_path.to_path_buf(),
            is_orphan: false,
        });
        return;
    }

    // File exists on disk — compare content byte-for-byte.
    let existing = match fs::read(absolute_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            // Treat read errors as "file doesn't exist" with a warning.
            eprintln!(
                "Warning: could not read existing file {}: {}. Treating as new file.",
                absolute_path.display(),
                e
            );
            delta.creates.push(DeltaEntry {
                unit,
                absolute_path: absolute_path.to_path_buf(),
                is_orphan: false,
            });
            return;
        }
    };

    let new_content = unit.content.as_deref().unwrap_or(&[]);

    if existing == new_content {
        delta.unchanged.push(DeltaEntry {
            unit,
            absolute_path: absolute_path.to_path_buf(),
            is_orphan: false,
        });
    } else {
        delta.updates.push(DeltaEntry {
            unit,
            absolute_path: absolute_path.to_path_buf(),
            is_orphan: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_unit(
        name: &str,
        rel_path: &str,
        action: FileAction,
        content: Option<Vec<u8>>,
    ) -> ExportUnit {
        ExportUnit {
            handler_name: name.to_string(),
            output_path: rel_path.into(),
            action,
            entry_count: 1,
            content,
        }
    }

    #[test]
    fn empty_mod_dir_all_creates() {
        let dir = TempDir::new().unwrap();
        let mut plan = ExportPlan {
            units: vec![
                make_unit(
                    "lsx",
                    "Public/Foo/bar.lsx",
                    FileAction::Create,
                    Some(b"hello".to_vec()),
                ),
                make_unit(
                    "lsx",
                    "Public/Foo/baz.lsx",
                    FileAction::Update,
                    Some(b"world".to_vec()),
                ),
            ],
            orphan_files: vec![],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert_eq!(delta.creates.len(), 2);
        assert!(delta.updates.is_empty());
        assert!(delta.deletes.is_empty());
        assert!(delta.unchanged.is_empty());
        assert_eq!(delta.total_changes(), 2);
        assert!(!delta.is_empty());
    }

    #[test]
    fn populated_dir_no_changes() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("data.lsx");
        fs::write(&file_path, b"same content").unwrap();

        let mut plan = ExportPlan {
            units: vec![make_unit(
                "lsx",
                "data.lsx",
                FileAction::Create,
                Some(b"same content".to_vec()),
            )],
            orphan_files: vec![],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert!(delta.creates.is_empty());
        assert!(delta.updates.is_empty());
        assert!(delta.deletes.is_empty());
        assert_eq!(delta.unchanged.len(), 1);
        assert!(delta.is_empty());
    }

    #[test]
    fn content_differs_classifies_as_update() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("data.lsx");
        fs::write(&file_path, b"old content").unwrap();

        let mut plan = ExportPlan {
            units: vec![make_unit(
                "lsx",
                "data.lsx",
                FileAction::Update,
                Some(b"new content".to_vec()),
            )],
            orphan_files: vec![],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert!(delta.creates.is_empty());
        assert_eq!(delta.updates.len(), 1);
        assert!(delta.deletes.is_empty());
        assert!(delta.unchanged.is_empty());
    }

    #[test]
    fn delete_action_goes_to_deletes() {
        let dir = TempDir::new().unwrap();

        let mut plan = ExportPlan {
            units: vec![make_unit("lsx", "gone.lsx", FileAction::Delete, None)],
            orphan_files: vec![],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert_eq!(delta.deletes.len(), 1);
        assert!(!delta.deletes[0].is_orphan);
    }

    #[test]
    fn orphan_existing_file_deleted() {
        let dir = TempDir::new().unwrap();
        let orphan_path = dir.path().join("stale.lsx");
        fs::write(&orphan_path, b"leftover").unwrap();

        let mut plan = ExportPlan {
            units: vec![],
            orphan_files: vec!["stale.lsx".into()],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert_eq!(delta.deletes.len(), 1);
        assert!(delta.deletes[0].is_orphan);
        assert_eq!(delta.deletes[0].unit.handler_name, "orphan");
    }

    #[test]
    fn orphan_missing_file_skipped() {
        let dir = TempDir::new().unwrap();

        let mut plan = ExportPlan {
            units: vec![],
            orphan_files: vec!["nonexistent.lsx".into()],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert!(delta.deletes.is_empty());
        assert!(delta.is_empty());
    }

    #[test]
    fn mixed_scenario() {
        let dir = TempDir::new().unwrap();

        // Existing file with same content (unchanged)
        fs::write(dir.path().join("unchanged.lsx"), b"keep").unwrap();
        // Existing file with different content (update)
        fs::write(dir.path().join("modified.lsx"), b"old").unwrap();
        // Existing orphan file (delete)
        fs::write(dir.path().join("orphan.lsx"), b"stale").unwrap();

        let mut plan = ExportPlan {
            units: vec![
                make_unit(
                    "lsx",
                    "unchanged.lsx",
                    FileAction::Create,
                    Some(b"keep".to_vec()),
                ),
                make_unit(
                    "lsx",
                    "modified.lsx",
                    FileAction::Update,
                    Some(b"new".to_vec()),
                ),
                make_unit(
                    "lsx",
                    "brand_new.lsx",
                    FileAction::Create,
                    Some(b"fresh".to_vec()),
                ),
                make_unit("lsx", "explicit_del.lsx", FileAction::Delete, None),
            ],
            orphan_files: vec!["orphan.lsx".into()],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert_eq!(delta.creates.len(), 1, "brand_new.lsx");
        assert_eq!(delta.updates.len(), 1, "modified.lsx");
        assert_eq!(delta.deletes.len(), 2, "explicit_del + orphan");
        assert_eq!(delta.unchanged.len(), 1, "unchanged.lsx");
        assert_eq!(delta.total_changes(), 4);
    }

    #[test]
    fn unchanged_action_goes_to_unchanged() {
        let dir = TempDir::new().unwrap();

        let mut plan = ExportPlan {
            units: vec![make_unit(
                "lsx",
                "skip.lsx",
                FileAction::Unchanged,
                Some(b"data".to_vec()),
            )],
            orphan_files: vec![],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        assert_eq!(delta.unchanged.len(), 1);
        assert!(delta.is_empty());
    }

    #[test]
    fn unit_with_none_content_compared_as_empty() {
        let dir = TempDir::new().unwrap();
        // File exists but is empty
        fs::write(dir.path().join("empty.lsx"), b"").unwrap();

        let mut plan = ExportPlan {
            units: vec![make_unit("lsx", "empty.lsx", FileAction::Create, None)],
            orphan_files: vec![],
        };

        let delta = compute_file_delta(&mut plan, dir.path()).unwrap();

        // None content treated as empty slice, empty file on disk → unchanged
        assert_eq!(delta.unchanged.len(), 1);
    }
}
