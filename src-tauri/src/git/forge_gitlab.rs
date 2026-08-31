//! GitLab forge adapter — implements `ForgeAdapter` for GitLab.com and self-hosted instances.

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
// Deserialization structs (private)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GlUser {
    username: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GlProject {
    path_with_namespace: String,
    web_url: String,
    http_url_to_repo: String,
    visibility: String,
    description: Option<String>,
    default_branch: Option<String>,
}

#[derive(Deserialize)]
struct GlAuthor {
    username: String,
}

#[derive(Deserialize)]
struct GlMR {
    iid: u32,
    title: String,
    state: String,
    author: GlAuthor,
    created_at: String,
    web_url: String,
    source_branch: String,
    target_branch: String,
    has_conflicts: Option<bool>,
}

#[derive(Deserialize)]
struct GlIssue {
    iid: u32,
    title: String,
    state: String,
    author: GlAuthor,
    created_at: String,
    web_url: String,
    labels: Vec<String>,
    assignee: Option<GlAuthor>,
}

#[derive(Deserialize)]
struct GlIssueDetail {
    iid: u32,
    title: String,
    state: String,
    author: GlAuthor,
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    web_url: String,
    description: Option<String>,
    labels: Vec<String>,
    assignees: Option<Vec<GlAuthor>>,
    milestone: Option<GlMilestone>,
}

#[derive(Deserialize)]
struct GlMilestone {
    title: String,
}

#[derive(Deserialize)]
struct GlUserFull {
    id: u64,
}

#[derive(Deserialize)]
struct GlError {
    message: Option<String>,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn encode_project_path(owner: &str, repo: &str) -> String {
    format!("{owner}/{repo}").replace('/', "%2F")
}

fn normalize_state(gitlab_state: &str) -> String {
    if gitlab_state == "opened" {
        "open".to_string()
    } else {
        gitlab_state.to_string()
    }
}

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
        _ => {
            let body = resp
                .json::<GlError>()
                .await
                .ok()
                .and_then(|e| e.message.or(e.error));
            let detail = body.unwrap_or_else(|| status.to_string());
            PlatformError::ApiError {
                status: code,
                message: format!("GitLab API error ({code}): {detail}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GitLabAdapter
// ---------------------------------------------------------------------------

pub struct GitLabAdapter {
    client: Client,
    api_base: String,
    host: String,
    rate_limiter: TokenBucket,
}

impl GitLabAdapter {
    pub fn new(host: &str) -> Self {
        let api_base = format!("https://{host}/api/v4");
        Self::with_api_base_and_host(&api_base, host)
    }

    /// Create a `GitLabAdapter` with a custom API base URL (for testing or non-standard deployments).
    pub fn with_api_base(api_base: &str) -> Self {
        Self::with_api_base_and_host(api_base, "custom")
    }

    fn with_api_base_and_host(api_base: &str, host: &str) -> Self {
        let client = build_client("CMTY-Studio", 30).expect("failed to build HTTP client");
        Self {
            client,
            api_base: api_base.to_string(),
            host: host.to_string(),
            rate_limiter: crate::git::rate_limits::gitlab_rate_limiter(),
        }
    }
}

impl ForgeAdapter for GitLabAdapter {
    fn forge_type(&self) -> ForgeType {
        ForgeType::GitLab
    }

    fn api_base(&self) -> &str {
        &self.api_base
    }

    fn token_creation_url(&self) -> String {
        format!(
            "https://{}/-/user_settings/personal_access_tokens",
            self.host
        )
    }

    fn pr_label(&self) -> &str {
        "Merge Request"
    }

    async fn validate_token(&self, token: &str) -> Result<ForgeUser, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!("{}/user", self.api_base);
        let resp = self
            .client
            .get(&url)
            .header("PRIVATE-TOKEN", token)
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

        let user: GlUser = resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse user response: {e}")))?;

        Ok(ForgeUser {
            login: user.username,
            name: user.name,
            avatar_url: user.avatar_url.unwrap_or_default(),
            forge_id: format!("gitlab:{}", self.host),
        })
    }

    async fn list_repos(&self, token: &str, page: u32) -> Result<Vec<ForgeRepo>, PlatformError> {
        self.rate_limiter.acquire().await;
        let url = format!(
            "{}/projects?membership=true&page={}&per_page=30&order_by=updated_at",
            self.api_base, page
        );
        let resp = self
            .client
            .get(&url)
            .header("PRIVATE-TOKEN", token)
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

        let projects: Vec<GlProject> = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse projects response: {e}"))
        })?;

        Ok(projects
            .into_iter()
            .map(|p| ForgeRepo {
                full_name: p.path_with_namespace,
                html_url: p.web_url,
                clone_url: p.http_url_to_repo,
                private: p.visibility == "private",
                description: p.description,
                default_branch: p.default_branch.unwrap_or_else(|| "main".to_string()),
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
        let url = format!("{}/projects", self.api_base);
        let visibility = if private { "private" } else { "public" };

        let body = serde_json::json!({
            "name": name,
            "description": description,
            "visibility": visibility,
        });

        let resp = self
            .client
            .post(&url)
            .header("PRIVATE-TOKEN", token)
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

        let p: GlProject = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse project response: {e}"))
        })?;

        Ok(ForgeRepo {
            full_name: p.path_with_namespace,
            html_url: p.web_url,
            clone_url: p.http_url_to_repo,
            private: p.visibility == "private",
            description: p.description,
            default_branch: p.default_branch.unwrap_or_else(|| "main".to_string()),
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
        let encoded = encode_project_path(owner, repo);
        let url = format!(
            "{}/projects/{}/merge_requests?state={}&scope=all&per_page=30",
            self.api_base, encoded, state
        );

        let resp = self
            .client
            .get(&url)
            .header("PRIVATE-TOKEN", token)
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

        let mrs: Vec<GlMR> = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse merge requests response: {e}"))
        })?;

        Ok(mrs
            .into_iter()
            .map(|mr| ForgePR {
                number: mr.iid,
                title: mr.title,
                state: normalize_state(&mr.state),
                author: mr.author.username,
                created_at: mr.created_at,
                html_url: mr.web_url,
                head_ref: mr.source_branch,
                base_ref: mr.target_branch,
                mergeable: mr.has_conflicts.map(|c| !c),
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
        let encoded = encode_project_path(owner, repo);
        let url = format!("{}/projects/{}/merge_requests", self.api_base, encoded);

        let payload = serde_json::json!({
            "title": params.title,
            "description": params.body,
            "source_branch": params.head,
            "target_branch": params.base,
        });

        let resp = self
            .client
            .post(&url)
            .header("PRIVATE-TOKEN", token)
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

        let mr: GlMR = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse merge request response: {e}"))
        })?;

        Ok(ForgePR {
            number: mr.iid,
            title: mr.title,
            state: normalize_state(&mr.state),
            author: mr.author.username,
            created_at: mr.created_at,
            html_url: mr.web_url,
            head_ref: mr.source_branch,
            base_ref: mr.target_branch,
            mergeable: mr.has_conflicts.map(|c| !c),
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
        let encoded = encode_project_path(owner, repo);
        let url = format!(
            "{}/projects/{}/issues?state={}&per_page=30",
            self.api_base, encoded, state
        );

        let resp = self
            .client
            .get(&url)
            .header("PRIVATE-TOKEN", token)
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

        let issues: Vec<GlIssue> = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse issues response: {e}"))
        })?;

        Ok(issues
            .into_iter()
            .map(|i| ForgeIssue {
                number: i.iid,
                title: i.title,
                state: normalize_state(&i.state),
                author: i.author.username,
                created_at: i.created_at,
                html_url: i.web_url,
                labels: i.labels,
                assignee: i.assignee.map(|a| a.username),
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
        let encoded = encode_project_path(owner, repo);
        let url = format!("{}/projects/{}/issues", self.api_base, encoded);

        let payload = serde_json::json!({
            "title": title,
            "description": body,
        });

        let resp = self
            .client
            .post(&url)
            .header("PRIVATE-TOKEN", token)
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

        let issue: GlIssue = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse issue response: {e}"))
        })?;

        Ok(ForgeIssue {
            number: issue.iid,
            title: issue.title,
            state: normalize_state(&issue.state),
            author: issue.author.username,
            created_at: issue.created_at,
            html_url: issue.web_url,
            labels: issue.labels,
            assignee: issue.assignee.map(|a| a.username),
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
        let encoded = encode_project_path(owner, repo);
        let url = format!("{}/projects/{}/issues/{}", self.api_base, encoded, number);

        let resp = self
            .client
            .get(&url)
            .header("PRIVATE-TOKEN", token)
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

        let detail: GlIssueDetail = resp.json().await.map_err(|e| {
            PlatformError::HttpError(format!("Failed to parse issue detail response: {e}"))
        })?;

        Ok(ForgeIssueDetail {
            number: detail.iid,
            title: detail.title,
            state: normalize_state(&detail.state),
            author: detail.author.username,
            created_at: detail.created_at,
            updated_at: detail.updated_at,
            html_url: detail.web_url,
            body: detail.description.unwrap_or_default(),
            labels: detail.labels,
            assignees: detail
                .assignees
                .unwrap_or_default()
                .into_iter()
                .map(|a| a.username)
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
        // GitLab needs the user's numeric ID for assignment.
        // Look up by username.
        self.rate_limiter.acquire().await;
        let user_url = format!("{}/users?username={}", self.api_base, assignee);
        let user_resp = self
            .client
            .get(&user_url)
            .header("PRIVATE-TOKEN", token)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    PlatformError::Timeout
                } else {
                    PlatformError::HttpError(e.to_string())
                }
            })?;

        if !user_resp.status().is_success() {
            return Err(extract_error(user_resp).await);
        }

        let users: Vec<GlUserFull> = user_resp
            .json()
            .await
            .map_err(|e| PlatformError::HttpError(format!("Failed to parse user response: {e}")))?;

        let user_id = users
            .first()
            .ok_or_else(|| PlatformError::ApiError {
                status: 404,
                message: format!("User '{assignee}' not found on GitLab"),
            })?
            .id;

        self.rate_limiter.acquire().await;
        let encoded = encode_project_path(owner, repo);
        let url = format!("{}/projects/{}/issues/{}", self.api_base, encoded, number);
        let payload = serde_json::json!({ "assignee_ids": [user_id] });

        let resp = self
            .client
            .put(&url)
            .header("PRIVATE-TOKEN", token)
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
