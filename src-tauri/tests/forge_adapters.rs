//! Integration tests for forge adapters (GitHub, GitLab, Gitea) using mock HTTP.
//!
//! Each adapter is tested against a local mockito server that returns realistic
//! JSON payloads. Tests verify correct URL construction, header usage, response
//! normalization, and error mapping.

use bg3_cmty_studio_lib::git::forge::ForgeAdapter;
use bg3_cmty_studio_lib::git::forge_gitea::GiteaAdapter;
use bg3_cmty_studio_lib::git::forge_github::GitHubAdapter;
use bg3_cmty_studio_lib::git::forge_gitlab::GitLabAdapter;
use bg3_cmty_studio_lib::platform::errors::PlatformError;
use mockito::Matcher;

const TOKEN: &str = "test-token-abc123";

// ===========================================================================
// GitHub adapter tests
// ===========================================================================

mod github {
    use super::*;

    async fn setup() -> (mockito::ServerGuard, GitHubAdapter) {
        let server = mockito::Server::new_async().await;
        let adapter = GitHubAdapter::with_api_base(&server.url());
        (server, adapter)
    }

    #[tokio::test]
    async fn validate_token_returns_forge_user() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/user")
            .match_header("Authorization", "Bearer test-token-abc123")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "login": "octocat",
                    "name": "The Octocat",
                    "avatar_url": "https://avatars.example.com/octocat.png"
                }"#,
            )
            .create_async()
            .await;

        let user = adapter.validate_token(TOKEN).await.unwrap();
        assert_eq!(user.login, "octocat");
        assert_eq!(user.name, Some("The Octocat".to_string()));
        assert_eq!(user.avatar_url, "https://avatars.example.com/octocat.png");
        assert_eq!(user.forge_id, "github:github.com");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_repos_normalizes_response() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/user/repos")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{
                    "full_name": "octocat/hello-world",
                    "html_url": "https://github.com/octocat/hello-world",
                    "clone_url": "https://github.com/octocat/hello-world.git",
                    "private": false,
                    "description": "A test repo",
                    "default_branch": "main"
                }]"#,
            )
            .create_async()
            .await;

        let repos = adapter.list_repos(TOKEN, 1).await.unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].full_name, "octocat/hello-world");
        assert!(!repos[0].private);
        assert_eq!(repos[0].default_branch, "main");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_prs_returns_forge_prs() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{
                    "number": 42,
                    "title": "Add feature",
                    "state": "open",
                    "user": {"login": "octocat"},
                    "created_at": "2025-01-01T00:00:00Z",
                    "html_url": "https://github.com/owner/repo/pull/42",
                    "head": {"ref": "feature-branch"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .create_async()
            .await;

        let prs = adapter
            .list_prs(TOKEN, "owner", "repo", "open")
            .await
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[0].title, "Add feature");
        assert_eq!(prs[0].state, "open");
        assert_eq!(prs[0].author, "octocat");
        assert_eq!(prs[0].head_ref, "feature-branch");
        assert_eq!(prs[0].base_ref, "main");
        assert!(prs[0].mergeable.is_none()); // Not available in list endpoint
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_issues_filters_out_prs() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/issues")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {
                        "number": 1,
                        "title": "Real issue",
                        "state": "open",
                        "user": {"login": "alice"},
                        "created_at": "2025-01-01T00:00:00Z",
                        "html_url": "https://github.com/owner/repo/issues/1",
                        "labels": [{"name": "bug"}],
                        "assignee": null
                    },
                    {
                        "number": 2,
                        "title": "PR masquerading as issue",
                        "state": "open",
                        "user": {"login": "bob"},
                        "created_at": "2025-01-02T00:00:00Z",
                        "html_url": "https://github.com/owner/repo/issues/2",
                        "labels": [],
                        "assignee": null,
                        "pull_request": {"url": "https://api.github.com/repos/owner/repo/pulls/2"}
                    }
                ]"#,
            )
            .create_async()
            .await;

        let issues = adapter
            .list_issues(TOKEN, "owner", "repo", "open")
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[0].title, "Real issue");
        assert_eq!(issues[0].labels, vec!["bug"]);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn get_issue_returns_detail() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/issues/1")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "number": 1,
                    "title": "Bug report",
                    "state": "open",
                    "user": {"login": "alice"},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-02T00:00:00Z",
                    "closed_at": null,
                    "html_url": "https://github.com/owner/repo/issues/1",
                    "body": "This is the body of the issue.",
                    "labels": [{"name": "bug"}, {"name": "high-priority"}],
                    "assignees": [{"login": "bob"}, {"login": "carol"}],
                    "milestone": {"title": "v1.0"}
                }"#,
            )
            .create_async()
            .await;

        let detail = adapter.get_issue(TOKEN, "owner", "repo", 1).await.unwrap();
        assert_eq!(detail.number, 1);
        assert_eq!(detail.title, "Bug report");
        assert_eq!(detail.state, "open");
        assert_eq!(detail.author, "alice");
        assert_eq!(detail.body, "This is the body of the issue.");
        assert_eq!(detail.labels, vec!["bug", "high-priority"]);
        assert_eq!(detail.assignees, vec!["bob", "carol"]);
        assert_eq!(detail.milestone, Some("v1.0".to_string()));
        assert!(detail.closed_at.is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn assign_issue_succeeds() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/issues/1/assignees")
            .match_header("Authorization", "Bearer test-token-abc123")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{}"#)
            .create_async()
            .await;

        adapter
            .assign_issue(TOKEN, "owner", "repo", 1, "bob")
            .await
            .unwrap();
        mock.assert_async().await;
    }
}

// ===========================================================================
// GitLab adapter tests
// ===========================================================================

mod gitlab {
    use super::*;

    async fn setup() -> (mockito::ServerGuard, GitLabAdapter) {
        let server = mockito::Server::new_async().await;
        let adapter = GitLabAdapter::with_api_base(&server.url());
        (server, adapter)
    }

    #[tokio::test]
    async fn validate_token_uses_private_token_header() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/user")
            .match_header("PRIVATE-TOKEN", "test-token-abc123")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "username": "gitlab_user",
                    "name": "GitLab User",
                    "avatar_url": "https://gitlab.example.com/avatar.png"
                }"#,
            )
            .create_async()
            .await;

        let user = adapter.validate_token(TOKEN).await.unwrap();
        assert_eq!(user.login, "gitlab_user");
        assert_eq!(user.name, Some("GitLab User".to_string()));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_prs_normalizes_opened_to_open() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/projects/owner%2Frepo/merge_requests")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{
                    "iid": 10,
                    "title": "Add feature",
                    "state": "opened",
                    "author": {"username": "dev"},
                    "created_at": "2025-01-01T00:00:00Z",
                    "web_url": "https://gitlab.com/owner/repo/-/merge_requests/10",
                    "source_branch": "feature",
                    "target_branch": "main",
                    "has_conflicts": null
                }]"#,
            )
            .create_async()
            .await;

        let prs = adapter
            .list_prs(TOKEN, "owner", "repo", "all")
            .await
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].state, "open"); // "opened" → "open"
        assert_eq!(prs[0].number, 10); // iid mapped to number
        assert_eq!(
            prs[0].html_url,
            "https://gitlab.com/owner/repo/-/merge_requests/10"
        ); // web_url → html_url
        assert_eq!(prs[0].head_ref, "feature"); // source_branch → head_ref
        assert_eq!(prs[0].base_ref, "main"); // target_branch → base_ref
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_prs_has_conflicts_inverts_to_mergeable() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/projects/owner%2Frepo/merge_requests")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {
                        "iid": 10,
                        "title": "Conflicting MR",
                        "state": "opened",
                        "author": {"username": "dev"},
                        "created_at": "2025-01-01T00:00:00Z",
                        "web_url": "https://gitlab.com/owner/repo/-/merge_requests/10",
                        "source_branch": "feature",
                        "target_branch": "main",
                        "has_conflicts": true
                    },
                    {
                        "iid": 11,
                        "title": "Clean MR",
                        "state": "opened",
                        "author": {"username": "dev"},
                        "created_at": "2025-01-02T00:00:00Z",
                        "web_url": "https://gitlab.com/owner/repo/-/merge_requests/11",
                        "source_branch": "other",
                        "target_branch": "main",
                        "has_conflicts": false
                    }
                ]"#,
            )
            .create_async()
            .await;

        let prs = adapter
            .list_prs(TOKEN, "owner", "repo", "all")
            .await
            .unwrap();
        assert_eq!(prs[0].mergeable, Some(false)); // has_conflicts: true → mergeable: false
        assert_eq!(prs[1].mergeable, Some(true)); // has_conflicts: false → mergeable: true
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn get_issue_uses_iid_and_maps_fields() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/projects/owner%2Frepo/issues/42")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "iid": 42,
                    "title": "Feature request",
                    "state": "opened",
                    "author": {"username": "reporter"},
                    "created_at": "2025-01-01T00:00:00Z",
                    "updated_at": "2025-01-05T00:00:00Z",
                    "closed_at": null,
                    "web_url": "https://gitlab.com/owner/repo/-/issues/42",
                    "description": "Please add this feature.",
                    "labels": ["enhancement"],
                    "assignees": [{"username": "dev1"}, {"username": "dev2"}],
                    "milestone": {"title": "Q1 2025"}
                }"#,
            )
            .create_async()
            .await;

        let detail = adapter.get_issue(TOKEN, "owner", "repo", 42).await.unwrap();
        assert_eq!(detail.number, 42); // iid → number
        assert_eq!(detail.state, "open"); // "opened" → "open"
        assert_eq!(detail.html_url, "https://gitlab.com/owner/repo/-/issues/42"); // web_url → html_url
        assert_eq!(detail.body, "Please add this feature."); // description → body
        assert_eq!(detail.author, "reporter");
        assert_eq!(detail.assignees, vec!["dev1", "dev2"]);
        assert_eq!(detail.milestone, Some("Q1 2025".to_string()));
        assert!(detail.closed_at.is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn assign_issue_looks_up_user_id_then_puts() {
        let (mut server, adapter) = setup().await;

        // Step 1: GitLab looks up the user by username to get numeric ID
        let user_mock = server
            .mock("GET", "/users")
            .match_query(Matcher::UrlEncoded("username".into(), "testuser".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"[{"id": 789}]"#)
            .create_async()
            .await;

        // Step 2: PUT /projects/:encoded/issues/:iid with assignee_ids
        let assign_mock = server
            .mock("PUT", "/projects/owner%2Frepo/issues/5")
            .match_header("PRIVATE-TOKEN", "test-token-abc123")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{}"#)
            .create_async()
            .await;

        adapter
            .assign_issue(TOKEN, "owner", "repo", 5, "testuser")
            .await
            .unwrap();

        user_mock.assert_async().await;
        assign_mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_issues_normalizes_state_and_maps_web_url() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/projects/owner%2Frepo/issues")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{
                    "iid": 7,
                    "title": "A bug",
                    "state": "opened",
                    "author": {"username": "reporter"},
                    "created_at": "2025-03-01T00:00:00Z",
                    "web_url": "https://gitlab.com/owner/repo/-/issues/7",
                    "labels": ["bug", "critical"],
                    "assignee": {"username": "fixer"}
                }]"#,
            )
            .create_async()
            .await;

        let issues = adapter
            .list_issues(TOKEN, "owner", "repo", "opened")
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 7); // iid → number
        assert_eq!(issues[0].state, "open"); // "opened" → "open"
        assert_eq!(
            issues[0].html_url,
            "https://gitlab.com/owner/repo/-/issues/7"
        ); // web_url → html_url
        assert_eq!(issues[0].labels, vec!["bug", "critical"]);
        assert_eq!(issues[0].assignee, Some("fixer".to_string()));
        mock.assert_async().await;
    }
}

// ===========================================================================
// Gitea adapter tests
// ===========================================================================

mod gitea {
    use super::*;

    async fn setup() -> (mockito::ServerGuard, GiteaAdapter) {
        let server = mockito::Server::new_async().await;
        let adapter = GiteaAdapter::new("localhost", &server.url());
        (server, adapter)
    }

    #[tokio::test]
    async fn validate_token_uses_token_auth_not_bearer() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/user")
            .match_header("Authorization", "token test-token-abc123")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "login": "gitea_user",
                    "full_name": "Gitea User",
                    "avatar_url": "https://codeberg.org/avatar.png"
                }"#,
            )
            .create_async()
            .await;

        let user = adapter.validate_token(TOKEN).await.unwrap();
        assert_eq!(user.login, "gitea_user");
        assert_eq!(user.name, Some("Gitea User".to_string()));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_prs_uses_limit_not_per_page() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(Matcher::UrlEncoded("limit".into(), "30".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{
                    "number": 1,
                    "title": "First PR",
                    "state": "open",
                    "user": {"login": "contributor"},
                    "created_at": "2025-01-01T00:00:00Z",
                    "html_url": "https://codeberg.org/owner/repo/pulls/1",
                    "head": {"ref": "feature"},
                    "base": {"ref": "main"},
                    "mergeable": true
                }]"#,
            )
            .create_async()
            .await;

        let prs = adapter
            .list_prs(TOKEN, "owner", "repo", "open")
            .await
            .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[0].mergeable, Some(true)); // direct passthrough (no inversion)
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_issues_uses_type_filter() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/issues")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("type".into(), "issues".into()),
                Matcher::UrlEncoded("limit".into(), "30".into()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[{
                    "number": 5,
                    "title": "Bug in parser",
                    "state": "open",
                    "user": {"login": "reporter"},
                    "created_at": "2025-02-01T00:00:00Z",
                    "html_url": "https://codeberg.org/owner/repo/issues/5",
                    "labels": [{"name": "bug"}],
                    "assignee": {"login": "fixer"}
                }]"#,
            )
            .create_async()
            .await;

        let issues = adapter
            .list_issues(TOKEN, "owner", "repo", "open")
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 5);
        assert_eq!(issues[0].labels, vec!["bug"]);
        assert_eq!(issues[0].assignee, Some("fixer".to_string()));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn get_issue_returns_detail() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/issues/3")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "number": 3,
                    "title": "Detailed issue",
                    "state": "open",
                    "user": {"login": "author"},
                    "created_at": "2025-01-15T00:00:00Z",
                    "updated_at": "2025-01-20T00:00:00Z",
                    "closed_at": null,
                    "html_url": "https://codeberg.org/owner/repo/issues/3",
                    "body": "Full description of the issue.",
                    "labels": [{"name": "enhancement"}, {"name": "help wanted"}],
                    "assignees": [{"login": "dev1"}],
                    "milestone": {"title": "v2.0"}
                }"#,
            )
            .create_async()
            .await;

        let detail = adapter.get_issue(TOKEN, "owner", "repo", 3).await.unwrap();
        assert_eq!(detail.number, 3);
        assert_eq!(detail.body, "Full description of the issue.");
        assert_eq!(detail.labels, vec!["enhancement", "help wanted"]);
        assert_eq!(detail.assignees, vec!["dev1"]);
        assert_eq!(detail.milestone, Some("v2.0".to_string()));
        assert!(detail.closed_at.is_none());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn assign_issue_succeeds() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/issues/3/assignees")
            .match_header("Authorization", "token test-token-abc123")
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(r#"{}"#)
            .create_async()
            .await;

        adapter
            .assign_issue(TOKEN, "owner", "repo", 3, "dev1")
            .await
            .unwrap();
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn mergeable_passthrough_no_inversion() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"[
                    {
                        "number": 1,
                        "title": "Mergeable PR",
                        "state": "open",
                        "user": {"login": "dev"},
                        "created_at": "2025-01-01T00:00:00Z",
                        "html_url": "https://codeberg.org/owner/repo/pulls/1",
                        "head": {"ref": "a"},
                        "base": {"ref": "main"},
                        "mergeable": true
                    },
                    {
                        "number": 2,
                        "title": "Non-mergeable PR",
                        "state": "open",
                        "user": {"login": "dev"},
                        "created_at": "2025-01-02T00:00:00Z",
                        "html_url": "https://codeberg.org/owner/repo/pulls/2",
                        "head": {"ref": "b"},
                        "base": {"ref": "main"},
                        "mergeable": false
                    }
                ]"#,
            )
            .create_async()
            .await;

        let prs = adapter
            .list_prs(TOKEN, "owner", "repo", "open")
            .await
            .unwrap();
        assert_eq!(prs[0].mergeable, Some(true)); // direct passthrough
        assert_eq!(prs[1].mergeable, Some(false)); // direct passthrough (NOT inverted like GitLab)
        mock.assert_async().await;
    }
}

// ===========================================================================
// Error response tests (using GitHub adapter as representative)
// ===========================================================================

mod error_cases {
    use super::*;

    async fn setup() -> (mockito::ServerGuard, GitHubAdapter) {
        let server = mockito::Server::new_async().await;
        let adapter = GitHubAdapter::with_api_base(&server.url());
        (server, adapter)
    }

    #[tokio::test]
    async fn error_401_auth_failure() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/user")
            .with_status(401)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message": "Bad credentials"}"#)
            .create_async()
            .await;

        let err = adapter.validate_token("bad-token").await.unwrap_err();
        match err {
            PlatformError::ApiError { status, message } => {
                assert_eq!(status, 401);
                assert!(
                    message.contains("Authentication failed") || message.contains("authentication"),
                    "Expected auth error message, got: {message}"
                );
            }
            other => panic!("Expected ApiError with status 401, got: {other:?}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn error_403_permission_denied() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/repo/pulls")
            .match_query(Matcher::Any)
            .with_status(403)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message": "Resource not accessible by integration"}"#)
            .create_async()
            .await;

        let err = adapter
            .list_prs(TOKEN, "owner", "repo", "open")
            .await
            .unwrap_err();
        match err {
            PlatformError::ApiError { status, message } => {
                assert_eq!(status, 403);
                assert!(
                    message.contains("Access denied") || message.contains("permission"),
                    "Expected permission error message, got: {message}"
                );
            }
            other => panic!("Expected ApiError with status 403, got: {other:?}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn error_404_not_found() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/repos/owner/nonexistent/issues/999")
            .with_status(404)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message": "Not Found"}"#)
            .create_async()
            .await;

        let err = adapter
            .get_issue(TOKEN, "owner", "nonexistent", 999)
            .await
            .unwrap_err();
        match err {
            PlatformError::ApiError { status, message } => {
                assert_eq!(status, 404);
                assert!(
                    message.to_lowercase().contains("not found"),
                    "Expected not-found error message, got: {message}"
                );
            }
            other => panic!("Expected ApiError with status 404, got: {other:?}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn error_422_validation() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/issues")
            .with_status(422)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message": "Validation Failed: title is missing"}"#)
            .create_async()
            .await;

        let err = adapter
            .create_issue(TOKEN, "owner", "repo", "", "")
            .await
            .unwrap_err();
        match err {
            PlatformError::ApiError { status, message } => {
                assert_eq!(status, 422);
                assert!(
                    message.contains("Validation error") || message.contains("Validation Failed"),
                    "Expected validation error message, got: {message}"
                );
            }
            other => panic!("Expected ApiError with status 422, got: {other:?}"),
        }
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn error_500_server_error() {
        let (mut server, adapter) = setup().await;
        let mock = server
            .mock("GET", "/user/repos")
            .match_query(Matcher::Any)
            .with_status(500)
            .with_header("content-type", "application/json")
            .with_body(r#"{"message": "Internal Server Error"}"#)
            .create_async()
            .await;

        let err = adapter.list_repos(TOKEN, 1).await.unwrap_err();
        match err {
            PlatformError::ApiError { status, message } => {
                assert_eq!(status, 500);
                assert!(
                    message.contains("500"),
                    "Expected 500 in error message, got: {message}"
                );
            }
            other => panic!("Expected ApiError with status 500, got: {other:?}"),
        }
        mock.assert_async().await;
    }
}
