use std::path::PathBuf;

use base64::prelude::*;
use image_dds::{ddsfile::Dds, image_from_dds};

/// Convert a DDS file to PNG, returning a base64-encoded PNG string.
/// Supports BC1–BC7 and uncompressed DXGI formats via image_dds.
pub fn convert_dds_to_png(path: &str, project_dir: &str) -> Result<String, String> {
    let file_path = resolve_and_validate(path, project_dir)?;

    let dds_data = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read DDS file '{}': {e}", file_path.display()))?;

    let dds = Dds::read(&dds_data[..])
        .map_err(|e| format!("Failed to parse DDS header '{}': {e}", file_path.display()))?;

    let img = image_from_dds(&dds, 0).map_err(|e| {
        format!(
            "Failed to decode DDS texture '{}': {e}",
            file_path.display()
        )
    })?;

    let mut png_bytes = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png_bytes);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| format!("Failed to encode PNG: {e}"))?;

    Ok(BASE64_STANDARD.encode(&png_bytes))
}

/// Get the dimensions (width, height) from a DDS file without full decode.
pub fn get_dds_dimensions(path: &str, project_dir: &str) -> Result<(u32, u32), String> {
    let file_path = resolve_and_validate(path, project_dir)?;

    let dds_data = std::fs::read(&file_path)
        .map_err(|e| format!("Failed to read DDS file '{}': {e}", file_path.display()))?;

    let dds = Dds::read(&dds_data[..])
        .map_err(|e| format!("Failed to parse DDS header '{}': {e}", file_path.display()))?;

    Ok((dds.get_width(), dds.get_height()))
}

/// Validate path is within the project directory (prevent path traversal).
fn resolve_and_validate(path: &str, project_dir: &str) -> Result<PathBuf, String> {
    let project = PathBuf::from(project_dir)
        .canonicalize()
        .map_err(|e| format!("Invalid project directory: {e}"))?;

    let file_path = project
        .join(path)
        .canonicalize()
        .map_err(|e| format!("File not found: {e}"))?;

    if !file_path.starts_with(&project) {
        return Err("Path traversal outside project directory rejected".to_string());
    }

    Ok(file_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_str().unwrap();

        let result = convert_dds_to_png("../../etc/passwd", project_dir);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("File not found") || err.contains("Path traversal"),
            "Expected path traversal error, got: {err}"
        );
    }

    #[test]
    fn nonexistent_file_errors() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_str().unwrap();

        let result = convert_dds_to_png("does_not_exist.dds", project_dir);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("File not found"),
            "Expected descriptive file-not-found error"
        );
    }

    #[test]
    fn dimensions_nonexistent_file_errors() {
        let tmp = TempDir::new().unwrap();
        let project_dir = tmp.path().to_str().unwrap();

        let result = get_dds_dimensions("missing.dds", project_dir);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("File not found"),
            "Expected descriptive file-not-found error"
        );
    }
}
