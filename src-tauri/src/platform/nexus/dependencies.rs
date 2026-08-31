//! Nexus Mods mod-level dependency/requirement management.
//!
//! NOTE: The v3 file-level dependency endpoints currently return empty data
//! for mod-level requirements. This module is retained for future use when
//! the Nexus v3 API supports mod-level requirements or when an alternative
//! approach (e.g. scraping v1 mod info) is implemented.
//! The dependencies drawer is hidden in the UI until this is resolved.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::platform::errors::PlatformError;

use super::client::NexusClient;

/// A dependency entry surfaced to the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NexusDependency {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub mod_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// Categories that should be skipped when scanning for dependencies.
const INACTIVE_CATEGORIES: &[&str] = &["old_version", "archived", "removed"];

/// Extract a JSON value as a string, whether it is a JSON string or number.
fn id_to_string(val: &serde_json::Value) -> Option<String> {
    val.as_str()
        .map(String::from)
        .or_else(|| val.as_u64().map(|n| n.to_string()))
}

/// Parse the materialized-dependencies response into `Vec<NexusDependency>`.
fn parse_materialized(json: &serde_json::Value) -> Vec<NexusDependency> {
    let items = json["dependencies"].as_array().cloned().unwrap_or_default();

    let mut deps = Vec::new();
    for dep in &items {
        let dep_id = dep["id"].as_str().unwrap_or_default().to_string();
        let Some(first_group) = dep["candidate_groups"].as_array().and_then(|a| a.first()) else {
            continue;
        };
        let mod_info = &first_group["mod"];
        let mod_id = mod_info["game_scoped_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if mod_id.is_empty() {
            continue;
        }
        let name = mod_info["name"].as_str().unwrap_or("Unknown").to_string();
        let version = first_group["candidate_versions"]
            .as_array()
            .and_then(|v| v.last())
            .and_then(|v| v["file"]["version"].as_str())
            .unwrap_or("")
            .to_string();
        deps.push(NexusDependency {
            id: dep_id,
            mod_id,
            name,
            version,
        });
    }
    deps
}

/// Fetch mod-level requirements given pre-resolved file-update-group IDs.
///
/// For each group, fetches versions to find file UUIDs, then queries
/// the materialized dependencies endpoint for each active file.
pub async fn get_mod_requirements(
    client: &NexusClient,
    group_ids: &[String],
) -> Result<Vec<NexusDependency>, PlatformError> {
    if group_ids.is_empty() {
        return Ok(vec![]);
    }

    let mut all_deps = Vec::new();
    let mut seen_mod_ids = HashSet::new();

    for gid in group_ids {
        let versions_path = format!("/file-update-groups/{gid}/versions");
        let versions_resp = match client.get(&versions_path).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let vers_json: serde_json::Value = match versions_resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let versions = vers_json["data"]["versions"]
            .as_array()
            .or_else(|| vers_json["file_versions"].as_array())
            .or_else(|| vers_json["versions"].as_array())
            .or_else(|| vers_json["data"].as_array())
            .or_else(|| vers_json.as_array())
            .cloned()
            .unwrap_or_default();

        // Find the newest active file in this group
        let newest_active = versions.iter().rev().find(|ver| {
            let file_node = if ver.get("file").is_some() {
                &ver["file"]
            } else {
                ver
            };
            let cat = file_node["category"].as_str().unwrap_or("");
            !INACTIVE_CATEGORIES.contains(&cat)
        });

        let Some(ver) = newest_active else { continue };

        let file_node = if ver.get("file").is_some() {
            &ver["file"]
        } else {
            ver
        };
        let Some(file_uuid) = file_node["id"]
            .as_str()
            .map(String::from)
            .or_else(|| id_to_string(&ver["id"]))
        else {
            continue;
        };

        let deps_path = format!("/mod-files/{file_uuid}/dependencies/materialized");
        let deps_resp = match client.get(&deps_path).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let deps_json: serde_json::Value = match deps_resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        for dep in parse_materialized(&deps_json) {
            if seen_mod_ids.insert(dep.mod_id.clone()) {
                all_deps.push(dep);
            }
        }
    }

    Ok(all_deps)
}

/// Replace all dependency ranges for a mod file.
///
/// `PUT /mod-files/{file_id}/dependencies/ranges`
///
/// `file_id` should be a v3 mod-file UUID.
pub async fn set_dependencies(
    client: &NexusClient,
    file_id: &str,
    dependency_mod_ids: &[String],
) -> Result<(), PlatformError> {
    let path = format!("/mod-files/{file_id}/dependencies/ranges");
    let body = serde_json::json!({
        "dependency_definitions": dependency_mod_ids.iter().map(|_| {
            serde_json::json!({ "ranges": [] })
        }).collect::<Vec<_>>(),
    });
    client.put_json(&path, &body).await?;
    Ok(())
}
