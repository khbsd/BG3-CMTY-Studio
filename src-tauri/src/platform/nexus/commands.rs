//! Tauri IPC commands for Nexus Mods integration.

use std::path::Path;
use std::sync::Mutex;

use serde::Deserialize;
use tauri::State;

use crate::error::AppError;
use crate::platform::credentials;
use crate::platform::errors::PlatformError;

use super::client::NexusClient;
use super::dependencies::NexusDependency;
use super::mod_files::FileUpdateGroup;
use super::mod_files::FileVersion;
use super::mods::NexusModDetails;
use super::upload::NexusUploadParams;

/// Keyring service/username for the Nexus API key.
const NEXUS_SERVICE: &str = "nexus";
const NEXUS_USERNAME: &str = "nexus-api-key";

/// Managed state holding the optional, lazily-initialised Nexus client.
pub struct NexusState {
    pub client: Mutex<Option<NexusClient>>,
}

// ── API Key Management ───────────────────────────────────────────────

/// Store a Nexus API key in the OS keyring and initialise the client.
#[tauri::command]
pub async fn cmd_nexus_set_api_key(
    state: State<'_, NexusState>,
    api_key: String,
) -> Result<(), AppError> {
    if api_key.trim().is_empty() {
        return Err(PlatformError::ValidationError("API key must not be empty".into()).into());
    }

    let key = api_key.trim().to_string();

    // Store in keyring (blocking I/O — run on blocking pool).
    let key_clone = key.clone();
    crate::blocking(move || {
        credentials::store_credential(NEXUS_SERVICE, NEXUS_USERNAME, &key_clone)
            .map_err(|e| e.to_string())
    })
    .await?;

    // Create and store the client.
    let client = NexusClient::new(&key)?;
    let mut guard = state
        .client
        .lock()
        .map_err(|_| AppError::internal("NexusState lock poisoned"))?;
    *guard = Some(client);

    Ok(())
}

/// Delete the stored Nexus API key and tear down the client.
#[tauri::command]
pub async fn cmd_nexus_clear_api_key(state: State<'_, NexusState>) -> Result<(), AppError> {
    crate::blocking(move || {
        credentials::delete_credential(NEXUS_SERVICE, NEXUS_USERNAME).map_err(|e| e.to_string())
    })
    .await?;

    let mut guard = state
        .client
        .lock()
        .map_err(|_| AppError::internal("NexusState lock poisoned"))?;
    *guard = None;

    Ok(())
}

/// Check whether a Nexus API key is stored (does not reveal it).
#[tauri::command]
pub async fn cmd_nexus_has_api_key() -> Result<bool, AppError> {
    crate::blocking(move || {
        credentials::get_credential(NEXUS_SERVICE, NEXUS_USERNAME)
            .map(|opt| opt.is_some())
            .map_err(|e| e.to_string())
    })
    .await
}

/// User profile data returned by the Nexus v1 validate endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NexusUserProfile {
    pub user_id: u64,
    pub name: String,
    pub profile_url: Option<String>,
}

/// Validate the stored API key by making a lightweight API call.
///
/// Uses the v1 endpoint (`/v1/users/validate.json`) because v3 has no
/// equivalent validation route.
///
/// Returns the user profile if the key is valid, `None` if authentication fails.
/// Other errors (network, timeout) propagate as `AppError`.
#[tauri::command]
pub async fn cmd_nexus_validate_api_key(
    state: State<'_, NexusState>,
) -> Result<Option<NexusUserProfile>, AppError> {
    let client = get_client(&state)?;

    let resp = client
        .inner()
        .get("https://api.nexusmods.com/v1/users/validate.json")
        .send()
        .await
        .map_err(|e| -> AppError {
            if e.is_timeout() {
                PlatformError::Timeout.into()
            } else {
                PlatformError::HttpError(format!("GET /v1/users/validate.json: {e}")).into()
            }
        })?;

    match resp.status().as_u16() {
        200 => {
            let json: serde_json::Value = resp.json().await.map_err(|e| -> AppError {
                PlatformError::HttpError(format!("Failed to parse validate response: {e}")).into()
            })?;
            let user_id = json["user_id"].as_u64().unwrap_or(0);
            let name = json["name"].as_str().unwrap_or("User").to_string();
            let profile_url = json["profile_url"].as_str().map(String::from);
            Ok(Some(NexusUserProfile {
                user_id,
                name,
                profile_url,
            }))
        }
        401 | 403 => Ok(None),
        code => {
            let body = resp.text().await.unwrap_or_default();
            let err: AppError = PlatformError::ApiError {
                status: code,
                message: body,
            }
            .into();
            Err(err)
        }
    }
}

// ── Mod Resolution ───────────────────────────────────────────────────

/// Resolve a Nexus mod from a URL, partial path, or bare numeric ID.
#[tauri::command]
pub async fn cmd_nexus_resolve_mod(
    state: State<'_, NexusState>,
    url_or_id: String,
) -> Result<NexusModDetails, AppError> {
    let client = get_client(&state)?;
    super::mods::resolve_mod(&client, &url_or_id)
        .await
        .map_err(AppError::from)
}

// ── File Update Groups ───────────────────────────────────────────────

/// Fetch file-update groups for a mod.
#[tauri::command]
pub async fn cmd_nexus_get_file_groups(
    state: State<'_, NexusState>,
    mod_uuid: String,
) -> Result<Vec<FileUpdateGroup>, AppError> {
    let client = get_client(&state)?;
    super::mod_files::get_file_groups(&client, &mod_uuid)
        .await
        .map_err(AppError::from)
}

/// Fetch file versions for a file-update group.
#[tauri::command]
pub async fn cmd_nexus_get_file_versions(
    state: State<'_, NexusState>,
    group_id: String,
) -> Result<Vec<FileVersion>, AppError> {
    let client = get_client(&state)?;
    super::mod_files::get_file_versions(&client, &group_id)
        .await
        .map_err(AppError::from)
}

/// Fetch ALL files for a mod via the v1 API (no file groups required).
#[tauri::command]
pub async fn cmd_nexus_get_all_mod_files(
    state: State<'_, NexusState>,
    mod_id: u64,
) -> Result<Vec<FileVersion>, AppError> {
    let client = get_client(&state)?;
    super::mod_files::get_all_mod_files(&client, mod_id)
        .await
        .map_err(AppError::from)
}

// ── File Upload ──────────────────────────────────────────────────────

/// Upload a file to Nexus as a new version in a file-update group.
#[tauri::command]
pub async fn cmd_nexus_upload_file(
    app: tauri::AppHandle,
    state: State<'_, NexusState>,
    params: NexusUploadParams,
) -> Result<(), AppError> {
    let client = get_client(&state)?;
    super::upload::upload_file(&client, &params, &app)
        .await
        .map_err(AppError::from)
}

// ── New Mod File Creation (AG7) ──────────────────────────────────────

/// Create a new mod file entry on Nexus from a completed upload.
#[tauri::command]
pub async fn cmd_nexus_create_mod_file(
    state: State<'_, NexusState>,
    mod_uuid: String,
    upload_id: String,
    name: String,
    version: String,
    description: String,
    category: String,
) -> Result<(), AppError> {
    let client = get_client(&state)?;
    super::mod_files::create_mod_file(
        &client,
        &mod_uuid,
        &upload_id,
        &name,
        &version,
        &description,
        &category,
    )
    .await
    .map_err(AppError::from)
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Extract the NexusClient from managed state, returning a user-friendly error
/// if no API key has been configured.
fn get_client(state: &State<'_, NexusState>) -> Result<NexusClient, AppError> {
    // NexusClient is not Clone — we need to recreate from stored key.
    // Instead, we hold the lock only briefly to check presence, then
    // clone is not needed because we use the reference within the lock scope.
    // However, since we need to return an owned type for async use,
    // we'll recreate the client from the keyring credential.
    let guard = state
        .client
        .lock()
        .map_err(|_| AppError::internal("NexusState lock poisoned"))?;

    if guard.is_none() {
        return Err(AppError::invalid_input(
            "No Nexus API key configured. Please add your API key in Settings > Publishing.",
        ));
    }
    drop(guard);

    // Re-read the key from keyring to create a fresh client for this request.
    // This avoids holding the Mutex across an async boundary.
    let key = credentials::get_credential(NEXUS_SERVICE, NEXUS_USERNAME)
        .map_err(AppError::from)?
        .ok_or_else(|| {
            AppError::invalid_input(
                "Nexus API key not found in keyring. Please re-enter your API key.",
            )
        })?;

    NexusClient::new(&key).map_err(AppError::from)
}

/// Try to auto-initialise the Nexus client from a stored keyring credential.
///
/// Called during app setup. Logs warnings on failure but never panics.
pub fn try_auto_init(state: &NexusState) {
    match credentials::get_credential(NEXUS_SERVICE, NEXUS_USERNAME) {
        Ok(Some(key)) => match NexusClient::new(&key) {
            Ok(client) => {
                if let Ok(mut guard) = state.client.lock() {
                    *guard = Some(client);
                    tracing::info!("Nexus client auto-initialised from stored API key");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to create Nexus client from stored key: {e}");
            }
        },
        Ok(None) => {
            tracing::debug!("No stored Nexus API key — client not initialised");
        }
        Err(e) => {
            tracing::warn!("Failed to read Nexus keyring credential: {e}");
        }
    }
}

// ── Integrated Package + Upload (AI6) ────────────────────────────────

/// Parameters for integrated package-and-upload.
#[derive(Debug, Deserialize)]
pub struct NexusPackageUploadParams {
    pub source_dir: String,
    pub mod_uuid: String,
    pub file_group_id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub exclude_patterns: Option<Vec<String>>,
}

/// Package a mod directory into a zip and upload it to Nexus in one step.
#[tauri::command]
pub async fn cmd_nexus_package_and_upload(
    app: tauri::AppHandle,
    state: State<'_, NexusState>,
    params: NexusPackageUploadParams,
) -> Result<(), AppError> {
    let client = get_client(&state)?;

    // Create a temp directory for the zip.
    let temp_dir = std::env::temp_dir().join("cmty-studio-upload");
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| PlatformError::IoError(format!("Failed to create temp dir: {e}")))?;

    let zip_name = format!(
        "{}-{}.zip",
        sanitize_filename(&params.name),
        &params.version
    );
    let zip_path = temp_dir.join(&zip_name);

    let excludes: Vec<&str> = params
        .exclude_patterns
        .as_ref()
        .map(|v| v.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    let _pkg = crate::platform::packaging::create_upload_zip(
        Path::new(&params.source_dir),
        &zip_path,
        &excludes,
    )?;

    let upload_params = NexusUploadParams {
        file_path: zip_path.to_string_lossy().to_string(),
        mod_uuid: params.mod_uuid,
        file_group_id: params.file_group_id,
        name: params.name,
        version: params.version,
        description: params.description,
        category: params.category,
    };

    let result = super::upload::upload_file(&client, &upload_params, &app).await;

    // Clean up the temp zip regardless of outcome.
    let _ = std::fs::remove_file(&zip_path);

    result.map_err(AppError::from)
}

/// Sanitize a string for use as a filename component.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── File Dependencies (AJ2) ─────────────────────────────────────────

/// Fetch mod-level requirements (aggregated from file-level deps via v3 API).
#[tauri::command]
pub async fn cmd_nexus_get_mod_requirements(
    state: State<'_, NexusState>,
    group_ids: Vec<String>,
) -> Result<Vec<NexusDependency>, AppError> {
    let client = get_client(&state)?;
    super::dependencies::get_mod_requirements(&client, &group_ids)
        .await
        .map_err(AppError::from)
}

/// Replace all dependencies for a Nexus mod file.
#[tauri::command]
pub async fn cmd_nexus_set_file_dependencies(
    state: State<'_, NexusState>,
    file_id: String,
    dependency_mod_ids: Vec<String>,
) -> Result<(), AppError> {
    let client = get_client(&state)?;
    super::dependencies::set_dependencies(&client, &file_id, &dependency_mod_ids)
        .await
        .map_err(AppError::from)
}

// ── File Group Rename (AJ8) ──────────────────────────────────────────

/// Rename a file update group on Nexus.
#[tauri::command]
pub async fn cmd_nexus_rename_file_group(
    state: State<'_, NexusState>,
    group_id: String,
    new_name: String,
) -> Result<(), AppError> {
    let client = get_client(&state)?;
    super::mod_files::rename_file_group(&client, &group_id, &new_name)
        .await
        .map_err(AppError::from)
}
