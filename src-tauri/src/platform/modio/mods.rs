//! mod.io "My Mods" API — list the authenticated user's mods.

use serde::{Deserialize, Serialize};

use crate::platform::errors::PlatformError;
use crate::platform::modio::client::{ModioClientSnapshot, BASE_URL};

/// Summary of a single mod on mod.io.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModioModSummary {
    pub id: u64,
    pub name: String,
    pub name_id: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub logo_url: String,
    #[serde(default)]
    pub status: u8,
    #[serde(default)]
    pub visibility: u8,
    #[serde(default)]
    pub date_added: u64,
    #[serde(default)]
    pub date_updated: u64,
    #[serde(default)]
    pub stats: ModioModStats,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub media_images: Vec<ModioImage>,
}

/// An image from the mod's media gallery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModioImage {
    pub filename: String,
    pub original: String,
    #[serde(default)]
    pub thumb_320x180: String,
}

/// Aggregate stats for a mod.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModioModStats {
    #[serde(default)]
    pub downloads_total: u64,
    #[serde(default)]
    pub subscribers_total: u64,
    #[serde(default)]
    pub ratings_positive: u64,
    #[serde(default)]
    pub ratings_negative: u64,
}

// ── mod.io response wrappers ────────────────────────────────────────

/// mod.io wraps logo in a nested object.
#[derive(Deserialize)]
struct LogoObject {
    #[serde(default)]
    thumb_640x360: String,
}

/// mod.io wraps stats in a nested object.
#[derive(Deserialize)]
struct StatsObject {
    #[serde(default)]
    downloads_total: u64,
    #[serde(default)]
    subscribers_total: u64,
    #[serde(default)]
    ratings_positive: u64,
    #[serde(default)]
    ratings_negative: u64,
}

/// A single tag entry from the mod.io API.
#[derive(Deserialize)]
struct TagEntry {
    name: String,
}

/// Image entry from the mod.io media object.
#[derive(Deserialize)]
struct RawImageEntry {
    #[serde(default)]
    filename: String,
    #[serde(default)]
    original: String,
    #[serde(default)]
    thumb_320x180: String,
}

/// Media container from mod.io API.
#[derive(Deserialize, Default)]
struct MediaObject {
    #[serde(default)]
    images: Vec<RawImageEntry>,
}

/// Raw mod entry from the mod.io API response.
#[derive(Deserialize)]
struct RawMod {
    id: u64,
    name: String,
    name_id: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    logo: Option<LogoObject>,
    #[serde(default)]
    status: u8,
    #[serde(default)]
    visible: u8,
    #[serde(default)]
    date_added: u64,
    #[serde(default)]
    date_updated: u64,
    #[serde(default)]
    stats: Option<StatsObject>,
    #[serde(default)]
    tags: Vec<TagEntry>,
    #[serde(default)]
    media: Option<MediaObject>,
}

/// Paginated response envelope from mod.io.
#[derive(Deserialize)]
struct PaginatedResponse {
    data: Vec<RawMod>,
    result_count: usize,
    result_total: usize,
    #[serde(default)]
    #[allow(dead_code)]
    result_offset: usize,
}

impl RawMod {
    fn into_summary(self) -> ModioModSummary {
        let logo_url = self.logo.map(|l| l.thumb_640x360).unwrap_or_default();
        let stats = self
            .stats
            .map(|s| ModioModStats {
                downloads_total: s.downloads_total,
                subscribers_total: s.subscribers_total,
                ratings_positive: s.ratings_positive,
                ratings_negative: s.ratings_negative,
            })
            .unwrap_or_default();
        let tags = self.tags.into_iter().map(|t| t.name).collect();
        let media_images = self
            .media
            .map(|m| {
                m.images
                    .into_iter()
                    .map(|img| ModioImage {
                        filename: img.filename,
                        original: img.original,
                        thumb_320x180: img.thumb_320x180,
                    })
                    .collect()
            })
            .unwrap_or_default();
        ModioModSummary {
            id: self.id,
            name: self.name,
            name_id: self.name_id,
            summary: self.summary,
            logo_url,
            status: self.status,
            visibility: self.visible,
            date_added: self.date_added,
            date_updated: self.date_updated,
            stats,
            tags,
            media_images,
        }
    }
}

// ── Error helpers ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct ErrorObject {
    message: String,
}

#[derive(Deserialize)]
struct ModioErrorResponse {
    error: ErrorObject,
}

// ── Public API ──────────────────────────────────────────────────────

/// Fetch all mods owned by the authenticated user for a given game.
///
/// Uses the game-scoped subdomain (`g-{game_id}.modapi.io`) for `/me/mods`.
/// The game subdomain implicitly scopes results to that game, so no
/// `game_id` query parameter is needed.
/// Handles mod.io pagination (100 results per page).
pub async fn get_my_mods(
    client: &ModioClientSnapshot,
    _game_id: u64,
) -> Result<Vec<ModioModSummary>, PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required".into(),
    })?;

    let mut all_mods = Vec::new();
    let mut offset: usize = 0;
    let limit: usize = 100;

    loop {
        // /me/mods on the game subdomain — game is implicit from the subdomain.
        let url = format!("{BASE_URL}/me/mods?_limit={limit}&_offset={offset}");

        eprintln!("[modio] GET {url}");

        let resp = client
            .http_client()
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Get my mods failed: {e}")))?;

        let status_code = resp.status();
        eprintln!("[modio] /me/mods response: HTTP {status_code}");

        if !status_code.is_success() {
            let status = status_code.as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            eprintln!("[modio] /me/mods error body: {body_text}");
            let message = serde_json::from_str::<ModioErrorResponse>(&body_text)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| format!("HTTP {status}"));
            return Err(PlatformError::ApiError { status, message });
        }

        let body_text = resp
            .text()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to read response body: {e}")))?;
        eprintln!(
            "[modio] /me/mods body (first 500 chars): {}",
            &body_text[..body_text.len().min(500)]
        );

        let page: PaginatedResponse = serde_json::from_str(&body_text)
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse mods response: {e}")))?;

        let count = page.result_count;
        let total = page.result_total;

        for raw in page.data {
            all_mods.push(raw.into_summary());
        }

        offset += count;
        if offset >= total || count == 0 {
            break;
        }
    }

    Ok(all_mods)
}

/// Fetch a single mod by ID.
///
/// Uses the game-scoped subdomain (`g-6715.modapi.io`) for
/// `GET /games/{game_id}/mods/{mod_id}`.
pub async fn get_mod(
    client: &ModioClientSnapshot,
    game_id: u64,
    mod_id: u64,
) -> Result<ModioModSummary, PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required".into(),
    })?;

    let url = format!("{BASE_URL}/games/{game_id}/mods/{mod_id}");

    let resp = client
        .http_client()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Get mod failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };
        return Err(PlatformError::ApiError { status, message });
    }

    let raw: RawMod = resp
        .json()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Failed to parse mod response: {e}")))?;

    Ok(raw.into_summary())
}

/// Fetch a single mod by its `name_id` slug.
///
/// Uses `GET /games/{game_id}/mods?name_id={name_id}` on the game-scoped
/// subdomain. Returns the first matching mod (name_id is unique per game).
pub async fn get_mod_by_name_id(
    client: &ModioClientSnapshot,
    game_id: u64,
    name_id: &str,
) -> Result<ModioModSummary, PlatformError> {
    let token = client.token().ok_or_else(|| PlatformError::ApiError {
        status: 401,
        message: "Not authenticated — OAuth2 token required".into(),
    })?;

    let url = format!("{BASE_URL}/games/{game_id}/mods?name_id={name_id}&_limit=1");

    let resp = client
        .http_client()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Get mod by name_id failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let message = match resp.json::<ModioErrorResponse>().await {
            Ok(body) => body.error.message,
            Err(_) => format!("HTTP {status}"),
        };
        return Err(PlatformError::ApiError { status, message });
    }

    let page: PaginatedResponse = resp
        .json()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Failed to parse mod response: {e}")))?;

    page.data
        .into_iter()
        .next()
        .map(|raw| raw.into_summary())
        .ok_or_else(|| PlatformError::ApiError {
            status: 404,
            message: format!("No mod found with name_id '{name_id}'"),
        })
}
