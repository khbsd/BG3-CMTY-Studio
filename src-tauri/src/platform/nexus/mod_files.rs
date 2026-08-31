//! Nexus Mods file-update-group queries and mod file creation.

use serde::Serialize;

use crate::platform::errors::PlatformError;

use super::client::NexusClient;

/// Normalize category strings to Title Case for display.
/// Handles v3 lowercase ("main"), v1 uppercase ("ARCHIVED"), and mixed case.
fn normalize_category(cat: &str) -> String {
    match cat.to_ascii_lowercase().as_str() {
        "main" => "Main".into(),
        "update" => "Update".into(),
        "optional" => "Optional".into(),
        "old_version" | "old" => "Old".into(),
        "miscellaneous" | "misc" => "Miscellaneous".into(),
        "archived" => "Archived".into(),
        "removed" | "deleted" => "Removed".into(),
        _ => {
            // Fallback: capitalize first letter, lowercase the rest
            let lower = cat.to_ascii_lowercase();
            let mut c = lower.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        }
    }
}

/// Format an ISO datetime string (e.g. "2024-01-30T15:22:43.000Z") to "YYYY-MM-DD".
fn format_datetime(s: &str) -> String {
    // Try to extract the date portion from an ISO datetime
    if s.len() >= 10 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(7) == Some(&b'-') {
        s[..10].to_string()
    } else {
        s.to_string()
    }
}

/// A file-update group within a Nexus mod.
#[derive(Debug, Clone, Serialize)]
pub struct FileUpdateGroup {
    pub id: String,
    pub name: String,
    pub version_count: u32,
    pub last_upload: Option<String>,
    /// Name of the latest published file in this group (from `latest_published_file_version`).
    pub latest_file_name: Option<String>,
}

/// Fetch file-update groups for a mod (by UUID).
///
/// `GET /mods/{uuid}/file-update-groups`
pub async fn get_file_groups(
    client: &NexusClient,
    mod_uuid: &str,
) -> Result<Vec<FileUpdateGroup>, PlatformError> {
    let path = format!("/mods/{mod_uuid}/file-update-groups");
    let resp = client.get(&path).await?;

    let json: serde_json::Value = resp.json().await.map_err(|e| {
        PlatformError::HttpError(format!("Failed to parse file-groups response: {e}"))
    })?;

    // The API may return `{ "data": { "groups": [...] } }`, `{ "file_update_groups": [...] }`,
    // or a bare array.
    let items = if let Some(arr) = json["data"]["groups"].as_array() {
        arr.clone()
    } else if let Some(arr) = json.as_array() {
        arr.clone()
    } else if let Some(arr) = json.get("file_update_groups").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = json.get("groups").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = json["data"].as_array() {
        arr.clone()
    } else {
        return Err(PlatformError::ApiError {
            status: 200,
            message: "Unexpected file-groups response format".into(),
        });
    };

    let groups = items
        .iter()
        .filter_map(|item| {
            let id = item["id"]
                .as_str()
                .map(String::from)
                .or_else(|| item["id"].as_u64().map(|n| n.to_string()))?;
            let name = item["name"].as_str().unwrap_or("Unnamed").to_string();
            let version_count = item["version_count"]
                .as_u64()
                .or_else(|| item["versions_count"].as_u64())
                .unwrap_or(0) as u32;
            let last_upload = item["last_upload"]
                .as_str()
                .or_else(|| item["updated_at"].as_str())
                .map(String::from);
            let latest_file_name = item["latest_published_file_version"]["file"]["name"]
                .as_str()
                .map(String::from);
            Some(FileUpdateGroup {
                id,
                name,
                version_count,
                last_upload,
                latest_file_name,
            })
        })
        .collect();

    Ok(groups)
}

/// Create a new mod file entry on Nexus.
///
/// `POST /mod-files` with the given metadata.
pub async fn create_mod_file(
    client: &NexusClient,
    mod_uuid: &str,
    upload_id: &str,
    name: &str,
    version: &str,
    description: &str,
    category: &str,
) -> Result<(), PlatformError> {
    let body = serde_json::json!({
        "mod_id": mod_uuid,
        "upload_id": upload_id,
        "name": name,
        "version": version,
        "description": description,
        "category": category,
    });

    client.post_json("/mod-files", &body).await?;
    Ok(())
}

/// A file version within a file-update group.
#[derive(Debug, Clone, Serialize)]
pub struct FileVersion {
    pub id: String,
    pub name: String,
    pub version: String,
    pub category: Option<String>,
    pub description: Option<String>,
    pub changelog_html: Option<String>,
    pub created_at: Option<String>,
    pub size_kb: Option<u64>,
}

/// Fetch file versions for a file-update group.
///
/// `GET /file-update-groups/{group_id}/versions`
pub async fn get_file_versions(
    client: &NexusClient,
    group_id: &str,
) -> Result<Vec<FileVersion>, PlatformError> {
    let path = format!("/file-update-groups/{group_id}/versions");
    let resp = client.get(&path).await?;

    let json: serde_json::Value = resp.json().await.map_err(|e| {
        PlatformError::HttpError(format!("Failed to parse file-versions response: {e}"))
    })?;

    // The API may return `{ "data": { "versions": [...] } }`, `{ "file_versions": [...] }`,
    // `{ "versions": [...] }`, or a bare array.
    let items = if let Some(arr) = json["data"]["versions"].as_array() {
        arr.clone()
    } else if let Some(arr) = json.as_array() {
        arr.clone()
    } else if let Some(arr) = json.get("file_versions").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = json.get("versions").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = json["data"].as_array() {
        arr.clone()
    } else {
        return Err(PlatformError::ApiError {
            status: 200,
            message: "Unexpected file-versions response format".into(),
        });
    };

    let versions = items
        .iter()
        .filter_map(|item| {
            // v3: version items nest file data under `file`; v1 items are flat.
            let file_node = if item.get("file").is_some() {
                &item["file"]
            } else {
                item
            };
            let id = file_node["id"]
                .as_str()
                .map(String::from)
                .or_else(|| file_node["id"].as_u64().map(|n| n.to_string()))?;
            let name = file_node["name"].as_str().unwrap_or("Unnamed").to_string();
            let version = file_node["version"].as_str().unwrap_or("").to_string();
            let category = file_node["category"].as_str().map(normalize_category);
            let description = file_node["description"]
                .as_str()
                .or_else(|| item["description"].as_str())
                .map(String::from);
            let changelog_html = file_node["changelog_html"]
                .as_str()
                .or_else(|| item["changelog_html"].as_str())
                .or_else(|| item["changelog"].as_str())
                .map(String::from);
            let created_at = file_node["uploaded_at"]
                .as_str()
                .or_else(|| item["created_at"].as_str())
                .or_else(|| item["uploaded_at"].as_str())
                .map(format_datetime);
            let size_kb = file_node["size_in_bytes"]
                .as_u64()
                .map(|b| b / 1024)
                .or_else(|| item["size_in_bytes"].as_u64().map(|b| b / 1024))
                .or_else(|| item["size_kb"].as_u64());
            Some(FileVersion {
                id,
                name,
                version,
                category,
                description,
                changelog_html,
                created_at,
                size_kb,
            })
        })
        .collect();

    Ok(versions)
}

/// Rename a file update group.
///
/// `PUT /mod-file-update-groups/{group_id}`
pub async fn rename_file_group(
    client: &NexusClient,
    group_id: &str,
    new_name: &str,
) -> Result<(), PlatformError> {
    let path = format!("/mod-file-update-groups/{group_id}");
    let body = serde_json::json!({ "name": new_name });
    client.put_json(&path, &body).await?;
    Ok(())
}

/// Fetch ALL files for a mod via the v1 API (no file groups required).
///
/// `GET /v1/games/baldursgate3/mods/{mod_id}/files.json`
pub async fn get_all_mod_files(
    client: &NexusClient,
    mod_id: u64,
) -> Result<Vec<FileVersion>, PlatformError> {
    let url = format!("https://api.nexusmods.com/v1/games/baldursgate3/mods/{mod_id}/files.json");
    let resp = client
        .inner()
        .get(&url)
        .send()
        .await
        .map_err(|e| PlatformError::HttpError(format!("GET v1/files: {e}")))?;

    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(PlatformError::ApiError {
            status,
            message: body,
        });
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| PlatformError::HttpError(format!("Failed to parse v1 files response: {e}")))?;

    let items = json
        .get("files")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let versions = items
        .iter()
        .filter_map(|item| {
            let id = item["file_id"]
                .as_u64()
                .or_else(|| item["id"].as_u64())
                .map(|n| n.to_string())
                .or_else(|| item["file_id"].as_str().map(String::from))
                .or_else(|| item["id"].as_str().map(String::from))?;
            let name = item["file_name"]
                .as_str()
                .or_else(|| item["name"].as_str())
                .unwrap_or("Unnamed")
                .to_string();
            let version = item["version"].as_str().unwrap_or("").to_string();
            let category = match item["category_id"].as_u64() {
                Some(1) => Some("Main".into()),
                Some(2) => Some("Update".into()),
                Some(3) => Some("Optional".into()),
                Some(4) => Some("Old".into()),
                Some(5) => Some("Miscellaneous".into()),
                Some(6) => Some("Archived".into()),
                _ => item["category_name"].as_str().map(normalize_category),
            };
            let description = item["description"].as_str().map(String::from);
            let changelog_html = item["changelog_html"]
                .as_str()
                .or_else(|| item["changelog"].as_str())
                .map(String::from);
            let created_at = item["uploaded_timestamp"]
                .as_u64()
                .map(|ts| {
                    chrono::DateTime::from_timestamp(ts as i64, 0)
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                        .unwrap_or_default()
                })
                .or_else(|| item["uploaded_time"].as_str().map(String::from));
            let size_kb = item["size_in_bytes"]
                .as_u64()
                .map(|b| b / 1024)
                .or_else(|| item["size_kb"].as_u64())
                .or_else(|| item["size"].as_u64());
            Some(FileVersion {
                id,
                name,
                version,
                category,
                description,
                changelog_html,
                created_at,
                size_kb,
            })
        })
        .collect();

    Ok(versions)
}
