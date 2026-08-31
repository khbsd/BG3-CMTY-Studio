//! mod.io tag management — get available tags, add/remove tags on mods.

use serde::{Deserialize, Serialize};

use crate::platform::errors::PlatformError;
use crate::platform::modio::client::{ModioClientSnapshot, BASE_URL};

// ── Response types ──────────────────────────────────────────────────

/// A tag option group defined by a game on mod.io.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagOption {
    pub name: String,
    #[serde(default, rename = "type")]
    pub tag_type: String,
    #[serde(default)]
    pub tags: Vec<String>,
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
    data: Vec<TagOption>,
}

// ── Public API ──────────────────────────────────────────────────────

/// Get the available tag options for a game.
///
/// `GET /games/{game_id}/tags` — uses API key or Bearer token.
pub async fn get_game_tags(
    client: &ModioClientSnapshot,
    game_id: u64,
) -> Result<Vec<TagOption>, PlatformError> {
    let url = format!("{BASE_URL}/games/{game_id}/tags");

    let mut req = client.http_client().get(&url);
    if let Some(token) = client.token() {
        req = req.bearer_auth(token);
    } else {
        req = req.query(&[("api_key", client.api_key())]);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Get game tags request failed: {e}")))?;

    if resp.status().is_success() {
        let body: PaginatedResponse = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse tags response: {e}")))?;
        Ok(body.data)
    } else {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };
        Err(PlatformError::ApiError { status, message })
    }
}

/// Add tags to a mod.
///
/// `POST /games/{game_id}/mods/{mod_id}/tags` — requires OAuth2 token.
pub async fn add_tags(
    client: &ModioClientSnapshot,
    game_id: u64,
    mod_id: u64,
    tags: &[String],
) -> Result<(), PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required to add tags".into(),
    })?;

    let url = format!("{BASE_URL}/games/{game_id}/mods/{mod_id}/tags");

    let form_data: Vec<(&str, &str)> = tags.iter().map(|t| ("tags[]", t.as_str())).collect();

    let resp = client
        .http_client()
        .post(&url)
        .bearer_auth(token)
        .form(&form_data)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Add tags request failed: {e}")))?;

    if resp.status().is_success() || resp.status().as_u16() == 201 {
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

/// Remove tags from a mod.
///
/// `DELETE /games/{game_id}/mods/{mod_id}/tags` — requires OAuth2 token.
pub async fn remove_tags(
    client: &ModioClientSnapshot,
    game_id: u64,
    mod_id: u64,
    tags: &[String],
) -> Result<(), PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required to remove tags".into(),
    })?;

    let url = format!("{BASE_URL}/games/{game_id}/mods/{mod_id}/tags");

    let form_data: Vec<(&str, &str)> = tags.iter().map(|t| ("tags[]", t.as_str())).collect();

    let resp = client
        .http_client()
        .delete(&url)
        .bearer_auth(token)
        .form(&form_data)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Remove tags request failed: {e}")))?;

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
