//! GitHub implementation of the ForgeAdapter trait.

use reqwest::Client;
use serde::Deserialize;

use super::forge::ForgeAdapter;
use super::types::{
    CreatePrParams, ForgeIssue, ForgeIssueDetail, ForgePR, ForgeRepo, ForgeType, ForgeUser,
};
use crate::platform::errors::PlatformError;
use crate::platform::http::build_client;
use crate::platform::rate_limiter::TokenBucket;

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

pub struct GitHubAdapter {
    client: Client,
    api_base: String,
    rate_limiter: TokenBucket,
}

impl GitHubAdapter {
    pub fn new() -> Self {
        Self::with_api_base("https://api.github.com")
    }

    /// Create a `GitHubAdapter` with a custom API base URL (for testing or GitHub Enterprise).
    pub fn with_api_base(api_base: &str) -> Self {
        let client = build_client("CMTY-Studio", 30).expect("failed to build HTTP client");

        Self {
            client,
            api_base: api_base.to_string(),
            rate_limiter: crate::git::rate_limits::github_rate_limiter(),
        }
    }
}

impl Default for GitHubAdapter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ForgeAdapter impl
// ---------------------------------------------------------------------------

impl ForgeAdapter for GitHubAdapter {
    fn forge_type(&self) -> ForgeType {
        ForgeType::GitHub
    }

    fn api_base(&self) -> &str {
        &self.api_base
    }

    fn token_creation_url(&self) -> String {
        "https://github.com/settings/tokens/new?scopes=repo".to_string()
    }

    fn pr_label(&self) -> &str {
        "Pull Request"
    }

    async fn validate_token(&self, token: &str) -> Result<ForgeUser, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/user", self.api_base);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let user: GhUser = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;

        Ok(ForgeUser {
            login: user.login,
            name: user.name,
            avatar_url: user.avatar_url.unwrap_or_default(),
            forge_id: "github:github.com".to_string(),
        })
    }

    async fn list_repos(&self, token: &str, page: u32) -> Result<Vec<ForgeRepo>, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/user/repos?page={}&per_page=30&sort=updated",
            self.api_base, page
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let repos: Vec<GhRepo> = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;

        Ok(repos.into_iter().map(|r| r.into()).collect())
    }

    async fn create_repo(
        &self,
        token: &str,
        name: &str,
        description: &str,
        private: bool,
    ) -> Result<ForgeRepo, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/user/repos", self.api_base);
        let body = serde_json::json!({
            "name": name,
            "description": description,
            "private": private,
        });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let repo: GhRepo = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;
        Ok(repo.into())
    }

    async fn list_prs(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<ForgePR>, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/repos/{}/{}/pulls?state={}&per_page=30",
            self.api_base, owner, repo, state
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let prs: Vec<GhPR> = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;

        Ok(prs.into_iter().map(|p| p.into()).collect())
    }

    async fn create_pr(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        params: &CreatePrParams,
    ) -> Result<ForgePR, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/repos/{}/{}/pulls", self.api_base, owner, repo);
        let payload = serde_json::json!({
            "title": params.title,
            "body": params.body,
            "head": params.head,
            "base": params.base,
        });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let pr: GhPR = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;
        Ok(pr.into())
    }

    async fn list_issues(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<ForgeIssue>, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/repos/{}/{}/issues?state={}&per_page=30",
            self.api_base, owner, repo, state
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let issues: Vec<GhIssue> = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;

        // GitHub's issues endpoint also returns PRs — filter them out
        Ok(issues
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .map(|i| i.into())
            .collect())
    }

    async fn create_issue(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<ForgeIssue, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/repos/{}/{}/issues", self.api_base, owner, repo);
        let payload = serde_json::json!({
            "title": title,
            "body": body,
        });

        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let issue: GhIssue = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;
        Ok(issue.into())
    }

    async fn get_issue(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Result<ForgeIssueDetail, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/repos/{}/{}/issues/{}",
            self.api_base, owner, repo, number
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }

        let detail: GhIssueDetail = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Parse error: {e}")))?;
        Ok(detail.into())
    }

    async fn assign_issue(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        number: u32,
        assignee: &str,
    ) -> Result<(), PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/repos/{}/{}/issues/{}/assignees",
            self.api_base, owner, repo, number
        );
        let payload = serde_json::json!({ "assignees": [assignee] });
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !resp.status().is_success() {
            return Err(map_error_response(resp).await);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Error handling
// ---------------------------------------------------------------------------

async fn map_error_response(resp: reqwest::Response) -> PlatformError {
    let status = resp.status().as_u16();

    // Check for rate limiting before consuming the body
    if status == 429 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .and_then(TokenBucket::parse_retry_after)
            .unwrap_or(60);
        return PlatformError::RateLimited {
            retry_after_secs: retry_after,
        };
    }

    let gh_err: Option<GhError> = resp.json().await.ok();
    let message = gh_err
        .as_ref()
        .map(|e| e.message.as_str())
        .unwrap_or("unknown error");

    match status {
        401 => PlatformError::ApiError {
            status: 401,
            message: "Authentication failed — verify your token has the correct scopes".into(),
        },
        403 => {
            if message.to_lowercase().contains("rate limit") {
                PlatformError::RateLimited {
                    retry_after_secs: 60,
                }
            } else {
                PlatformError::ApiError {
                    status: 403,
                    message: "Access denied — check token permissions".into(),
                }
            }
        }
        404 => PlatformError::ApiError {
            status: 404,
            message: "Not found".into(),
        },
        422 => PlatformError::ApiError {
            status: 422,
            message: format!("Validation error: {message}"),
        },
        _ => PlatformError::ApiError {
            status,
            message: format!("GitHub API error ({status}): {message}"),
        },
    }
}

// ---------------------------------------------------------------------------
// GitHub API response structs (private)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhError {
    message: String,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GhRepo {
    full_name: String,
    html_url: String,
    clone_url: String,
    private: bool,
    description: Option<String>,
    default_branch: String,
}

impl From<GhRepo> for ForgeRepo {
    fn from(r: GhRepo) -> Self {
        Self {
            full_name: r.full_name,
            html_url: r.html_url,
            clone_url: r.clone_url,
            private: r.private,
            description: r.description,
            default_branch: r.default_branch,
        }
    }
}

#[derive(Deserialize)]
struct GhPR {
    number: u32,
    title: String,
    state: String,
    user: GhMinimalUser,
    created_at: String,
    html_url: String,
    head: GhBranchRef,
    base: GhBranchRef,
}

impl From<GhPR> for ForgePR {
    fn from(p: GhPR) -> Self {
        Self {
            number: p.number,
            title: p.title,
            state: p.state,
            author: p.user.login,
            created_at: p.created_at,
            html_url: p.html_url,
            head_ref: p.head.ref_name,
            base_ref: p.base.ref_name,
            mergeable: None, // Not available in GitHub list endpoint
        }
    }
}

#[derive(Deserialize)]
struct GhBranchRef {
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Deserialize)]
struct GhMinimalUser {
    login: String,
}

#[derive(Deserialize)]
struct GhIssue {
    number: u32,
    title: String,
    state: String,
    user: GhMinimalUser,
    created_at: String,
    html_url: String,
    labels: Vec<GhLabel>,
    pull_request: Option<serde_json::Value>,
    assignee: Option<GhMinimalUser>,
}

impl From<GhIssue> for ForgeIssue {
    fn from(i: GhIssue) -> Self {
        Self {
            number: i.number,
            title: i.title,
            state: i.state,
            author: i.user.login,
            created_at: i.created_at,
            html_url: i.html_url,
            labels: i.labels.into_iter().map(|l| l.name).collect(),
            assignee: i.assignee.map(|a| a.login),
        }
    }
}

#[derive(Deserialize)]
struct GhIssueDetail {
    number: u32,
    title: String,
    state: String,
    user: GhMinimalUser,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: String,
    body: Option<String>,
    labels: Vec<GhLabel>,
    assignees: Vec<GhMinimalUser>,
    milestone: Option<GhMilestone>,
}

#[derive(Deserialize)]
struct GhMilestone {
    title: String,
}

impl From<GhIssueDetail> for ForgeIssueDetail {
    fn from(i: GhIssueDetail) -> Self {
        Self {
            number: i.number,
            title: i.title,
            state: i.state,
            author: i.user.login,
            created_at: i.created_at,
            updated_at: i.updated_at,
            html_url: i.html_url,
            body: i.body.unwrap_or_default(),
            labels: i.labels.into_iter().map(|l| l.name).collect(),
            assignees: i.assignees.into_iter().map(|a| a.login).collect(),
            milestone: i.milestone.map(|m| m.title),
            closed_at: i.closed_at,
        }
    }
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}
