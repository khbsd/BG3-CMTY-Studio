//! HTTP fetch commands for retrieving remote resources (e.g., JSON schemas).

use crate::error::AppError;

/// Allowed URL prefixes for remote fetching (security: prevent arbitrary requests).
const ALLOWED_PREFIXES: &[&str] = &["https://raw.githubusercontent.com/", "https://github.com/"];

/// Maximum response size in bytes (1 MB).
const MAX_RESPONSE_SIZE: usize = 1_048_576;

/// Fetch a remote text resource (e.g., a JSON schema) from an allowed URL.
///
/// Returns the response body as a string. Only HTTPS URLs matching
/// `ALLOWED_PREFIXES` are permitted to prevent SSRF.
#[tauri::command]
pub async fn cmd_fetch_remote_schema(url: String) -> Result<String, AppError> {
    // Validate the URL is in the allowlist
    if !ALLOWED_PREFIXES
        .iter()
        .any(|prefix| url.starts_with(prefix))
    {
        return Err(AppError::internal(format!("URL not in allowlist: {url}")));
    }

    let response = reqwest::get(&url)
        .await
        .map_err(|e| AppError::internal(format!("HTTP request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(AppError::internal(format!(
            "HTTP {} for {url}",
            response.status()
        )));
    }

    let content_length = response.content_length().unwrap_or(0) as usize;
    if content_length > MAX_RESPONSE_SIZE {
        return Err(AppError::internal(format!(
            "Response too large ({content_length} bytes, max {MAX_RESPONSE_SIZE})"
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| AppError::internal(format!("Failed to read response body: {e}")))?;

    if bytes.len() > MAX_RESPONSE_SIZE {
        return Err(AppError::internal(format!(
            "Response too large ({} bytes, max {MAX_RESPONSE_SIZE})",
            bytes.len()
        )));
    }

    String::from_utf8(bytes.to_vec())
        .map_err(|e| AppError::internal(format!("Response is not valid UTF-8: {e}")))
}
