//! mod.io email-based OAuth2 authentication flow.

use serde::{Deserialize, Serialize};

use super::client::{user_base_url, BASE_URL};
use crate::platform::errors::PlatformError;

/// Avatar object nested inside the mod.io user response.
#[derive(Debug, Clone, Default, Deserialize)]
struct AvatarObject {
    #[serde(default)]
    thumb_100x100: String,
    #[serde(default)]
    original: String,
}

/// Raw user response from the mod.io `/me` endpoint.
#[derive(Debug, Deserialize)]
struct RawUserProfile {
    id: u64,
    #[serde(alias = "username")]
    name: String,
    #[serde(default)]
    avatar: Option<AvatarObject>,
    date_online: u64,
}

/// User profile returned by the mod.io `/me` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModioUserProfile {
    pub id: u64,
    #[serde(alias = "username")]
    pub name: String,
    #[serde(default)]
    pub avatar_url: String,
    pub date_online: u64,
}

// ── Response wrappers (mod.io envelope) ─────────────────────────────

#[derive(Deserialize)]
struct EmailRequestResponse {
    #[allow(dead_code)]
    message: String,
}

#[derive(Deserialize)]
struct EmailExchangeResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct ErrorObject {
    message: String,
}

#[derive(Deserialize)]
struct ModioErrorResponse {
    error: ErrorObject,
}

// ── Public API ──────────────────────────────────────────────────────

/// Step 1: Request a security code be sent to the user's email.
///
/// `POST /oauth/emailrequest?api_key=…` with `email` as form data.
pub async fn email_request(
    client: &reqwest::Client,
    api_key: &str,
    email: &str,
) -> Result<(), PlatformError> {
    let url = format!("{BASE_URL}/oauth/emailrequest");
    let resp = client
        .post(&url)
        .query(&[("api_key", api_key)])
        .form(&[("email", email)])
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Email request failed: {e}")))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(extract_api_error(resp).await)
    }
}

/// Step 2: Exchange the emailed security code for an OAuth2 access token.
///
/// `POST /oauth/emailexchange?api_key=…` with `security_code` as form data.
/// Returns the OAuth2 bearer token.
pub async fn email_exchange(
    client: &reqwest::Client,
    api_key: &str,
    code: &str,
) -> Result<String, PlatformError> {
    let url = format!("{BASE_URL}/oauth/emailexchange");
    let resp = client
        .post(&url)
        .query(&[("api_key", api_key)])
        .form(&[("security_code", code)])
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Email exchange failed: {e}")))?;

    if resp.status().is_success() {
        let body: EmailExchangeResponse = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse token response: {e}"))
        })?;
        Ok(body.access_token)
    } else {
        Err(extract_api_error(resp).await)
    }
}

/// Fetch the authenticated user's profile.
///
/// `GET /me` with `Authorization: Bearer {token}`.
///
/// Uses the user-scoped subdomain (`u-{user_id}.modapi.io`)
/// which is required for `/me` endpoints with Bearer tokens.
pub async fn get_user(
    client: &reqwest::Client,
    token: &str,
    user_id: u64,
) -> Result<ModioUserProfile, PlatformError> {
    let url = format!("{}/me", user_base_url(user_id));

    let resp = client
        .get(&url)
        .header("Accept", "application/json")
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Get user failed: {e}")))?;

    if resp.status().is_success() {
        return parse_user_profile(resp).await;
    }

    Err(extract_api_error(resp).await)
}

async fn parse_user_profile(resp: reqwest::Response) -> Result<ModioUserProfile, PlatformError> {
    let raw: RawUserProfile = resp
        .json()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Failed to parse user profile: {e}")))?;

    let avatar_url = raw
        .avatar
        .map(|a| {
            if !a.thumb_100x100.is_empty() {
                a.thumb_100x100
            } else {
                a.original
            }
        })
        .unwrap_or_default();

    Ok(ModioUserProfile {
        id: raw.id,
        name: raw.name,
        avatar_url,
        date_online: raw.date_online,
    })
}

/// Revoke the current OAuth2 token.
///
/// `POST /oauth/logout` with `Authorization: Bearer {token}`.
///
/// Uses the user-scoped subdomain (`u-{user_id}.modapi.io`).
pub async fn logout(
    client: &reqwest::Client,
    token: &str,
    user_id: u64,
) -> Result<(), PlatformError> {
    let url = format!("{}/oauth/logout", user_base_url(user_id));
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Logout failed: {e}")))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        // Best-effort: if logout fails we still clear local state
        let _ = extract_api_error(resp).await;
        Ok(())
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

async fn extract_api_error(resp: reqwest::Response) -> PlatformError {
    let status = resp.status().as_u16();
    match resp.json::<ModioErrorResponse>().await {
        Ok(body) => PlatformError::ApiError {
            status,
            message: body.error.message,
        },
        Err(_) => PlatformError::ApiError {
            status,
            message: format!("HTTP {status}"),
        },
    }
}
