//! Tauri commands for forge operations (GitHub, GitLab, Gitea/Codeberg).

use crate::error::AppError;
use crate::git::forge::{detect_forge, ForgeAdapter};
use crate::git::forge_gitea::GiteaAdapter;
use crate::git::forge_github::GitHubAdapter;
use crate::git::forge_gitlab::GitLabAdapter;
use crate::git::types::*;
use crate::platform::credentials;
use crate::platform::errors::PlatformError;

/// Enum dispatcher — avoids dyn-incompatible async trait methods.
enum Adapter {
    GitHub(GitHubAdapter),
    GitLab(GitLabAdapter),
    Gitea(GiteaAdapter),
}

fn get_adapter(
    forge_type: &ForgeType,
    host: &str,
    api_base: &str,
) -> Result<Adapter, PlatformError> {
    match forge_type {
        ForgeType::GitHub => Ok(Adapter::GitHub(GitHubAdapter::new())),
        ForgeType::GitLab => Ok(Adapter::GitLab(GitLabAdapter::new(host))),
        ForgeType::Gitea => Ok(Adapter::Gitea(GiteaAdapter::new(host, api_base))),
        ForgeType::Unknown => Err(PlatformError::ValidationError(
            "Unknown forge type — forge features are not available for this remote".to_string(),
        )),
    }
}

/// Dispatch macro to avoid repeating match arms for every method call.
macro_rules! dispatch {
    ($adapter:expr, $method:ident ( $($arg:expr),* )) => {
        match $adapter {
            Adapter::GitHub(a) => a.$method($($arg),*).await,
            Adapter::GitLab(a) => a.$method($($arg),*).await,
            Adapter::Gitea(a) => a.$method($($arg),*).await,
        }
    };
}

/// Get the forge token for a host from the platform keyring.
fn get_forge_token(host: &str) -> Result<Option<String>, PlatformError> {
    credentials::get_credential("forge_token", host)
}

/// Store a forge token for a host in the platform keyring.
fn store_forge_token(host: &str, token: &str) -> Result<(), PlatformError> {
    credentials::store_credential("forge_token", host, token)
}

/// Delete the forge token for a host from the platform keyring.
fn delete_forge_token(host: &str) -> Result<(), PlatformError> {
    credentials::delete_credential("forge_token", host)
}

/// Require an authenticated token, returning an error if not found.
fn require_token(host: &str) -> Result<String, PlatformError> {
    match get_forge_token(host)? {
        Some(t) if !t.is_empty() => Ok(t),
        _ => Err(PlatformError::ApiError {
            status: 401,
            message: "Not authenticated — add a Personal Access Token in Settings > Git > Remote Accounts".into(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn cmd_forge_detect(remote_url: String) -> Result<ForgeInfo, AppError> {
    Ok(detect_forge(&remote_url))
}

#[tauri::command]
pub async fn cmd_forge_auth_status(
    host: String,
    forge_type: ForgeType,
    api_base: String,
) -> Result<Option<ForgeUser>, AppError> {
    let token = match get_forge_token(&host)? {
        Some(t) if !t.is_empty() => t,
        _ => return Ok(None),
    };
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    match dispatch!(adapter, validate_token(&token)) {
        Ok(user) => Ok(Some(user)),
        Err(_) => Ok(None), // Token invalid/expired — treat as not connected
    }
}

#[tauri::command]
pub async fn cmd_forge_set_token(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    token: String,
) -> Result<ForgeUser, AppError> {
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    let user = dispatch!(adapter, validate_token(&token))?;
    store_forge_token(&host, &token)?;
    Ok(user)
}

#[tauri::command]
pub fn cmd_forge_clear_token(host: String) -> Result<(), AppError> {
    delete_forge_token(&host)?;
    Ok(())
}

#[tauri::command]
pub async fn cmd_forge_list_repos(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    page: u32,
) -> Result<Vec<ForgeRepo>, AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(adapter, list_repos(&token, page))?)
}

#[tauri::command]
pub async fn cmd_forge_create_repo(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    name: String,
    description: String,
    private: bool,
) -> Result<ForgeRepo, AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(
        adapter,
        create_repo(&token, &name, &description, private)
    )?)
}

#[tauri::command]
pub async fn cmd_forge_list_prs(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    owner: String,
    repo: String,
    state: String,
) -> Result<Vec<ForgePR>, AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(adapter, list_prs(&token, &owner, &repo, &state))?)
}

#[tauri::command]
pub async fn cmd_forge_create_pr(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    owner: String,
    repo: String,
    params: CreatePrParams,
) -> Result<ForgePR, AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(
        adapter,
        create_pr(&token, &owner, &repo, &params)
    )?)
}

#[tauri::command]
pub async fn cmd_forge_list_issues(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    owner: String,
    repo: String,
    state: String,
) -> Result<Vec<ForgeIssue>, AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(
        adapter,
        list_issues(&token, &owner, &repo, &state)
    )?)
}

#[tauri::command]
pub async fn cmd_forge_create_issue(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    owner: String,
    repo: String,
    title: String,
    body: String,
) -> Result<ForgeIssue, AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(
        adapter,
        create_issue(&token, &owner, &repo, &title, &body)
    )?)
}

#[tauri::command]
pub async fn cmd_forge_get_issue(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    owner: String,
    repo: String,
    number: u32,
) -> Result<ForgeIssueDetail, AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(
        adapter,
        get_issue(&token, &owner, &repo, number)
    )?)
}

#[tauri::command]
pub async fn cmd_forge_assign_issue(
    host: String,
    forge_type: ForgeType,
    api_base: String,
    owner: String,
    repo: String,
    number: u32,
    assignee: String,
) -> Result<(), AppError> {
    let token = require_token(&host)?;
    let adapter = get_adapter(&forge_type, &host, &api_base)?;
    Ok(dispatch!(
        adapter,
        assign_issue(&token, &owner, &repo, number, &assignee)
    )?)
}
