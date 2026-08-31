use std::path::{Path, PathBuf};

use serde::Deserialize;
use walkdir::WalkDir;

use crate::error::AppError;
use crate::pak::convert::{
    convert_loca_xml_file_to_binary, convert_lsfx_lsx_file_to_binary,
    convert_lsx_file_to_lsf_bytes, convert_pak_path_loca, convert_pak_path_to_lsf,
    convert_pak_path_to_lsfx, should_convert_loca_xml, should_convert_to_lsf,
    should_convert_to_lsfx,
};
use crate::pak::format::{CompressionLevel, PakCompression, PakPackageFlags};
use crate::pak::merge::compute_merges;
use crate::pak::path::PakPath;
use crate::pak::writer::{PakWriteResult, PakWriter, PakWriterOptions};

#[derive(Deserialize)]
pub struct PackageModOptions {
    pub mod_path: String,
    pub output_path: String,
    pub priority: Option<u8>,
    pub compression: Option<String>,
    pub compression_level: Option<String>,
}

/// Files and directories to exclude from packaging.
const EXCLUDED_NAMES: &[&str] = &[
    ".git",
    ".vscode",
    "__MACOSX",
    ".DS_Store",
    "thumbs.db",
    "Thumbs.db",
    "desktop.ini",
];

const EXCLUDED_EXTENSIONS: &[&str] = &["bak"];

fn is_excluded(path: &Path, name: &str) -> bool {
    // Check exact name matches
    if EXCLUDED_NAMES.iter().any(|&e| e.eq_ignore_ascii_case(name)) {
        return true;
    }

    // Check extension
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if EXCLUDED_EXTENSIONS
            .iter()
            .any(|&e| e.eq_ignore_ascii_case(ext))
        {
            return true;
        }
    }

    // Skip hidden files/dirs (starting with '.')
    if name.starts_with('.') {
        return true;
    }

    false
}

fn parse_compression(s: &str) -> Result<PakCompression, String> {
    match s.to_ascii_lowercase().as_str() {
        "none" => Ok(PakCompression::None),
        "zlib" => Ok(PakCompression::Zlib),
        "lz4" => Ok(PakCompression::Lz4),
        other => Err(format!(
            "Unknown compression method: '{other}'. Expected 'none', 'zlib', or 'lz4'."
        )),
    }
}

fn parse_compression_level(s: &str) -> Result<CompressionLevel, String> {
    match s.to_ascii_lowercase().as_str() {
        "fast" => Ok(CompressionLevel::Fast),
        "default" => Ok(CompressionLevel::Default),
        "max" => Ok(CompressionLevel::Max),
        other => Err(format!(
            "Unknown compression level: '{other}'. Expected 'fast', 'default', or 'max'."
        )),
    }
}

#[tauri::command]
pub async fn cmd_package_mod(options: PackageModOptions) -> Result<PakWriteResult, AppError> {
    crate::blocking(move || {
        let mod_path = PathBuf::from(&options.mod_path);
        if !mod_path.is_dir() {
            return Err(format!("Mod directory not found: {}", mod_path.display()));
        }

        let mut output_path = PathBuf::from(&options.output_path);

        // If output_path is a directory, derive a .pak filename from the mod folder name
        if output_path.is_dir() {
            let mod_name = mod_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("mod");
            output_path = output_path.join(format!("{mod_name}.pak"));
        } else if output_path.extension().is_none() {
            output_path.set_extension("pak");
        }

        // Create parent directory for output if needed
        if let Some(parent) = output_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create output directory: {e}"))?;
            }
        }

        let compression = match &options.compression {
            Some(s) => parse_compression(s)?,
            None => PakCompression::Lz4,
        };

        let compression_level = match &options.compression_level {
            Some(s) => parse_compression_level(s)?,
            None => CompressionLevel::Default,
        };

        let writer_options = PakWriterOptions {
            compression,
            compression_level,
            priority: options.priority.unwrap_or(0),
            flags: PakPackageFlags::empty(),
        };

        let mut writer = PakWriter::new(&output_path, writer_options)
            .map_err(|e| format!("Failed to create pak writer: {e}"))?;

        // Collect files with pak-internal paths
        let mut file_entries: Vec<(PathBuf, PakPath)> = Vec::new();

        for entry in WalkDir::new(&mod_path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !is_excluded(e.path(), &name)
            })
        {
            let entry = entry.map_err(|e| format!("Failed to walk directory: {e}"))?;
            if !entry.file_type().is_file() {
                continue;
            }

            let relative = entry
                .path()
                .strip_prefix(&mod_path)
                .map_err(|e| format!("Failed to compute relative path: {e}"))?;

            let pak_path_str = relative.to_string_lossy().replace('\\', "/");
            let pak_path = PakPath::parse(&pak_path_str)
                .map_err(|e| format!("Invalid pak path '{pak_path_str}': {e}"))?;

            file_entries.push((entry.path().to_path_buf(), pak_path));
        }

        // Sort by pak path for deterministic output
        file_entries.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()));

        // Compute merge plan (RootTemplates, REGIONTYPE.lsx, Stats condensation)
        let merge_plan =
            compute_merges(&file_entries).map_err(|e| format!("Merge planning failed: {e}"))?;

        // Add merged entries first
        for (pak_path, bytes) in &merge_plan.merged_entries {
            writer
                .add_bytes(pak_path, bytes)
                .map_err(|e| format!("Failed to add merged entry {pak_path}: {e}"))?;
        }

        // Add non-merged files to the writer
        for (i, (disk_path, pak_path)) in file_entries.iter().enumerate() {
            // Skip entries consumed by merges
            if merge_plan.skip_indices.contains(&i) {
                continue;
            }

            // Apply path rewrites (e.g., .lsf.lsf → .lsf)
            let effective_pak = merge_plan.path_rewrites.get(&i).unwrap_or(pak_path);

            if should_convert_to_lsfx(effective_pak) {
                // .lsfx.lsx → binary .lsfx conversion
                let lsfx_path = convert_pak_path_to_lsfx(effective_pak)
                    .map_err(|e| format!("Path conversion failed for {pak_path}: {e}"))?;
                let lsfx_bytes = convert_lsfx_lsx_file_to_binary(disk_path)?;
                writer
                    .add_bytes(&lsfx_path, &lsfx_bytes)
                    .map_err(|e| format!("Failed to add {pak_path} as LSFX: {e}"))?;
            } else if should_convert_to_lsf(effective_pak) {
                // LSX → LSF conversion
                let lsf_path = convert_pak_path_to_lsf(effective_pak)
                    .map_err(|e| format!("Path conversion failed for {pak_path}: {e}"))?;
                let lsf_bytes = convert_lsx_file_to_lsf_bytes(disk_path)?;
                writer
                    .add_bytes(&lsf_path, &lsf_bytes)
                    .map_err(|e| format!("Failed to add {pak_path} as LSF: {e}"))?;
            } else if should_convert_loca_xml(effective_pak) {
                // .loca.xml → binary .loca conversion
                let loca_path = convert_pak_path_loca(effective_pak)
                    .map_err(|e| format!("Path conversion failed for {pak_path}: {e}"))?;
                let loca_bytes = convert_loca_xml_file_to_binary(disk_path)?;
                writer
                    .add_bytes(&loca_path, &loca_bytes)
                    .map_err(|e| format!("Failed to add {pak_path} as LOCA: {e}"))?;
            } else {
                writer
                    .add_file(disk_path, effective_pak)
                    .map_err(|e| format!("Failed to add {pak_path}: {e}"))?;
            }
        }

        writer
            .finish()
            .map_err(|e| format!("Failed to finalize pak: {e}"))
    })
    .await
}
