//! Nexus Mods API client.

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;

use crate::platform::errors::PlatformError;

/// Base URL for the Nexus Mods v3 API.
const NEXUS_API_BASE: &str = "https://api.nexusmods.com/v3";

/// User-Agent sent with every Nexus API request.
/// Uses a static base since reqwest::ClientBuilder::user_agent takes
/// an impl TryInto<HeaderValue> and concat!() isn't available with env!().
const USER_AGENT: &str = "BG3-CMTY-Studio";

/// Application-Name header required by Nexus Mods API acceptable use policy.
/// Sourced from `src/lib/version.ts` APP_NAME at compile time via build.rs.
const APPLICATION_NAME: &str = env!("FRONTEND_APP_NAME");

/// Application-Version header required by Nexus Mods API acceptable use policy.
/// Sourced from `src/lib/version.ts` APP_VERSION at compile time via build.rs.
const APPLICATION_VERSION: &str = env!("FRONTEND_APP_VERSION");

/// Default request timeout in seconds.
const TIMEOUT_SECS: u64 = 120;

/// HTTP client pre-configured for the Nexus Mods API.
pub struct NexusClient {
    client: Client,
    base_url: String,
}

impl NexusClient {
    /// Create a new `NexusClient` with the given API key.
    ///
    /// The key is attached as a default `apikey` header (marked sensitive)
    /// on every outgoing request.
    pub fn new(api_key: &str) -> Result<Self, PlatformError> {
        let mut api_header = HeaderValue::from_str(api_key)
            .map_err(|e| PlatformError::ValidationError(format!("Invalid API key format: {e}")))?;
        api_header.set_sensitive(true);

        let mut headers = HeaderMap::new();
        headers.insert("apikey", api_header);
        headers.insert(
            "Application-Name",
            HeaderValue::from_static(APPLICATION_NAME),
        );
        headers.insert(
            "Application-Version",
            HeaderValue::from_str(APPLICATION_VERSION)
                .unwrap_or_else(|_| HeaderValue::from_static("0.0.0")),
        );

        let user_agent = format!("{USER_AGENT}/{APPLICATION_VERSION}");

        let client = Client::builder()
            .user_agent(user_agent)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .default_headers(headers)
            .build()
            .map_err(|e| PlatformError::HttpError(format!("Failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url: NEXUS_API_BASE.to_string(),
        })
    }

    /// Return a reference to the inner `reqwest::Client`.
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Build a full URL for the given API path (must start with `/`).
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Make a GET request to the given API path and return the response.
    pub async fn get(&self, path: &str) -> Result<reqwest::Response, PlatformError> {
        let url = self.url(path);
        let resp = self.client.get(&url).send().await.map_err(|e| {
            if e.is_timeout() {
                PlatformError::Timeout
            } else {
                PlatformError::HttpError(format!("GET {url}: {e}"))
            }
        })?;
        check_response(resp).await
    }

    /// Make a POST request with a JSON body and return the response.
    pub async fn post_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, PlatformError> {
        let url = self.url(path);
        let resp = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(format!("POST {url}: {e}"))
                }
            })?;
        check_response(resp).await
    }

    /// Make a PUT request with a JSON body and return the response.
    pub async fn put_json(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, PlatformError> {
        let url = self.url(path);
        let resp = self.client.put(&url).json(body).send().await.map_err(|e| {
            if e.is_timeout() {
                PlatformError::Timeout
            } else {
                PlatformError::HttpError(format!("PUT {url}: {e}"))
            }
        })?;
        check_response(resp).await
    }
}

/// Check HTTP response status and convert errors.
async fn check_response(resp: reqwest::Response) -> Result<reqwest::Response, PlatformError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    // Check for rate limiting
    if status.as_u16() == 429 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::platform::rate_limiter::TokenBucket::parse_retry_after)
            .unwrap_or(60);
        return Err(PlatformError::RateLimited {
            retry_after_secs: retry_after,
        });
    }

    let status_code = status.as_u16();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|_| "(failed to read body)".into());
    Err(PlatformError::ApiError {
        status: status_code,
        message: body,
    })
}
