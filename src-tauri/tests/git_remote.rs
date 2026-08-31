//! Integration tests for git remote operations using git2 directly.
//!
//! Network operations (fetch/push/pull) are not tested here — only local
//! remote configuration management.

use git2::Repository;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn init_repo(dir: &TempDir) -> Repository {
    Repository::init(dir.path()).unwrap()
}

// ---------------------------------------------------------------------------
// Add remote
// ---------------------------------------------------------------------------

#[test]
fn add_remote_and_find_it() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo(&dir);

    repo.remote("origin", "https://github.com/user/repo.git")
        .unwrap();

    let remote = repo.find_remote("origin").unwrap();
    assert_eq!(remote.url().unwrap(), "https://github.com/user/repo.git");
}

// ---------------------------------------------------------------------------
// Remove remote
// ---------------------------------------------------------------------------

#[test]
fn remove_remote() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo(&dir);

    repo.remote("origin", "https://github.com/user/repo.git")
        .unwrap();
    assert!(repo.find_remote("origin").is_ok());

    repo.remote_delete("origin").unwrap();
    assert!(repo.find_remote("origin").is_err());
}

// ---------------------------------------------------------------------------
// List remotes
// ---------------------------------------------------------------------------

#[test]
fn list_multiple_remotes() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo(&dir);

    repo.remote("origin", "https://github.com/user/repo.git")
        .unwrap();
    repo.remote("upstream", "https://github.com/org/repo.git")
        .unwrap();

    let remotes = repo.remotes().unwrap();
    let names: Vec<&str> = remotes.iter().flatten().collect();

    assert!(names.contains(&"origin"));
    assert!(names.contains(&"upstream"));
    assert_eq!(names.len(), 2);
}

// ---------------------------------------------------------------------------
// Invalid URL format — git allows arbitrary strings as URLs
// ---------------------------------------------------------------------------

#[test]
fn arbitrary_url_is_accepted() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo(&dir);

    // git allows any string as a remote URL (it only fails at fetch/push time)
    repo.remote("weird", "not-a-real-url").unwrap();

    let remote = repo.find_remote("weird").unwrap();
    assert_eq!(remote.url().unwrap(), "not-a-real-url");
}

// ---------------------------------------------------------------------------
// Push URL can be different from fetch URL
// ---------------------------------------------------------------------------

#[test]
fn remote_push_url() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo(&dir);

    repo.remote("origin", "https://github.com/user/repo.git")
        .unwrap();

    // Set a different push URL
    repo.remote_set_pushurl("origin", Some("git@github.com:user/repo.git"))
        .unwrap();

    let remote = repo.find_remote("origin").unwrap();
    assert_eq!(remote.url().unwrap(), "https://github.com/user/repo.git");
    assert_eq!(remote.pushurl().unwrap(), "git@github.com:user/repo.git");
}

// ---------------------------------------------------------------------------
// Duplicate remote name errors
// ---------------------------------------------------------------------------

#[test]
fn duplicate_remote_name_errors() {
    let dir = TempDir::new().unwrap();
    let repo = init_repo(&dir);

    repo.remote("origin", "https://github.com/user/repo.git")
        .unwrap();

    // Adding a second remote with the same name should fail
    let result = repo.remote("origin", "https://gitlab.com/user/repo.git");
    assert!(result.is_err());
}
