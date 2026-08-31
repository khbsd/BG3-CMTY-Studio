//! Mod resolution: URL/ID → Nexus mod details.

use serde::Serialize;

use crate::platform::errors::PlatformError;

use super::client::NexusClient;

/// Default game domain when none is provided in the URL.
const DEFAULT_GAME_DOMAIN: &str = "baldursgate3";

/// Details of a resolved Nexus mod.
#[derive(Debug, Clone, Serialize)]
pub struct NexusModDetails {
    /// Internal UUID of the mod on Nexus.
    pub id: String,
    /// Game-scoped numeric mod ID (the number in the URL).
    pub game_scoped_id: u64,
    /// Human-readable mod name.
    pub name: String,
    /// Short summary text from the mod page.
    pub summary: Option<String>,
    /// Thumbnail / main image URL.
    pub thumbnail_url: Option<String>,
    /// Whether the mod is flagged as containing adult content.
    pub contains_adult_content: bool,
    /// Category name (mapped from category_id).
    pub category_name: Option<String>,
    /// Tags associated with the mod.
    pub tags: Vec<String>,
}

/// Parse a user-provided URL or bare ID into `(game_domain, game_scoped_id)`.
///
/// Accepted formats:
/// - `https://www.nexusmods.com/baldursgate3/mods/1933?tab=files`
/// - `baldursgate3/mods/1933`
/// - `1933`
fn parse_mod_input(url_or_id: &str) -> Result<(String, u64), PlatformError> {
    let input = url_or_id.trim().trim_end_matches('/');

    // Try bare numeric ID first.
    if let Ok(id) = input.parse::<u64>() {
        return Ok((DEFAULT_GAME_DOMAIN.to_string(), id));
    }

    // Strip URL scheme + host if present.
    let path = if let Some(rest) = input
        .strip_prefix("https://www.nexusmods.com/")
        .or_else(|| input.strip_prefix("https://nexusmods.com/"))
        .or_else(|| input.strip_prefix("http://www.nexusmods.com/"))
        .or_else(|| input.strip_prefix("http://nexusmods.com/"))
    {
        rest
    } else {
        input
    };

    // Strip query params.
    let path = path.split('?').next().unwrap_or(path);
    let path = path.trim_end_matches('/');

    // Expected: `{game_domain}/mods/{id}`
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 3 && parts[1].eq_ignore_ascii_case("mods") {
        let domain = parts[0].to_string();
        let id = parts[2].parse::<u64>().map_err(|_| {
            PlatformError::ValidationError(format!(
                "Cannot parse mod ID from '{}' — expected a number",
                parts[2]
            ))
        })?;
        return Ok((domain, id));
    }

    Err(PlatformError::ValidationError(format!(
        "Unrecognized mod URL or ID: '{url_or_id}'. \
         Expected a Nexus URL (https://www.nexusmods.com/baldursgate3/mods/1933), \
         a partial path (baldursgate3/mods/1933), or a bare numeric ID (1933)."
    )))
}

/// Resolve a Nexus mod by URL or numeric ID.
///
/// Makes a `GET /v1/games/{domain}/mods/{id}.json` API call (v1 REST)
/// and returns the mod's UUID, scoped ID, and name.
///
/// The v1 endpoint is used because mod resolution needs the `uid` field
/// which is reliably present in v1 responses. The v3 API is GraphQL-based
/// and doesn't have a direct REST equivalent.
pub async fn resolve_mod(
    client: &NexusClient,
    url_or_id: &str,
) -> Result<NexusModDetails, PlatformError> {
    let (domain, scoped_id) = parse_mod_input(url_or_id)?;

    // Use v1 REST endpoint directly (same pattern as validate_api_key)
    let url = format!("https://api.nexusmods.com/v1/games/{domain}/mods/{scoped_id}.json");
    let resp = client.inner().get(&url).send().await.map_err(|e| {
        if e.is_timeout() {
            PlatformError::Timeout
        } else {
            PlatformError::HttpError(format!("GET {url}: {e}"))
        }
    })?;

    match resp.status().as_u16() {
        200 => {}
        404 => {
            return Err(PlatformError::ApiError {
                status: 404,
                message: format!("Mod {scoped_id} not found on {domain}"),
            })
        }
        401 | 403 => {
            return Err(PlatformError::ApiError {
                status: resp.status().as_u16(),
                message: "API key is invalid or lacks permission".into(),
            })
        }
        code => {
            let body = resp.text().await.unwrap_or_default();
            return Err(PlatformError::ApiError {
                status: code,
                message: body,
            });
        }
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Failed to parse mod response: {e}")))?;

    // v1 returns `uid` (can be numeric or string) and `mod_id` (same as scoped_id).
    // Try string first, then fall back to numeric conversion.
    let id = json["uid"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| json["uid"].as_u64().map(|n| n.to_string()))
        .or_else(|| json["uid"].as_i64().map(|n| n.to_string()))
        .or_else(|| json["uuid"].as_str().map(|s| s.to_string()))
        .or_else(|| json["id"].as_str().map(|s| s.to_string()))
        .or_else(|| json["id"].as_u64().map(|n| n.to_string()))
        .ok_or_else(|| PlatformError::ApiError {
            status: 200,
            message: format!(
                "Nexus API response missing uid field. Keys present: {:?}",
                json.as_object()
                    .map(|o| o.keys().collect::<Vec<_>>())
                    .unwrap_or_default()
            ),
        })?;

    let name = json["name"].as_str().unwrap_or("Unknown").to_string();

    let summary = json["summary"].as_str().map(|s| s.to_string());

    let thumbnail_url = json["picture_url"].as_str().map(|s| s.to_string());

    let contains_adult_content = json["contains_adult_content"].as_bool().unwrap_or(false);

    let category_name = json["category_name"].as_str().map(|s| s.to_string());

    let tags = json["tags"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // The v1 `uid` is a large numeric ID, NOT a v3 UUID. The v3 endpoints
    // (`/mods/{uuid}/file-update-groups`, etc.) require the real v3 UUID.
    // Resolve it via `GET /games/{domain}/mods/{scoped_id}`.
    let v3_uuid = match resolve_v3_uuid(client, &domain, scoped_id).await {
        Ok(uuid) => uuid,
        Err(e) => {
            tracing::warn!("nexus-mods: v3 UUID resolution failed, falling back to v1 uid: {e}");
            id
        }
    };

    Ok(NexusModDetails {
        id: v3_uuid,
        game_scoped_id: scoped_id,
        name,
        summary,
        thumbnail_url,
        contains_adult_content,
        category_name,
        tags,
    })
}

/// Resolve a mod's v3 UUID from its game domain and scoped ID.
///
/// `GET /games/{domain}/mods/{scoped_id}` (v3 REST)
async fn resolve_v3_uuid(
    client: &NexusClient,
    domain: &str,
    scoped_id: u64,
) -> Result<String, PlatformError> {
    let path = format!("/games/{domain}/mods/{scoped_id}");
    let resp = client.get(&path).await?;
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Failed to parse v3 mod response: {e}")))?;

    json["data"]["id"]
        .as_str()
        .or_else(|| json["id"].as_str())
        .map(String::from)
        .ok_or_else(|| PlatformError::ApiError {
            status: 200,
            message: format!(
                "v3 mod response missing UUID. Body: {}",
                serde_json::to_string(&json)
                    .unwrap_or_default()
                    .chars()
                    .take(500)
                    .collect::<String>()
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_id() {
        let (domain, id) = parse_mod_input("1933").unwrap();
        assert_eq!(domain, "baldursgate3");
        assert_eq!(id, 1933);
    }

    #[test]
    fn parse_full_url() {
        let (domain, id) =
            parse_mod_input("https://www.nexusmods.com/baldursgate3/mods/1933?tab=files").unwrap();
        assert_eq!(domain, "baldursgate3");
        assert_eq!(id, 1933);
    }

    #[test]
    fn parse_partial_path() {
        let (domain, id) = parse_mod_input("baldursgate3/mods/1933").unwrap();
        assert_eq!(domain, "baldursgate3");
        assert_eq!(id, 1933);
    }

    #[test]
    fn parse_trailing_slash() {
        let (domain, id) =
            parse_mod_input("https://www.nexusmods.com/baldursgate3/mods/1933/").unwrap();
        assert_eq!(domain, "baldursgate3");
        assert_eq!(id, 1933);
    }

    #[test]
    fn parse_garbage_fails() {
        assert!(parse_mod_input("not-a-url-at-all").is_err());
    }
}
