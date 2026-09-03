//! Schema database lifecycle management.
//!
//! Schema DBs (ref_base, ref_honor, ref_mods, staging) are built at compile
//! time by `generate_schema` and bundled as Tauri resources.  At runtime they
//! are copied to the app-data directory on first launch, then populated by the
//! native pak-streaming pipeline.
//!
//! Users never interact with the DB files directly.

use std::fs;
use std::path::{Path, PathBuf};
use tauri::Manager;

/// Names of the schema databases bundled as resources.
const SCHEMA_DB_NAMES: &[&str] = &[
    "ref_base.sqlite",
    "ref_honor.sqlite",
    "ref_mods.sqlite",
    "staging.sqlite",
];

/// Resolved paths to writable schema databases.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DbPaths {
    pub base: PathBuf,
    pub honor: PathBuf,
    pub mods: PathBuf,
    pub staging: PathBuf,
    /// The parent directory containing all DB files.
    pub dir: PathBuf,
}

/// Return the writable directory where schema DBs live at runtime.
///
/// During development (no bundled resources), falls back to `src-tauri/resources/`.
pub fn get_db_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Cannot resolve app data dir: {e}"))?
        .join("databases")
        .join("gamedbs");
    Ok(dir)
}

/// Resolve the DB paths without creating directories or copying files.
///
/// Useful for read-only checks (e.g. "does the base DB exist and have data?").
pub fn get_db_paths(app: &tauri::AppHandle) -> Result<DbPaths, String> {
    let db_dir = get_db_dir(app)?;
    Ok(build_db_paths(&db_dir))
}

/// Ensure all schema DBs exist in the writable app-data directory.
///
/// On first launch (or after a reset), this copies the bundled resource
/// versions into the writable location.  On subsequent launches the existing
/// (possibly populated) copies are left untouched.
///
/// Returns the resolved paths.
pub fn ensure_schema_dbs(app: &tauri::AppHandle) -> Result<DbPaths, String> {
    let db_dir = get_db_dir(app)?;
    fs::create_dir_all(&db_dir)
        .map_err(|e| format!("Cannot create DB dir {}: {}", db_dir.display(), e))?;
    validate_db_dir(&db_dir)?;

    for name in SCHEMA_DB_NAMES {
        let dest = db_dir.join(name);
        if !dest.exists() {
            copy_bundled_db(app, name, &dest)?;
        }
    }

    Ok(build_db_paths(&db_dir))
}

/// Recreate only the staging database by deleting and re-copying from the
/// bundled resource.  Reference DBs (ref_base, ref_honor, ref_mods) are
/// untouched.
///
/// After copying, UUID uniqueness indexes are applied so that staging entries
/// cannot have duplicate UUIDs.
///
/// Returns the path to the fresh staging database.
pub fn recreate_staging_db(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let db_dir = get_db_dir(app)?;
    fs::create_dir_all(&db_dir)
        .map_err(|e| format!("Cannot create DB dir {}: {}", db_dir.display(), e))?;
    validate_db_dir(&db_dir)?;

    let dest = db_dir.join("staging.sqlite");
    remove_db_files(&dest);
    copy_bundled_db(app, "staging.sqlite", &dest)?;
    ensure_staging_uuid_indexes(&dest)?;

    Ok(dest)
}

/// Reset all schema DBs by re-copying from the bundled resources.
///
/// Existing (populated) databases are deleted and replaced with the clean
/// schema-only versions.  The caller should re-run the populate pipeline
/// afterward.
pub fn reset_schema_dbs(app: &tauri::AppHandle) -> Result<DbPaths, String> {
    let db_dir = get_db_dir(app)?;
    fs::create_dir_all(&db_dir)
        .map_err(|e| format!("Cannot create DB dir {}: {}", db_dir.display(), e))?;
    validate_db_dir(&db_dir)?;

    for name in SCHEMA_DB_NAMES {
        let dest = db_dir.join(name);
        // Remove existing DB and WAL/SHM sidecars
        remove_db_files(&dest);
        copy_bundled_db(app, name, &dest)?;
    }

    Ok(build_db_paths(&db_dir))
}

/// Reset only the reference databases (ref_base, ref_honor, ref_mods) by
/// re-copying from the bundled resources.  Staging is left untouched because
/// it contains user project data.
pub fn reset_reference_dbs(app: &tauri::AppHandle) -> Result<DbPaths, String> {
    let db_dir = get_db_dir(app)?;
    fs::create_dir_all(&db_dir)
        .map_err(|e| format!("Cannot create DB dir {}: {}", db_dir.display(), e))?;
    validate_db_dir(&db_dir)?;

    for name in &SCHEMA_DB_NAMES[0..3] {
        //log::info!("{}", name);
        let dest = db_dir.join(name);
        remove_db_files(&dest);
        //log::info!("{:?}", &dest);
        copy_bundled_db(app, name, &dest)?;
    }

    Ok(build_db_paths(&db_dir))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Validate that the DB directory is safe for use.
///
/// Checks:
/// - The path is not a symlink (prevents redirect attacks)
/// - The directory is writable
fn validate_db_dir(dir: &Path) -> Result<(), String> {
    // Reject if the DB directory itself is a symlink
    let meta = fs::symlink_metadata(dir)
        .map_err(|e| format!("Cannot stat DB dir {}: {}", dir.display(), e))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "DB directory {} is a symlink — refusing to use it for security reasons. \
             Remove the symlink and restart the application.",
            dir.display(),
        ));
    }

    // Verify the directory is writable by creating and removing a probe file
    let probe = dir.join(".bg3_write_probe");
    fs::write(&probe, b"probe")
        .map_err(|e| format!("DB dir {} is not writable: {}", dir.display(), e))?;
    let _ = fs::remove_file(&probe);

    Ok(())
}

/// Copy a single bundled resource DB to the destination.
///
/// In production, this reads from the Tauri resource directory.
/// In development, it falls back to `src-tauri/resources/` relative to the
/// manifest directory.
fn copy_bundled_db(app: &tauri::AppHandle, name: &str, dest: &Path) -> Result<(), String> {
    // Try Tauri resource resolver first (production path)
    let resource_path = get_db_dir(&app).unwrap();

    // print!("{}", format!("path: {}", resource_path.display()));

    if resource_path.is_file() {
        fs::copy(&resource_path, dest).map_err(|e| {
            format!(
                "Copy {} → {}: {}",
                resource_path.display(),
                dest.display(),
                e
            )
        })?;
        return Ok(());
    }

    // Dev fallback: src-tauri/resources/ relative to CARGO_MANIFEST_DIR
    let dev_path = dev_resources_dir().join("gamedbs").join(name);
    if dev_path.is_file() {
        fs::copy(&dev_path, dest)
            .map_err(|e| format!("Copy {} → {}: {}", dev_path.display(), dest.display(), e))?;
        return Ok(());
    }

    Err(format!(
        "Bundled schema DB '{}' not found. Checked:\n  {}\n  {}\n\
         Run `cargo run --release --bin generate_schema` to create them.",
        name,
        resource_path.display(),
        dev_path.display(),
    ))
}

/// Remove a DB file and its WAL/SHM sidecars.
fn remove_db_files(path: &Path) {
    let _ = fs::remove_file(path);
    for sfx in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), sfx));
        let _ = fs::remove_file(&sidecar);
    }
}

/// Build the `DbPaths` struct from a directory.
fn build_db_paths(dir: &Path) -> DbPaths {
    DbPaths {
        base: dir.join("ref_base.sqlite"),
        honor: dir.join("ref_honor.sqlite"),
        mods: dir.join("ref_mods.sqlite"),
        staging: dir.join("staging.sqlite"),
        dir: dir.to_path_buf(),
    }
}

/// Dev-mode resources directory (relative to Cargo manifest).
fn dev_resources_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is set at compile time for the crate
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir).join("resources")
}

/// Apply UNIQUE indexes on UUID columns in the staging DB (DSM-03).
///
/// Tables with `PkStrategy::Uuid` already have UUID as PRIMARY KEY (unique).
/// This scans for any non-PK "UUID" columns in other tables and adds a unique
/// index to prevent duplicate entries during authoring.
fn ensure_staging_uuid_indexes(staging_path: &Path) -> Result<(), String> {
    let conn = rusqlite::Connection::open(staging_path)
        .map_err(|e| format!("Open staging DB for indexing: {e}"))?;

    // List all user tables (skip meta/internal tables starting with _)
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name NOT LIKE '\\_%' ESCAPE '\\'",
            )
            .map_err(|e| format!("List tables: {e}"))?;
        let result: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| format!("Query tables: {e}"))?
            .filter_map(|r| r.ok())
            .collect();
        result
    };

    for table in &tables {
        // Check if this table has a UUID column that is NOT already the PK
        let col_info: Vec<(String, bool)> = {
            let mut pstmt = conn
                .prepare(&format!("PRAGMA table_info(\"{table}\")"))
                .map_err(|e| format!("PRAGMA table_info({table}): {e}"))?;
            let result: Vec<(String, bool)> = pstmt
                .query_map([], |row| {
                    let name: String = row.get(1)?;
                    let pk: i32 = row.get(5)?;
                    Ok((name, pk != 0))
                })
                .map_err(|e| format!("Read cols for {table}: {e}"))?
                .filter_map(|r| r.ok())
                .collect();
            result
        };

        for (col_name, is_pk) in &col_info {
            if col_name == "UUID" && !is_pk {
                let idx_name = format!("uq_{table}_{col_name}");
                let sql = format!(
                    "CREATE UNIQUE INDEX IF NOT EXISTS \"{idx_name}\" ON \"{table}\"(\"{col_name}\")"
                );
                conn.execute_batch(&sql).map_err(|e| {
                    format!("Create unique index {idx_name} on {table}.{col_name}: {e}")
                })?;
            }
        }
    }

    drop(conn);
    Ok(())
}

/// Run `PRAGMA integrity_check` on a database file (DSM-04).
///
/// Returns `Ok(None)` if the DB passes ("ok"), or `Ok(Some(details))` with
/// the integrity check output if issues are found.  Returns `Err` only on
/// connection/IO failures.
pub fn run_integrity_check(db_path: &Path) -> Result<Option<String>, String> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("Open {} for integrity check: {}", db_path.display(), e))?;

    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| format!("PRAGMA integrity_check on {}: {}", db_path.display(), e))?;

    conn.close().map_err(|(_c, e)| format!("Close DB: {e}"))?;

    if result == "ok" {
        Ok(None)
    } else {
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn validate_db_dir_succeeds_on_real_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let result = validate_db_dir(tmp.path());
        assert!(
            result.is_ok(),
            "validate_db_dir should succeed on a real temp dir"
        );
    }

    #[test]
    fn validate_db_dir_rejects_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let real_dir = tmp.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let link_path = tmp.path().join("link");

        // Create a symlink (requires dev mode or elevated on Windows)
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_dir, &link_path).unwrap();
        #[cfg(windows)]
        {
            // junction doesn't need elevation and is detected as symlink by symlink_metadata
            match std::os::windows::fs::symlink_dir(&real_dir, &link_path) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Skipping symlink test (needs elevation): {e}");
                    return;
                }
            }
        }

        let result = validate_db_dir(&link_path);
        assert!(result.is_err(), "validate_db_dir should reject symlinks");
        let err = result.unwrap_err();
        assert!(
            err.contains("symlink"),
            "Error should mention symlink: {err}"
        );
    }

    #[test]
    fn run_integrity_check_ok_on_valid_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.sqlite");

        // Create a valid (empty) SQLite database
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'hello')", [])
            .unwrap();
        drop(conn);

        let result = run_integrity_check(&db_path).unwrap();
        assert_eq!(result, None, "Valid DB should return Ok(None)");
    }

    #[test]
    fn run_integrity_check_detects_corrupt_header() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("corrupt.sqlite");

        // Write garbage that isn't a valid SQLite DB
        fs::write(&db_path, b"THIS IS NOT A SQLITE DATABASE FILE AT ALL").unwrap();

        // Opening this as a DB and running integrity_check should fail or report issues
        let result = run_integrity_check(&db_path);
        match result {
            Ok(Some(details)) => {
                // Integrity check found issues — expected
                assert!(!details.is_empty(), "Should have non-empty details");
            }
            Err(_) => {
                // Connection error on corrupt file — also acceptable
            }
            Ok(None) => {
                panic!("Corrupt file should not pass integrity check");
            }
        }
    }

    #[test]
    fn build_db_paths_produces_correct_names() {
        let dir = PathBuf::from("/tmp/test_dbs");
        let paths = build_db_paths(&dir);
        assert_eq!(paths.base, dir.join("ref_base.sqlite"));
        assert_eq!(paths.honor, dir.join("ref_honor.sqlite"));
        assert_eq!(paths.mods, dir.join("ref_mods.sqlite"));
        assert_eq!(paths.staging, dir.join("staging.sqlite"));
        assert_eq!(paths.dir, dir);
    }

    #[test]
    fn remove_db_files_handles_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("nonexistent.sqlite");
        // Should not panic on missing files
        remove_db_files(&db_path);
    }

    #[test]
    fn remove_db_files_cleans_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let wal_path = tmp.path().join("test.sqlite-wal");
        let shm_path = tmp.path().join("test.sqlite-shm");

        fs::write(&db_path, b"db").unwrap();
        fs::write(&wal_path, b"wal").unwrap();
        fs::write(&shm_path, b"shm").unwrap();

        remove_db_files(&db_path);

        assert!(!db_path.exists(), "DB file should be removed");
        assert!(!wal_path.exists(), "WAL sidecar should be removed");
        assert!(!shm_path.exists(), "SHM sidecar should be removed");
    }
}
