//! mod.io mod profile creation and editing.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::platform::errors::PlatformError;
use crate::platform::modio::client::{ModioClientSnapshot, BASE_URL};

/// Maximum logo file size: 8 MB.
const MAX_LOGO_SIZE: u64 = 8 * 1024 * 1024;

// ── Request types ───────────────────────────────────────────────────

/// Parameters for creating a new mod on mod.io.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateModParams {
    pub game_id: u64,
    pub name: String,
    /// URL-friendly slug (e.g. `"my-cool-mod"`). Auto-generated if omitted.
    pub name_id: Option<String>,
    /// Path to logo image file on disk (min 512×288, JPG/PNG, max 8 MB).
    pub logo_path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub homepage_url: Option<String>,
    /// 0 = hidden, 1 = public.
    pub visible: Option<u8>,
    pub tags: Option<Vec<String>>,
}

/// Parameters for editing an existing mod on mod.io.
#[derive(Debug, Clone, Deserialize)]
pub struct EditModParams {
    pub game_id: u64,
    pub mod_id: u64,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub homepage_url: Option<String>,
    pub visible: Option<u8>,
    pub maturity_option: Option<u8>,
    /// Optional logo update — path to image file on disk.
    pub logo_path: Option<String>,
}

// ── Response types ──────────────────────────────────────────────────

/// Parsed response from mod creation/editing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModioModResponse {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub name_id: String,
    #[serde(default)]
    pub status: u8,
    #[serde(default)]
    pub date_added: u64,
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

// ── Helpers ─────────────────────────────────────────────────────────

/// Guess MIME type from file extension.
fn mime_for_image(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        e if e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg") => "image/jpeg",
        e if e.eq_ignore_ascii_case("png") => "image/png",
        _ => "application/octet-stream",
    }
}

/// Read and validate a logo file from disk.
async fn read_logo(path: &str) -> Result<(Vec<u8>, String, &'static str), PlatformError> {
    let p = Path::new(path);

    if !p.exists() {
        return Err(PlatformError::ValidationError(format!(
            "Logo file does not exist: {path}"
        )));
    }

    let meta = std::fs::metadata(p)?;
    if meta.len() > MAX_LOGO_SIZE {
        return Err(PlatformError::ValidationError(format!(
            "Logo exceeds 8 MB limit ({:.1} MB)",
            meta.len() as f64 / (1024.0 * 1024.0)
        )));
    }

    let bytes = tokio::fs::read(p)
        .await
        .map_err(|e| PlatformError::IoError(format!("Failed to read logo: {e}")))?;

    let filename = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("logo.png")
        .to_string();

    let mime = mime_for_image(p);
    Ok((bytes, filename, mime))
}

// ── Public API ──────────────────────────────────────────────────────

/// Create a new mod profile on mod.io.
///
/// `POST /games/{game_id}/mods` — requires OAuth2 token.
pub async fn create_mod(
    client: &ModioClientSnapshot,
    params: &CreateModParams,
) -> Result<ModioModResponse, PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required to create mods".into(),
    })?;

    let (logo_bytes, logo_filename, logo_mime) = read_logo(&params.logo_path).await?;

    let logo_part = reqwest::multipart::Part::bytes(logo_bytes)
        .file_name(logo_filename)
        .mime_str(logo_mime)
        .map_err(|e| PlatformError::HttpError(format!("Failed to set MIME type: {e}")))?;

    let mut form = reqwest::multipart::Form::new()
        .text("name", params.name.clone())
        .part("logo", logo_part);

    if let Some(ref name_id) = params.name_id {
        form = form.text("name_id", name_id.clone());
    }
    if let Some(ref summary) = params.summary {
        form = form.text("summary", summary.clone());
    }
    if let Some(ref description) = params.description {
        form = form.text("description", description.clone());
    }
    if let Some(ref homepage) = params.homepage_url {
        form = form.text("homepage_url", homepage.clone());
    }
    if let Some(visible) = params.visible {
        form = form.text("visible", visible.to_string());
    }
    if let Some(ref tags) = params.tags {
        for tag in tags {
            form = form.text("tags[]", tag.clone());
        }
    }

    let url = format!("{BASE_URL}/games/{}/mods", params.game_id);

    let resp = client
        .http_client()
        .post(&url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Create mod request failed: {e}")))?;

    if resp.status().is_success() {
        let body: ModioModResponse = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse create mod response: {e}"))
        })?;
        Ok(body)
    } else {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };
        Err(PlatformError::ApiError { status, message })
    }
}

/// Edit an existing mod profile on mod.io.
///
/// `POST /games/{game_id}/mods/{mod_id}` — requires OAuth2 token.
pub async fn edit_mod(
    client: &ModioClientSnapshot,
    params: &EditModParams,
) -> Result<ModioModResponse, PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required to edit mods".into(),
    })?;

    let mut form = reqwest::multipart::Form::new();

    if let Some(ref name) = params.name {
        form = form.text("name", name.clone());
    }
    if let Some(ref summary) = params.summary {
        form = form.text("summary", summary.clone());
    }
    if let Some(ref description) = params.description {
        form = form.text("description", description.clone());
    }
    if let Some(ref homepage) = params.homepage_url {
        form = form.text("homepage_url", homepage.clone());
    }
    if let Some(visible) = params.visible {
        form = form.text("visible", visible.to_string());
    }
    if let Some(maturity) = params.maturity_option {
        form = form.text("maturity_option", maturity.to_string());
    }
    if let Some(ref logo_path) = params.logo_path {
        let (logo_bytes, logo_filename, logo_mime) = read_logo(logo_path).await?;
        let logo_part = reqwest::multipart::Part::bytes(logo_bytes)
            .file_name(logo_filename)
            .mime_str(logo_mime)
            .map_err(|e| PlatformError::HttpError(format!("Failed to set MIME type: {e}")))?;
        form = form.part("logo", logo_part);
    }

    let url = format!("{BASE_URL}/games/{}/mods/{}", params.game_id, params.mod_id);

    let resp = client
        .http_client()
        .post(&url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Edit mod request failed: {e}")))?;

    if resp.status().is_success() {
        let body: ModioModResponse = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse edit mod response: {e}"))
        })?;
        Ok(body)
    } else {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };
        Err(PlatformError::ApiError { status, message })
    }
}
