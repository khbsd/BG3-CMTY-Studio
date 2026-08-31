//! mod.io file/version management — list, edit, and delete modfiles.

use serde::{Deserialize, Deserializer, Serialize};

use crate::platform::errors::PlatformError;
use crate::platform::modio::client::{ModioClientSnapshot, BASE_URL};

// ── Helpers ─────────────────────────────────────────────────────────

/// Deserialize a value that may be `null` in JSON, falling back to `T::default()`.
fn nullable_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

// ── Response types ──────────────────────────────────────────────────

/// A single modfile entry from mod.io.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModioFileEntry {
    pub id: u64,
    pub mod_id: u64,
    #[serde(default, deserialize_with = "nullable_default")]
    pub filename: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub filesize: u64,
    #[serde(default, deserialize_with = "nullable_default")]
    pub date_added: u64,
    #[serde(default)]
    pub changelog: Option<String>,
    #[serde(default, deserialize_with = "nullable_default")]
    pub virus_status: u8,
}

/// Parameters for editing a modfile's metadata.
#[derive(Debug, Clone, Deserialize)]
pub struct EditFileParams {
    pub game_id: u64,
    pub mod_id: u64,
    pub file_id: u64,
    pub version: Option<String>,
    pub changelog: Option<String>,
    pub active: Option<bool>,
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

/// Paginated response envelope from mod.io.
#[derive(Deserialize)]
struct PaginatedResponse {
    data: Vec<ModioFileEntry>,
}

// ── Public API ──────────────────────────────────────────────────────

/// List all files for a mod.
///
/// `GET /games/{game_id}/mods/{mod_id}/files` — uses API key or Bearer token.
pub async fn list_files(
    client: &ModioClientSnapshot,
    game_id: u64,
    mod_id: u64,
) -> Result<Vec<ModioFileEntry>, PlatformError> {
    let url = format!("{BASE_URL}/games/{game_id}/mods/{mod_id}/files?_limit=100");

    eprintln!("[modio] GET {url}");

    let mut req = client.http_client().get(&url);
    if let Some(token) = client.token() {
        req = req.bearer_auth(token);
    } else {
        req = req.query(&[("api_key", client.api_key())]);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("List files request failed: {e}")))?;

    let status_code = resp.status();
    eprintln!("[modio] list_files response: HTTP {status_code}");

    if status_code.is_success() {
        let body_text = resp.text().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to read list files body: {e}"))
        })?;
        eprintln!(
            "[modio] list_files body (first 500 chars): {}",
            &body_text[..body_text.len().min(500)]
        );

        let body: PaginatedResponse = serde_json::from_str(&body_text).map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse list files response: {e}"))
        })?;
        eprintln!("[modio] list_files parsed {} file entries", body.data.len());
        Ok(body.data)
    } else {
        let status = status_code.as_u16();
        let body_text = resp.text().await.unwrap_or_default();
        eprintln!("[modio] list_files error body: {body_text}");
        let message = serde_json::from_str::<ModioErrorResponse>(&body_text)
            .map(|e| e.error.message)
            .unwrap_or_else(|_| format!("HTTP {status}"));
        Err(PlatformError::ApiError { status, message })
    }
}

/// Edit a modfile's metadata (version, changelog, active status).
///
/// `PUT /games/{game_id}/mods/{mod_id}/files/{file_id}` — requires OAuth2 token.
pub async fn edit_file(
    client: &ModioClientSnapshot,
    params: &EditFileParams,
) -> Result<(), PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required to edit files".into(),
    })?;

    let url = format!(
        "{BASE_URL}/games/{}/mods/{}/files/{}",
        params.game_id, params.mod_id, params.file_id
    );

    let mut form_data: Vec<(&str, String)> = Vec::new();
    if let Some(ref version) = params.version {
        form_data.push(("version", version.clone()));
    }
    if let Some(ref changelog) = params.changelog {
        form_data.push(("changelog", changelog.clone()));
    }
    if let Some(active) = params.active {
        form_data.push((
            "active",
            if active {
                "true".into()
            } else {
                "false".into()
            },
        ));
    }

    let resp = client
        .http_client()
        .put(&url)
        .bearer_auth(token)
        .form(&form_data)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Edit file request failed: {e}")))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };
        Err(PlatformError::ApiError { status, message })
    }
}

/// Delete a modfile.
///
/// `DELETE /games/{game_id}/mods/{mod_id}/files/{file_id}` — requires OAuth2 token.
pub async fn delete_file(
    client: &ModioClientSnapshot,
    game_id: u64,
    mod_id: u64,
    file_id: u64,
) -> Result<(), PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required to delete files".into(),
    })?;

    let url = format!("{BASE_URL}/games/{game_id}/mods/{mod_id}/files/{file_id}");

    let resp = client
        .http_client()
        .delete(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Delete file request failed: {e}")))?;

    if resp.status().is_success() || resp.status().as_u16() == 204 {
        Ok(())
    } else {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };
        Err(PlatformError::ApiError { status, message })
    }
}
