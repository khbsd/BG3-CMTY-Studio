//! mod.io direct file upload with MD5 hashing and progress reporting.

use std::path::Path;

use digest::Digest;
use serde::{Deserialize, Serialize};

use crate::platform::errors::PlatformError;
use crate::platform::modio::client::{ModioClientSnapshot, BASE_URL};
use crate::platform::progress::{self, Platform, UploadProgress, UploadStage};

/// Maximum upload file size: 500 MB.
const MAX_FILE_SIZE: u64 = 500 * 1024 * 1024;

/// Parameters for a mod.io file upload.
#[derive(Debug, Clone, Deserialize)]
pub struct ModioUploadParams {
    /// mod.io game ID (default: 6715 for BG3).
    pub game_id: u64,
    /// The mod to upload to.
    pub mod_id: u64,
    /// Local path to the ZIP file to upload.
    pub file_path: String,
    /// Semantic version string.
    pub version: String,
    /// Optional changelog text.
    pub changelog: Option<String>,
    /// Whether to set this as the active (live) file. Defaults to `true`.
    pub active: Option<bool>,
    /// Optional metadata blob.
    pub metadata_blob: Option<String>,
}

/// Response from a successful modfile upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModioModfileResponse {
    pub id: u64,
    pub mod_id: u64,
    pub filename: String,
    pub version: Option<String>,
    pub filesize: u64,
    #[serde(default)]
    pub virus_status: u8,
}

// ── Error envelope ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct ErrorObject {
    message: String,
}

#[derive(Deserialize)]
struct ModioErrorResponse {
    error: ErrorObject,
}

// ── Public API ──────────────────────────────────────────────────────

/// Upload a ZIP file to an existing mod on mod.io.
///
/// - Validates file exists, is a ZIP, and ≤500 MB
/// - Computes MD5 hash before upload
/// - Sends multipart/form-data to `POST /games/{game_id}/mods/{mod_id}/files`
/// - Emits progress events via `platform::progress::emit_progress()`
pub async fn upload_file(
    client: &ModioClientSnapshot,
    params: &ModioUploadParams,
    app_handle: &tauri::AppHandle,
) -> Result<ModioModfileResponse, PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required for uploads".into(),
    })?;

    // ── Pre-upload validation ───────────────────────────────────────
    let file_path = Path::new(&params.file_path);

    if !file_path.exists() {
        return Err(PlatformError::ValidationError(format!(
            "File does not exist: {}",
            params.file_path
        )));
    }

    let metadata = std::fs::metadata(file_path)?;
    let file_size = metadata.len();

    if file_size > MAX_FILE_SIZE {
        return Err(PlatformError::ValidationError(format!(
            "File exceeds 500 MB limit ({:.1} MB)",
            file_size as f64 / (1024.0 * 1024.0)
        )));
    }

    // Check ZIP extension
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if !ext.eq_ignore_ascii_case("zip") {
        return Err(PlatformError::ValidationError(
            "Only ZIP files can be uploaded to mod.io".into(),
        ));
    }

    // ── MD5 hash ────────────────────────────────────────────────────
    let _ = progress::emit_progress(
        app_handle,
        &UploadProgress {
            platform: Platform::Modio,
            stage: UploadStage::Hashing,
            percent: 0.0,
            bytes_sent: 0,
            bytes_total: file_size,
            message: "Computing file hash…".into(),
        },
    );

    let md5_hash = compute_md5(file_path)?;

    let _ = progress::emit_progress(
        app_handle,
        &UploadProgress {
            platform: Platform::Modio,
            stage: UploadStage::Hashing,
            percent: 100.0,
            bytes_sent: file_size,
            bytes_total: file_size,
            message: "Hash computed".into(),
        },
    );

    // ── Build multipart form ────────────────────────────────────────
    let _ = progress::emit_progress(
        app_handle,
        &UploadProgress {
            platform: Platform::Modio,
            stage: UploadStage::Uploading,
            percent: 0.0,
            bytes_sent: 0,
            bytes_total: file_size,
            message: "Uploading…".into(),
        },
    );

    let file_bytes = tokio::fs::read(file_path)
        .await
        .map_err(|e| PlatformError::IoError(format!("Failed to read file: {e}")))?;

    let filename = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload.zip")
        .to_string();

    let file_part = reqwest::multipart::Part::bytes(file_bytes)
        .file_name(filename)
        .mime_str("application/zip")
        .map_err(|e| PlatformError::HttpError(format!("Failed to set MIME type: {e}")))?;

    let mut form = reqwest::multipart::Form::new()
        .part("filedata", file_part)
        .text("version", params.version.clone())
        .text("filehash", md5_hash)
        .text(
            "active",
            if params.active.unwrap_or(true) {
                "true"
            } else {
                "false"
            },
        )
        .text("platforms[]", "windows");

    if let Some(ref changelog) = params.changelog {
        form = form.text("changelog", changelog.clone());
    }
    if let Some(ref blob) = params.metadata_blob {
        form = form.text("metadata_blob", blob.clone());
    }

    // ── Send request ────────────────────────────────────────────────
    let url = format!(
        "{BASE_URL}/games/{}/mods/{}/files",
        params.game_id, params.mod_id
    );

    let resp = client
        .http_client()
        .post(&url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                PlatformError::UploadTimeout
            } else {
                PlatformError::HttpError(format!("Upload request failed: {e}"))
            }
        })?;

    // ── Handle response ─────────────────────────────────────────────
    if resp.status().is_success() {
        let _ = progress::emit_progress(
            app_handle,
            &UploadProgress {
                platform: Platform::Modio,
                stage: UploadStage::Complete,
                percent: 100.0,
                bytes_sent: file_size,
                bytes_total: file_size,
                message: "Upload complete".into(),
            },
        );

        let modfile: ModioModfileResponse = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse upload response: {e}"))
        })?;
        Ok(modfile)
    } else {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };

        let _ = progress::emit_progress(
            app_handle,
            &UploadProgress {
                platform: Platform::Modio,
                stage: UploadStage::Error,
                percent: 0.0,
                bytes_sent: 0,
                bytes_total: file_size,
                message: format!("Upload failed: {message}"),
            },
        );

        Err(PlatformError::ApiError { status, message })
    }
}

/// Compute the MD5 hex digest of a file using streaming reads.
fn compute_md5(path: &Path) -> Result<String, PlatformError> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = md5::Md5::new();
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let result = hasher.finalize();
    Ok(result.iter().map(|b| format!("{b:02x}")).collect())
}
