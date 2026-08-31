//! Forge detection and API abstraction (GitHub, GitLab, Gitea/Codeberg).

use super::types::{
    CreatePrParams, ForgeInfo, ForgeIssue, ForgeIssueDetail, ForgePR, ForgeRepo, ForgeType,
    ForgeUser,
};
use crate::platform::errors::PlatformError;

// ---------------------------------------------------------------------------
// ForgeAdapter trait (native async fn — Rust 1.75+)
// ---------------------------------------------------------------------------

#[allow(async_fn_in_trait)] // Internal trait — Send bound not needed at trait level
pub trait ForgeAdapter: Send + Sync {
    fn forge_type(&self) -> ForgeType;
    fn api_base(&self) -> &str;
    fn token_creation_url(&self) -> String;
    /// "Pull Request" (GitHub/Gitea) or "Merge Request" (GitLab).
    fn pr_label(&self) -> &str;

    async fn validate_token(&self, token: &str) -> Result<ForgeUser, PlatformError>;
    async fn list_repos(&self, token: &str, page: u32) -> Result<Vec<ForgeRepo>, PlatformError>;
    async fn create_repo(
        &self,
        token: &str,
        name: &str,
        description: &str,
        private: bool,
    ) -> Result<ForgeRepo, PlatformError>;
    async fn list_prs(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<ForgePR>, PlatformError>;
    async fn create_pr(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        params: &CreatePrParams,
    ) -> Result<ForgePR, PlatformError>;
    async fn list_issues(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<ForgeIssue>, PlatformError>;
    async fn create_issue(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<ForgeIssue, PlatformError>;
    async fn get_issue(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        number: u32,
    ) -> Result<ForgeIssueDetail, PlatformError>;
    async fn assign_issue(
        &self,
        token: &str,
        owner: &str,
        repo: &str,
        number: u32,
        assignee: &str,
    ) -> Result<(), PlatformError>;
}

// ---------------------------------------------------------------------------
// Forge detection
// ---------------------------------------------------------------------------

/// Parse a remote URL (HTTPS or SSH) and return forge metadata.
pub fn detect_forge(remote_url: &str) -> ForgeInfo {
    let (host, path) = parse_remote_url(remote_url);

    let (forge_type, api_base) = match host.as_str() {
        "github.com" => (ForgeType::GitHub, "https://api.github.com".to_string()),
        "gitlab.com" => (ForgeType::GitLab, "https://gitlab.com/api/v4".to_string()),
        "codeberg.org" => (ForgeType::Gitea, "https://codeberg.org/api/v1".to_string()),
        _ => (ForgeType::Unknown, String::new()),
    };

    let forge_id = format!(
        "{}:{}",
        match forge_type {
            ForgeType::GitHub => "github",
            ForgeType::GitLab => "gitlab",
            ForgeType::Gitea => "gitea",
            ForgeType::Unknown => "unknown",
        },
        host
    );

    let (owner, repo) = split_owner_repo(&path, &forge_type);

    ForgeInfo {
        forge_type,
        host,
        api_base,
        forge_id,
        owner,
        repo,
    }
}

/// Parse host and path from an HTTPS or SSH remote URL.
fn parse_remote_url(url: &str) -> (String, String) {
    // SSH: git@host:path.git
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some(colon) = rest.find(':') {
            let host = &rest[..colon];
            let path = rest[colon + 1..].trim_end_matches(".git");
            return (host.to_string(), path.to_string());
        }
    }

    // HTTPS: https://host/path.git (also handle http://)
    if url.starts_with("https://") || url.starts_with("http://") {
        if let Some(after_scheme) = url.split_once("://").map(|(_, r)| r) {
            if let Some(slash) = after_scheme.find('/') {
                let host = &after_scheme[..slash];
                let path = after_scheme[slash + 1..].trim_end_matches(".git");
                return (host.to_string(), path.to_string());
            }
        }
    }

    (String::new(), String::new())
}

/// Split path into (owner, repo). GitLab supports subgroups (owner = "group/subgroup").
fn split_owner_repo(path: &str, forge_type: &ForgeType) -> (Option<String>, Option<String>) {
    if path.is_empty() {
        return (None, None);
    }

    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return (Some(parts.join("/")), None);
    }

    match forge_type {
        // GitLab supports subgroups: everything except last segment is the "owner"
        ForgeType::GitLab => {
            let repo = parts.last().unwrap().to_string();
            let owner = parts[..parts.len() - 1].join("/");
            (Some(owner), Some(repo))
        }
        // GitHub / Gitea / Unknown: first segment is owner, second is repo
        _ => {
            let owner = parts[0].to_string();
            let repo = parts[1].to_string();
            (Some(owner), Some(repo))
        }
    }
}

// ---------------------------------------------------------------------------
// URL construction helpers for "Open on Forge"
// ---------------------------------------------------------------------------

/// Build a URL to view a file on the forge web UI.
pub fn forge_file_url(info: &ForgeInfo, branch: &str, path: &str) -> Option<String> {
    let owner = info.owner.as_deref()?;
    let repo = info.repo.as_deref()?;

    match info.forge_type {
        ForgeType::GitHub => Some(format!(
            "https://github.com/{owner}/{repo}/blob/{branch}/{path}"
        )),
        ForgeType::GitLab => Some(format!(
            "https://{host}/{owner}/{repo}/-/blob/{branch}/{path}",
            host = info.host
        )),
        ForgeType::Gitea => Some(format!(
            "https://{host}/{owner}/{repo}/src/branch/{branch}/{path}",
            host = info.host
        )),
        ForgeType::Unknown => None,
    }
}

/// Build a URL to view a commit on the forge web UI.
pub fn forge_commit_url(info: &ForgeInfo, sha: &str) -> Option<String> {
    let owner = info.owner.as_deref()?;
    let repo = info.repo.as_deref()?;

    match info.forge_type {
        ForgeType::GitHub => Some(format!("https://github.com/{owner}/{repo}/commit/{sha}")),
        ForgeType::GitLab => Some(format!(
            "https://{host}/{owner}/{repo}/-/commit/{sha}",
            host = info.host
        )),
        ForgeType::Gitea => Some(format!(
            "https://{host}/{owner}/{repo}/commit/{sha}",
            host = info.host
        )),
        ForgeType::Unknown => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- detect_forge ---------------------------------------------------------

    #[test]
    fn github_https() {
        let info = detect_forge("https://github.com/owner/repo.git");
        assert!(matches!(info.forge_type, ForgeType::GitHub));
        assert_eq!(info.host, "github.com");
        assert_eq!(info.api_base, "https://api.github.com");
        assert_eq!(info.forge_id, "github:github.com");
        assert_eq!(info.owner.as_deref(), Some("owner"));
        assert_eq!(info.repo.as_deref(), Some("repo"));
    }

    #[test]
    fn github_ssh() {
        let info = detect_forge("git@github.com:owner/repo.git");
        assert!(matches!(info.forge_type, ForgeType::GitHub));
        assert_eq!(info.host, "github.com");
        assert_eq!(info.owner.as_deref(), Some("owner"));
        assert_eq!(info.repo.as_deref(), Some("repo"));
    }

    #[test]
    fn gitlab_https_with_subgroups() {
        let info = detect_forge("https://gitlab.com/group/subgroup/repo.git");
        assert!(matches!(info.forge_type, ForgeType::GitLab));
        assert_eq!(info.host, "gitlab.com");
        assert_eq!(info.api_base, "https://gitlab.com/api/v4");
        assert_eq!(info.forge_id, "gitlab:gitlab.com");
        assert_eq!(info.owner.as_deref(), Some("group/subgroup"));
        assert_eq!(info.repo.as_deref(), Some("repo"));
    }

    #[test]
    fn codeberg_https() {
        let info = detect_forge("https://codeberg.org/owner/repo.git");
        assert!(matches!(info.forge_type, ForgeType::Gitea));
        assert_eq!(info.host, "codeberg.org");
        assert_eq!(info.api_base, "https://codeberg.org/api/v1");
        assert_eq!(info.forge_id, "gitea:codeberg.org");
        assert_eq!(info.owner.as_deref(), Some("owner"));
        assert_eq!(info.repo.as_deref(), Some("repo"));
    }

    #[test]
    fn unknown_host() {
        let info = detect_forge("https://example.com/foo/bar.git");
        assert!(matches!(info.forge_type, ForgeType::Unknown));
        assert_eq!(info.host, "example.com");
        assert!(info.api_base.is_empty());
        assert_eq!(info.forge_id, "unknown:example.com");
    }

    // -- forge_file_url -------------------------------------------------------

    #[test]
    fn file_url_github() {
        let info = detect_forge("https://github.com/owner/repo.git");
        let url = forge_file_url(&info, "main", "src/lib.rs");
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/owner/repo/blob/main/src/lib.rs")
        );
    }

    #[test]
    fn file_url_gitlab() {
        let info = detect_forge("https://gitlab.com/group/repo.git");
        let url = forge_file_url(&info, "develop", "README.md");
        assert_eq!(
            url.as_deref(),
            Some("https://gitlab.com/group/repo/-/blob/develop/README.md")
        );
    }

    #[test]
    fn file_url_gitea() {
        let info = detect_forge("https://codeberg.org/owner/repo.git");
        let url = forge_file_url(&info, "main", "docs/setup.md");
        assert_eq!(
            url.as_deref(),
            Some("https://codeberg.org/owner/repo/src/branch/main/docs/setup.md")
        );
    }

    #[test]
    fn file_url_unknown_returns_none() {
        let info = detect_forge("https://example.com/foo/bar.git");
        assert!(forge_file_url(&info, "main", "file.txt").is_none());
    }

    // -- forge_commit_url -----------------------------------------------------

    #[test]
    fn commit_url_github() {
        let info = detect_forge("https://github.com/owner/repo.git");
        let url = forge_commit_url(&info, "abc123");
        assert_eq!(
            url.as_deref(),
            Some("https://github.com/owner/repo/commit/abc123")
        );
    }

    #[test]
    fn commit_url_gitlab() {
        let info = detect_forge("https://gitlab.com/group/repo.git");
        let url = forge_commit_url(&info, "def456");
        assert_eq!(
            url.as_deref(),
            Some("https://gitlab.com/group/repo/-/commit/def456")
        );
    }

    #[test]
    fn commit_url_gitea() {
        let info = detect_forge("https://codeberg.org/owner/repo.git");
        let url = forge_commit_url(&info, "789abc");
        assert_eq!(
            url.as_deref(),
            Some("https://codeberg.org/owner/repo/commit/789abc")
        );
    }

    #[test]
    fn commit_url_unknown_returns_none() {
        let info = detect_forge("https://example.com/foo/bar.git");
        assert!(forge_commit_url(&info, "abc").is_none());
    }
}
