//! ZIP packaging utility for platform uploads.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use digest::Digest;
use walkdir::WalkDir;

use super::errors::PlatformError;

/// Information about a created upload package.
pub struct PackageInfo {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub file_count: usize,
    pub md5_hash: String,
}

/// Default glob patterns for files/directories excluded from upload ZIPs.
const DEFAULT_EXCLUDES: &[&str] = &[".git/", "__pycache__/", "*.log", ".DS_Store", "Thumbs.db"];

/// Check whether a relative path should be excluded based on the exclude patterns.
fn should_exclude(rel_path: &str, excludes: &[&str]) -> bool {
    for pattern in excludes {
        if pattern.ends_with('/') {
            // Directory prefix pattern
            let dir_name = pattern.trim_end_matches('/');
            // Match if any path component equals the directory name
            for component in rel_path.split(['/', '\\']) {
                if component == dir_name {
                    return true;
                }
            }
        } else if pattern.starts_with("*.") {
            // Extension pattern
            let ext = &pattern[1..]; // e.g. ".log"
            if rel_path.ends_with(ext) {
                return true;
            }
        } else {
            // Exact filename match against the last component
            if let Some(filename) = rel_path.rsplit(['/', '\\']).next() {
                if filename == *pattern {
                    return true;
                }
            }
        }
    }
    false
}

/// Create a ZIP archive of `source_dir` at `output_path`, excluding matching patterns.
///
/// Uses streaming compression — files are read and compressed one at a time
/// without loading the entire directory tree into memory.
///
/// `exclude` overrides default excludes; pass `&[]` to use only `DEFAULT_EXCLUDES`.
pub fn create_upload_zip(
    source_dir: &Path,
    output_path: &Path,
    exclude: &[&str],
) -> Result<PackageInfo, PlatformError> {
    let all_excludes: Vec<&str> = DEFAULT_EXCLUDES
        .iter()
        .chain(exclude.iter())
        .copied()
        .collect();

    let file = File::create(output_path)
        .map_err(|e| PlatformError::PackagingError(format!("Failed to create ZIP file: {e}")))?;
    let buf_writer = BufWriter::new(file);
    let mut zip = zip::ZipWriter::new(buf_writer);

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut file_count = 0usize;
    let mut read_buf = vec![0u8; 64 * 1024]; // 64 KB read buffer

    for entry in WalkDir::new(source_dir).follow_links(false) {
        let entry = entry
            .map_err(|e| PlatformError::PackagingError(format!("Failed to walk directory: {e}")))?;

        let abs_path = entry.path();
        let rel_path = abs_path
            .strip_prefix(source_dir)
            .map_err(|e| PlatformError::PackagingError(format!("Path strip error: {e}")))?;

        // Skip the root directory itself
        if rel_path.as_os_str().is_empty() {
            continue;
        }

        let rel_str = rel_path.to_string_lossy();
        if should_exclude(&rel_str, &all_excludes) {
            continue;
        }

        if entry.file_type().is_dir() {
            // Add directory entry (trailing /)
            let dir_name = format!("{rel_str}/");
            zip.add_directory(&dir_name, options).map_err(|e| {
                PlatformError::PackagingError(format!("Failed to add directory to ZIP: {e}"))
            })?;
        } else if entry.file_type().is_file() {
            zip.start_file(rel_str.to_string(), options).map_err(|e| {
                PlatformError::PackagingError(format!("Failed to start ZIP entry: {e}"))
            })?;

            let mut src = File::open(abs_path).map_err(|e| {
                PlatformError::PackagingError(format!("Failed to open {rel_str}: {e}"))
            })?;

            loop {
                let n = src.read(&mut read_buf).map_err(|e| {
                    PlatformError::PackagingError(format!("Failed to read {rel_str}: {e}"))
                })?;
                if n == 0 {
                    break;
                }
                zip.write_all(&read_buf[..n]).map_err(|e| {
                    PlatformError::PackagingError(format!("Failed to write to ZIP: {e}"))
                })?;
            }
            file_count += 1;
        }
    }

    zip.finish()
        .map_err(|e| PlatformError::PackagingError(format!("Failed to finalize ZIP: {e}")))?;

    // Compute MD5 hash of the finished ZIP file
    let md5_hash = compute_md5(output_path)?;

    let metadata = std::fs::metadata(output_path)
        .map_err(|e| PlatformError::PackagingError(format!("Failed to stat ZIP file: {e}")))?;

    Ok(PackageInfo {
        path: output_path.to_path_buf(),
        size_bytes: metadata.len(),
        file_count,
        md5_hash,
    })
}

/// Compute the MD5 hash of a file, returning the hex-encoded digest.
fn compute_md5(path: &Path) -> Result<String, PlatformError> {
    let mut file = File::open(path)
        .map_err(|e| PlatformError::PackagingError(format!("Failed to open for hashing: {e}")))?;
    let mut hasher = md5::Md5::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| {
            PlatformError::PackagingError(format!("Failed to read for hashing: {e}"))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(result.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_git_dir() {
        assert!(should_exclude(".git/config", DEFAULT_EXCLUDES));
        assert!(should_exclude("subdir/.git/HEAD", DEFAULT_EXCLUDES));
    }

    #[test]
    fn exclude_pycache() {
        assert!(should_exclude("__pycache__/module.pyc", DEFAULT_EXCLUDES));
    }

    #[test]
    fn exclude_log_files() {
        assert!(should_exclude("output.log", DEFAULT_EXCLUDES));
        assert!(should_exclude("subdir/debug.log", DEFAULT_EXCLUDES));
    }

    #[test]
    fn exclude_os_files() {
        assert!(should_exclude(".DS_Store", DEFAULT_EXCLUDES));
        assert!(should_exclude("Thumbs.db", DEFAULT_EXCLUDES));
    }

    #[test]
    fn normal_files_not_excluded() {
        assert!(!should_exclude("README.md", DEFAULT_EXCLUDES));
        assert!(!should_exclude("src/main.rs", DEFAULT_EXCLUDES));
        assert!(!should_exclude("Mods/MyMod/meta.lsx", DEFAULT_EXCLUDES));
    }

    #[test]
    fn create_zip_from_temp_dir() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let src = tmp.path().join("source");
        std::fs::create_dir_all(src.join("sub")).expect("create subdirs");
        std::fs::write(src.join("file.txt"), b"hello").expect("write file");
        std::fs::write(src.join("sub/nested.txt"), b"world").expect("write nested");
        std::fs::write(src.join(".DS_Store"), b"junk").expect("write excluded");

        let out = tmp.path().join("output.zip");
        let info = create_upload_zip(&src, &out, &[]).expect("create zip");

        assert_eq!(info.file_count, 2); // .DS_Store excluded
        assert!(info.size_bytes > 0);
        assert!(!info.md5_hash.is_empty());
        assert_eq!(info.md5_hash.len(), 32); // MD5 hex is 32 chars
    }
}
