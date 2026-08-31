//! Gitea / Forgejo / Codeberg forge adapter.
//!
//! Key difference from GitHub/GitLab: auth header is `Authorization: token <PAT>`
//! (NOT `Bearer`). Pagination uses `limit` (not `per_page`).

use super::forge::ForgeAdapter;
use super::types::{
    CreatePrParams, ForgeIssue, ForgeIssueDetail, ForgePR, ForgeRepo, ForgeType, ForgeUser,
};
use crate::platform::errors::PlatformError;
use crate::platform::http::build_client;
use crate::platform::rate_limiter::TokenBucket;
use reqwest::Client;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Gitea response deserialization structs (private)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GtUser {
    login: String,
    full_name: Option<String>,
    avatar_url: String,
}

#[derive(Deserialize)]
struct GtRepo {
    full_name: String,
    html_url: String,
    clone_url: String,
    private: bool,
    description: Option<String>,
    default_branch: String,
}

#[derive(Deserialize)]
struct GtPR {
    number: u32,
    title: String,
    state: String,
    user: GtPRUser,
    created_at: String,
    html_url: String,
    head: GtBranchRef,
    base: GtBranchRef,
    mergeable: Option<bool>,
}

#[derive(Deserialize)]
struct GtPRUser {
    login: String,
}

#[derive(Deserialize)]
struct GtBranchRef {
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Deserialize)]
struct GtIssue {
    number: u32,
    title: String,
    state: String,
    user: GtIssueUser,
    created_at: String,
    html_url: String,
    labels: Option<Vec<GtLabel>>,
    assignee: Option<GtIssueUser>,
}

#[derive(Deserialize)]
struct GtIssueDetail {
    number: u32,
    title: String,
    state: String,
    user: GtIssueUser,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    html_url: String,
    body: Option<String>,
    labels: Option<Vec<GtLabel>>,
    assignees: Option<Vec<GtIssueUser>>,
    milestone: Option<GtMilestone>,
}

#[derive(Deserialize)]
struct GtMilestone {
    title: String,
}

#[derive(Deserialize)]
struct GtIssueUser {
    login: String,
}

#[derive(Deserialize)]
struct GtLabel {
    name: String,
}

#[derive(Deserialize)]
struct GtError {
    message: Option<String>,
}

// ---------------------------------------------------------------------------
// GiteaAdapter
// ---------------------------------------------------------------------------

pub struct GiteaAdapter {
    client: Client,
    api_base: String,
    host: String,
    rate_limiter: TokenBucket,
}

impl GiteaAdapter {
    pub fn new(host: &str, api_base: &str) -> Self {
        let client = build_client("CMTY-Studio", 30).expect("failed to build HTTP client");

        Self {
            client,
            api_base: api_base.to_string(),
            host: host.to_string(),
            rate_limiter: crate::git::rate_limits::gitea_rate_limiter(),
        }
    }
}

/// Gitea auth header: `token <PAT>` (NOT `Bearer`).
fn auth_header(token: &str) -> String {
    format!("token {token}")
}

/// Extract a structured error from a Gitea API response.
async fn extract_error(resp: reqwest::Response) -> PlatformError {
    let status = resp.status();
    let code = status.as_u16();

    if code == 429 {
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

    // Try to parse Gitea's JSON error body
    let message = resp
        .json::<GtError>()
        .await
        .ok()
        .and_then(|e| e.message)
        .unwrap_or_default();

    match code {
        401 => PlatformError::ApiError {
            status: 401,
            message: "Authentication failed — verify your token has the correct scopes".into(),
        },
        403 => PlatformError::ApiError {
            status: 403,
            message: "Access denied — check token permissions".into(),
        },
        404 => PlatformError::ApiError {
            status: 404,
            message: "Not found".into(),
        },
        422 => {
            if message.is_empty() {
                PlatformError::ApiError {
                    status: 422,
                    message: "Validation error (422)".into(),
                }
            } else {
                PlatformError::ApiError {
                    status: 422,
                    message: format!("Validation error: {message}"),
                }
            }
        }
        _ => {
            if message.is_empty() {
                PlatformError::ApiError {
                    status: code,
                    message: format!("Gitea API error ({code})"),
                }
            } else {
                PlatformError::ApiError {
                    status: code,
                    message: format!("Gitea API error ({code}): {message}"),
                }
            }
        }
    }
}

impl ForgeAdapter for GiteaAdapter {
    fn forge_type(&self) -> ForgeType {
        ForgeType::Gitea
    }

    fn api_base(&self) -> &str {
        &self.api_base
    }

    fn token_creation_url(&self) -> String {
        format!("https://{}/user/settings/applications", self.host)
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
            .header("Authorization", auth_header(token))
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
            return Err(extract_error(resp).await);
        }

        let user: GtUser = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse user response: {e}")))?;

        let name = user
            .full_name
            .filter(|n| !n.is_empty())
            .or_else(|| Some(user.login.clone()));

        Ok(ForgeUser {
            login: user.login,
            name,
            avatar_url: user.avatar_url,
            forge_id: format!("gitea:{}", self.host),
        })
    }

    async fn list_repos(&self, token: &str, page: u32) -> Result<Vec<ForgeRepo>, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/user/repos?page={}&limit=30&sort=updated",
            self.api_base, page
        );
        let resp = self
            .client
            .get(&url)
            .header("Authorization", auth_header(token))
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
            return Err(extract_error(resp).await);
        }

        let repos: Vec<GtRepo> = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse repos response: {e}"))
        })?;

        Ok(repos
            .into_iter()
            .map(|r| ForgeRepo {
                full_name: r.full_name,
                html_url: r.html_url,
                clone_url: r.clone_url,
                private: r.private,
                description: r.description,
                default_branch: r.default_branch,
            })
            .collect())
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
            .header("Authorization", auth_header(token))
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
            return Err(extract_error(resp).await);
        }

        let repo: GtRepo = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse repo response: {e}")))?;

        Ok(ForgeRepo {
            full_name: repo.full_name,
            html_url: repo.html_url,
            clone_url: repo.clone_url,
            private: repo.private,
            description: repo.description,
            default_branch: repo.default_branch,
        })
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
            "{}/repos/{}/{}/pulls?state={}&limit=30",
            self.api_base, owner, repo, state
        );
        let resp = self
            .client
            .get(&url)
            .header("Authorization", auth_header(token))
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
            return Err(extract_error(resp).await);
        }

        let prs: Vec<GtPR> = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse PRs response: {e}")))?;

        Ok(prs
            .into_iter()
            .map(|pr| ForgePR {
                number: pr.number,
                title: pr.title,
                state: pr.state,
                author: pr.user.login,
                created_at: pr.created_at,
                html_url: pr.html_url,
                head_ref: pr.head.ref_name,
                base_ref: pr.base.ref_name,
                mergeable: pr.mergeable,
            })
            .collect())
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
        let req_body = serde_json::json!({
            "title": params.title,
            "body": params.body,
            "head": params.head,
            "base": params.base,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", auth_header(token))
            .json(&req_body)
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
            return Err(extract_error(resp).await);
        }

        let pr: GtPR = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse PR response: {e}")))?;

        Ok(ForgePR {
            number: pr.number,
            title: pr.title,
            state: pr.state,
            author: pr.user.login,
            created_at: pr.created_at,
            html_url: pr.html_url,
            head_ref: pr.head.ref_name,
            base_ref: pr.base.ref_name,
            mergeable: pr.mergeable,
        })
    }

    async fn list_issues(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<ForgeIssue>, PlatformError> {
        self.rate_limiter.acquire().await;
        // `type=issues` excludes PRs from the results (Gitea returns PRs as issues by default)
        let url = format!(
            "{}/repos/{}/{}/issues?state={}&limit=30&type=issues",
            self.api_base, owner, repo, state
        );
        let resp = self
            .client
            .get(&url)
            .header("Authorization", auth_header(token))
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
            return Err(extract_error(resp).await);
        }

        let issues: Vec<GtIssue> = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse issues response: {e}"))
        })?;

        Ok(issues
            .into_iter()
            .map(|i| ForgeIssue {
                number: i.number,
                title: i.title,
                state: i.state,
                author: i.user.login.clone(),
                created_at: i.created_at,
                html_url: i.html_url,
                labels: i
                    .labels
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.name)
                    .collect(),
                assignee: i.assignee.map(|a| a.login),
            })
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
        let req_body = serde_json::json!({
            "title": title,
            "body": body,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", auth_header(token))
            .json(&req_body)
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
            return Err(extract_error(resp).await);
        }

        let issue: GtIssue = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse issue response: {e}"))
        })?;

        Ok(ForgeIssue {
            number: issue.number,
            title: issue.title,
            state: issue.state,
            author: issue.user.login,
            created_at: issue.created_at,
            html_url: issue.html_url,
            labels: issue
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
            assignee: issue.assignee.map(|a| a.login),
        })
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
            .header("Authorization", auth_header(token))
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
            return Err(extract_error(resp).await);
        }

        let detail: GtIssueDetail = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse issue detail response: {e}"))
        })?;

        Ok(ForgeIssueDetail {
            number: detail.number,
            title: detail.title,
            state: detail.state,
            author: detail.user.login,
            created_at: detail.created_at,
            updated_at: detail.updated_at,
            html_url: detail.html_url,
            body: detail.body.unwrap_or_default(),
            labels: detail
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
            assignees: detail
                .assignees
                .unwrap_or_default()
                .into_iter()
                .map(|a| a.login)
                .collect(),
            milestone: detail.milestone.map(|m| m.title),
            closed_at: detail.closed_at,
        })
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
            .header("Authorization", auth_header(token))
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
            return Err(extract_error(resp).await);
        }
        Ok(())
    }
}
