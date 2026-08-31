//! IPC commands for mod.io authentication and management.

use std::sync::Mutex;

use serde::Deserialize;
use tauri::State;

use crate::error::AppError;
use crate::platform::credentials;
use crate::platform::modio::auth;
use crate::platform::modio::auth::ModioUserProfile;
use crate::platform::modio::client::ModioClient;
use crate::platform::modio::deps::{self, ModioDependency};
use crate::platform::modio::files::{self, EditFileParams, ModioFileEntry};
use crate::platform::modio::manage::{self, CreateModParams, EditModParams, ModioModResponse};
use crate::platform::modio::media::{self, AddMediaParams};
use crate::platform::modio::meta::{self, MetadataEntry};
use crate::platform::modio::mods::{self, ModioModSummary};
use crate::platform::modio::tags::{self, TagOption};
use crate::platform::modio::upload::{self, ModioModfileResponse, ModioUploadParams};

/// Credential service name.
const SERVICE: &str = "modio";

/// Keyring username for the API key.
const KEY_API_KEY: &str = "modio-api-key";

/// Keyring username for the OAuth2 token.
const KEY_OAUTH_TOKEN: &str = "modio-oauth-token";

/// Keyring username for the user ID.
const KEY_USER_ID: &str = "modio-user-id";

/// Default BG3 game ID on mod.io.
const BG3_GAME_ID: u64 = 6715;

/// Managed Tauri state holding the mod.io client.
pub struct ModioState {
    pub client: Mutex<Option<ModioClient>>,
}

// ── IPC Commands ────────────────────────────────────────────────────

/// Store the mod.io API key in the OS keyring and initialise the client.
#[tauri::command]
pub async fn cmd_modio_set_api_key(
    state: State<'_, ModioState>,
    api_key: String,
) -> Result<(), AppError> {
    credentials::store_credential(SERVICE, KEY_API_KEY, &api_key)?;

    let new_client = ModioClient::new(&api_key)?;
    let mut guard = state
        .client
        .lock()
        .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
    *guard = Some(new_client);
    Ok(())
}

/// Check whether an API key is stored in the keyring.
#[tauri::command]
pub async fn cmd_modio_has_api_key() -> Result<bool, AppError> {
    let val = credentials::get_credential(SERVICE, KEY_API_KEY)?;
    Ok(val.is_some())
}

/// Check whether an OAuth2 token is stored in the keyring.
#[tauri::command]
pub async fn cmd_modio_has_oauth_token() -> Result<bool, AppError> {
    let val = credentials::get_credential(SERVICE, KEY_OAUTH_TOKEN)?;
    Ok(val.is_some())
}

/// Store an OAuth2 Access Token and User ID, validate, and return the user profile.
///
/// This is the primary authentication path for third-party tools.
/// The user provides their User ID and OAuth2 Access Token from
/// <https://mod.io/me/access>.  We validate by calling `GET /me`
/// on the user-scoped subdomain `u-{user_id}.modapi.io`.
#[tauri::command]
pub async fn cmd_modio_set_oauth_token(
    state: State<'_, ModioState>,
    token: String,
    user_id: u64,
) -> Result<ModioUserProfile, AppError> {
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err(AppError::invalid_input("OAuth token must not be empty"));
    }
    if user_id == 0 {
        return Err(AppError::invalid_input("User ID must not be zero"));
    }

    // Build a client with user subdomain and validate via /me
    let new_client = ModioClient::with_user_token(&trimmed, user_id)?;
    let profile = auth::get_user(new_client.http_client(), &trimmed, user_id).await?;

    // Persist credentials in keyring
    credentials::store_credential(SERVICE, KEY_OAUTH_TOKEN, &trimmed)?;
    credentials::store_credential(SERVICE, KEY_USER_ID, &user_id.to_string())?;
    // Clear any old API key — no longer needed
    let _ = credentials::delete_credential(SERVICE, KEY_API_KEY);

    // Install the client
    let mut guard = state
        .client
        .lock()
        .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
    *guard = Some(new_client);

    Ok(profile)
}

/// Clear the stored mod.io API key and drop the client.
///
/// Unlike `cmd_modio_disconnect`, this only removes the API key and does NOT
/// attempt to revoke the OAuth token remotely. Use this when the user wants to
/// re-enter a different API key without going through the full disconnect flow.
#[tauri::command]
pub async fn cmd_modio_clear_api_key(state: State<'_, ModioState>) -> Result<(), AppError> {
    credentials::delete_credential(SERVICE, KEY_API_KEY)?;
    let _ = credentials::delete_credential(SERVICE, KEY_OAUTH_TOKEN);

    let mut guard = state
        .client
        .lock()
        .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
    *guard = None;

    Ok(())
}

/// Start the email authentication flow — sends a security code to the user's email.
#[tauri::command]
pub async fn cmd_modio_connect(
    state: State<'_, ModioState>,
    email: String,
) -> Result<(), AppError> {
    let (client_ref, api_key) = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        let c = guard.as_ref().ok_or_else(|| {
            AppError::invalid_input("mod.io API key not set — call cmd_modio_set_api_key first")
        })?;
        (c.http_client().clone(), c.api_key().to_string())
    };

    auth::email_request(&client_ref, &api_key, &email).await?;
    Ok(())
}

/// Exchange the emailed security code for an OAuth2 token.
///
/// On success the token is stored in the keyring and the user profile is returned.
#[tauri::command]
pub async fn cmd_modio_verify_code(
    state: State<'_, ModioState>,
    code: String,
) -> Result<ModioUserProfile, AppError> {
    let (client_ref, api_key) = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        let c = guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io API key not set"))?;
        (c.http_client().clone(), c.api_key().to_string())
    };

    // Exchange code for token
    let token = auth::email_exchange(&client_ref, &api_key, &code).await?;

    // Store token in keyring
    credentials::store_credential(SERVICE, KEY_OAUTH_TOKEN, &token)?;

    // We need the user's ID to use the user-scoped subdomain.
    // The email flow gives us a token from the game domain, so we use the
    // game domain's /me endpoint (which may still work with api_key auth).
    // First, try to get profile from the game domain with the token.
    let game_url = format!("https://g-{BG3_GAME_ID}.modapi.io/v1/me");
    let profile_resp = client_ref
        .get(&game_url)
        .header("Accept", "application/json")
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| AppError::internal(format!("Failed to get user from game domain: {e}")))?;

    let profile: ModioUserProfile = if profile_resp.status().is_success() {
        profile_resp
            .json()
            .await
            .map_err(|e| AppError::internal(format!("Failed to parse user profile: {e}")))?
    } else {
        return Err(AppError::internal(
            "Failed to fetch user profile after email exchange. Please try connecting with User ID + Access Token instead."
        ));
    };

    // Store user ID in keyring
    credentials::store_credential(SERVICE, KEY_USER_ID, &profile.id.to_string())?;

    // Set token and user ID on client
    {
        let mut guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        if let Some(c) = guard.as_mut() {
            c.set_token(&token);
            c.set_user_id(profile.id);
        }
    }

    Ok(profile)
}

/// Disconnect from mod.io: revoke token, clear credentials, drop client.
#[tauri::command]
pub async fn cmd_modio_disconnect(state: State<'_, ModioState>) -> Result<(), AppError> {
    // Best-effort remote logout
    let maybe_token = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard.as_ref().and_then(|c| {
            c.token()
                .map(|t| (c.http_client().clone(), t.to_string(), c.user_id()))
        })
    };

    if let Some((client_ref, token, Some(uid))) = maybe_token {
        let _ = auth::logout(&client_ref, &token, uid).await;
    }

    // Clear local credentials
    credentials::delete_credential(SERVICE, KEY_API_KEY)?;
    credentials::delete_credential(SERVICE, KEY_OAUTH_TOKEN)?;
    let _ = credentials::delete_credential(SERVICE, KEY_USER_ID);

    // Drop client
    let mut guard = state
        .client
        .lock()
        .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
    *guard = None;

    Ok(())
}

/// Fetch the authenticated user's profile.
#[tauri::command]
pub async fn cmd_modio_get_user(
    state: State<'_, ModioState>,
) -> Result<ModioUserProfile, AppError> {
    let (client_ref, token, user_id) = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        let c = guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?;
        let t = c.token().ok_or_else(|| {
            AppError::invalid_input("Not authenticated with mod.io — no OAuth2 token")
        })?;
        let uid = c
            .user_id()
            .ok_or_else(|| AppError::invalid_input("User ID not set — reconnect to mod.io"))?;
        (c.http_client().clone(), t.to_string(), uid)
    };

    let profile = auth::get_user(&client_ref, &token, user_id).await?;
    Ok(profile)
}

/// List the authenticated user's mods for BG3 (or another game).
#[tauri::command]
pub async fn cmd_modio_get_my_mods(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
) -> Result<Vec<ModioModSummary>, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    // Acquire read rate limit
    client_snapshot.read_limiter.acquire().await;

    let gid = game_id.unwrap_or(BG3_GAME_ID);
    let result = mods::get_my_mods(&client_snapshot, gid).await?;
    Ok(result)
}

/// Resolve a single mod by its numeric ID.
#[tauri::command]
pub async fn cmd_modio_get_mod(
    state: State<'_, ModioState>,
    mod_id: u64,
    game_id: Option<u64>,
) -> Result<ModioModSummary, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.read_limiter.acquire().await;

    let gid = game_id.unwrap_or(BG3_GAME_ID);
    let result = mods::get_mod(&client_snapshot, gid, mod_id).await?;
    Ok(result)
}

/// Resolve a mod by its `name_id` slug (used when linking from a mod.io URL).
#[tauri::command]
pub async fn cmd_modio_get_mod_by_name_id(
    state: State<'_, ModioState>,
    name_id: String,
    game_id: Option<u64>,
) -> Result<ModioModSummary, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.read_limiter.acquire().await;

    let gid = game_id.unwrap_or(BG3_GAME_ID);
    let result = mods::get_mod_by_name_id(&client_snapshot, gid, &name_id).await?;
    Ok(result)
}

/// Upload a file to an existing mod on mod.io.
#[tauri::command]
pub async fn cmd_modio_upload_file(
    app: tauri::AppHandle,
    state: State<'_, ModioState>,
    params: ModioUploadParams,
) -> Result<ModioModfileResponse, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    // Acquire write rate limit
    client_snapshot.write_limiter.acquire().await;

    let result = upload::upload_file(&client_snapshot, &params, &app).await?;
    Ok(result)
}

/// Try to auto-initialise the ModioClient from stored keyring credentials.
///
/// Called during app setup. Silently returns `Ok(())` if no credentials are found.
pub fn try_restore_client(state: &ModioState) -> Result<(), AppError> {
    // First, try to restore from user_id + token (the primary auth path)
    if let (Some(token), Some(uid_str)) = (
        credentials::get_credential(SERVICE, KEY_OAUTH_TOKEN)?,
        credentials::get_credential(SERVICE, KEY_USER_ID)?,
    ) {
        if let Ok(user_id) = uid_str.parse::<u64>() {
            let client = ModioClient::with_user_token(&token, user_id)?;
            let mut guard = state
                .client
                .lock()
                .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
            *guard = Some(client);
            return Ok(());
        }
    }

    // Fallback: restore from api_key (legacy email flow)
    let api_key = credentials::get_credential(SERVICE, KEY_API_KEY)?;
    let api_key = match api_key {
        Some(k) => k,
        None => return Ok(()),
    };

    let mut client = ModioClient::new(&api_key)?;

    if let Some(token) = credentials::get_credential(SERVICE, KEY_OAUTH_TOKEN)? {
        client.set_token(&token);
    }
    if let Some(uid_str) = credentials::get_credential(SERVICE, KEY_USER_ID)? {
        if let Ok(user_id) = uid_str.parse::<u64>() {
            client.set_user_id(user_id);
        }
    }

    let mut guard = state
        .client
        .lock()
        .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
    *guard = Some(client);
    Ok(())
}

// ── Mod Profile Management ──────────────────────────────────────────

/// Create a new mod profile on mod.io.
#[tauri::command]
pub async fn cmd_modio_create_mod(
    state: State<'_, ModioState>,
    params: CreateModParams,
) -> Result<ModioModResponse, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let result = manage::create_mod(&client_snapshot, &params).await?;
    Ok(result)
}

/// Edit an existing mod profile on mod.io.
#[tauri::command]
pub async fn cmd_modio_edit_mod(
    state: State<'_, ModioState>,
    params: EditModParams,
) -> Result<ModioModResponse, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let result = manage::edit_mod(&client_snapshot, &params).await?;
    Ok(result)
}

// ── Media Management ────────────────────────────────────────────────

/// Add images, logo, or YouTube links to a mod.
#[tauri::command]
pub async fn cmd_modio_add_media(
    state: State<'_, ModioState>,
    params: AddMediaParams,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    media::add_media(&client_snapshot, &params).await?;
    Ok(())
}

/// Delete images from a mod by filename.
#[tauri::command]
pub async fn cmd_modio_delete_media(
    state: State<'_, ModioState>,
    game_id: u64,
    mod_id: u64,
    filenames: Vec<String>,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    media::delete_media(&client_snapshot, game_id, mod_id, &filenames).await?;
    Ok(())
}

// ── File/Version Management ─────────────────────────────────────────

/// List all files for a mod.
#[tauri::command]
pub async fn cmd_modio_list_files(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
) -> Result<Vec<ModioFileEntry>, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.read_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    let result = files::list_files(&client_snapshot, gid, mod_id).await?;
    Ok(result)
}

/// Edit a modfile's metadata (version, changelog, active status).
#[tauri::command]
pub async fn cmd_modio_edit_file(
    state: State<'_, ModioState>,
    params: EditFileParams,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    files::edit_file(&client_snapshot, &params).await?;
    Ok(())
}

/// Delete a modfile.
#[tauri::command]
pub async fn cmd_modio_delete_file(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
    file_id: u64,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    files::delete_file(&client_snapshot, gid, mod_id, file_id).await?;
    Ok(())
}

// ── Dependency Management ───────────────────────────────────────────

/// Get dependencies for a mod.
#[tauri::command]
pub async fn cmd_modio_get_dependencies(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
) -> Result<Vec<ModioDependency>, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.read_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    let result = deps::get_dependencies(&client_snapshot, gid, mod_id).await?;
    Ok(result)
}

/// Add dependencies to a mod.
#[tauri::command]
pub async fn cmd_modio_add_dependencies(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
    dependency_ids: Vec<u64>,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    deps::add_dependencies(&client_snapshot, gid, mod_id, &dependency_ids).await?;
    Ok(())
}

/// Remove dependencies from a mod.
#[tauri::command]
pub async fn cmd_modio_remove_dependencies(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
    dependency_ids: Vec<u64>,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    deps::remove_dependencies(&client_snapshot, gid, mod_id, &dependency_ids).await?;
    Ok(())
}

// ── Tag Management ──────────────────────────────────────────────────

/// Get available tag options for a game.
#[tauri::command]
pub async fn cmd_modio_get_game_tags(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
) -> Result<Vec<TagOption>, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.read_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    let result = tags::get_game_tags(&client_snapshot, gid).await?;
    Ok(result)
}

/// Add tags to a mod.
#[tauri::command]
pub async fn cmd_modio_add_tags(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
    tags: Vec<String>,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    tags::add_tags(&client_snapshot, gid, mod_id, &tags).await?;
    Ok(())
}

/// Remove tags from a mod.
#[tauri::command]
pub async fn cmd_modio_remove_tags(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
    tags: Vec<String>,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    tags::remove_tags(&client_snapshot, gid, mod_id, &tags).await?;
    Ok(())
}

// ── Metadata KVP ────────────────────────────────────────────────────

/// Get metadata key-value pairs for a mod.
#[tauri::command]
pub async fn cmd_modio_get_metadata(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
) -> Result<Vec<MetadataEntry>, AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.read_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    let result = meta::get_metadata(&client_snapshot, gid, mod_id).await?;
    Ok(result)
}

/// Add metadata key-value pairs to a mod.
#[tauri::command]
pub async fn cmd_modio_add_metadata(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
    entries: Vec<MetadataEntry>,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    meta::add_metadata(&client_snapshot, gid, mod_id, &entries).await?;
    Ok(())
}

/// Remove metadata key-value pairs from a mod.
#[tauri::command]
pub async fn cmd_modio_remove_metadata(
    state: State<'_, ModioState>,
    game_id: Option<u64>,
    mod_id: u64,
    entries: Vec<MetadataEntry>,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    client_snapshot.write_limiter.acquire().await;
    let gid = game_id.unwrap_or(BG3_GAME_ID);
    meta::remove_metadata(&client_snapshot, gid, mod_id, &entries).await?;
    Ok(())
}

// ── Integrated Package + Upload ─────────────────────────────────────

/// Parameters for integrated package-and-upload to mod.io.
#[derive(Debug, Deserialize)]
pub struct ModioPackageUploadParams {
    pub source_dir: String,
    pub mod_id: u64,
    pub version: String,
    pub changelog: Option<String>,
    pub active: Option<bool>,
    pub exclude_patterns: Option<Vec<String>>,
}

/// Package a mod directory into a zip and upload it to mod.io in one step.
#[tauri::command]
pub async fn cmd_modio_package_and_upload(
    app: tauri::AppHandle,
    state: State<'_, ModioState>,
    params: ModioPackageUploadParams,
) -> Result<(), AppError> {
    let client_snapshot = {
        let guard = state
            .client
            .lock()
            .map_err(|e| AppError::internal(format!("Failed to lock ModioState: {e}")))?;
        guard
            .as_ref()
            .ok_or_else(|| AppError::invalid_input("mod.io client not initialised"))?
            .clone_snapshot()
    };

    // Create a temp directory for the zip.
    let temp_dir = std::env::temp_dir().join("cmty-studio-upload");
    std::fs::create_dir_all(&temp_dir).map_err(|e| {
        crate::platform::errors::PlatformError::IoError(format!("Failed to create temp dir: {e}"))
    })?;

    let zip_name = format!("modio-{}-{}.zip", params.mod_id, &params.version);
    let zip_path = temp_dir.join(&zip_name);

    let excludes: Vec<&str> = params
        .exclude_patterns
        .as_ref()
        .map(|v| v.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let _pkg = crate::platform::packaging::create_upload_zip(
        std::path::Path::new(&params.source_dir),
        &zip_path,
        &excludes,
    )?;

    // Acquire write rate limit before upload.
    client_snapshot.write_limiter.acquire().await;

    let upload_params = upload::ModioUploadParams {
        game_id: BG3_GAME_ID,
        mod_id: params.mod_id,
        file_path: zip_path.to_string_lossy().to_string(),
        version: params.version,
        changelog: params.changelog,
        active: params.active,
        metadata_blob: None,
    };

    let result = upload::upload_file(&client_snapshot, &upload_params, &app).await;

    // Clean up the temp zip regardless of outcome.
    let _ = std::fs::remove_file(&zip_path);

    result.map(|_| ()).map_err(AppError::from)
}
