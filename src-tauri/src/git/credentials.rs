//! Git credential handling: HTTPS credential callback and commit identity resolution.

use crate::platform::credentials;

/// Extract hostname from a URL by splitting on `://` and `/`.
fn extract_host(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let host_port = after_scheme.split('/').next()?;
    // Strip optional user@ prefix
    let host = if let Some((_user, rest)) = host_port.split_once('@') {
        rest
    } else {
        host_port
    };
    // Strip optional :port suffix
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

/// HTTPS credential callback for `git2::RemoteCallbacks`.
///
/// Looks up `forge_token:<host>` in the OS keyring and returns
/// `userpass_plaintext("token", &stored_token)` if found.
pub fn https_credentials_callback(
    url: &str,
    _username_from_url: Option<&str>,
    _allowed_types: git2::CredentialType,
) -> Result<git2::Cred, git2::Error> {
    let host = extract_host(url).ok_or_else(|| {
        git2::Error::from_str(&format!("Could not parse hostname from URL: {url}"))
    })?;

    match credentials::get_credential("forge_token", &host) {
        Ok(Some(token)) if !token.is_empty() => git2::Cred::userpass_plaintext("token", &token),
        Ok(_) => Err(git2::Error::from_str(
            "No credentials stored for this host. \
             Please add a Personal Access Token in Settings > Git > Remote Accounts.",
        )),
        Err(e) => Err(git2::Error::from_str(&format!(
            "Failed to read token for '{host}': {e}"
        ))),
    }
}

/// Resolve the git commit identity from settings, falling back to global git config.
///
/// Fallback chain:
/// 1. `settings_name` / `settings_email` if both non-empty
/// 2. `git2::Config::open_default()` → `user.name` / `user.email`
/// 3. Error with guidance
pub fn resolve_git_identity(
    settings_name: &str,
    settings_email: &str,
) -> Result<(String, String), String> {
    let name = settings_name.trim();
    let email = settings_email.trim();

    if !name.is_empty() && !email.is_empty() {
        return Ok((name.to_string(), email.to_string()));
    }

    // Try global git config
    if let Ok(cfg) = git2::Config::open_default() {
        let cfg_name = cfg.get_string("user.name").ok();
        let cfg_email = cfg.get_string("user.email").ok();

        let final_name = if !name.is_empty() {
            name.to_string()
        } else {
            cfg_name.unwrap_or_default()
        };
        let final_email = if !email.is_empty() {
            email.to_string()
        } else {
            cfg_email.unwrap_or_default()
        };

        if !final_name.is_empty() && !final_email.is_empty() {
            return Ok((final_name, final_email));
        }
    }

    Err(
        "Git identity not configured. Set name and email in Settings > Git, \
         or configure ~/.gitconfig"
            .to_string(),
    )
}

/// Build a `git2::Signature` from settings, falling back to global git config.
pub fn build_signature(
    settings_name: &str,
    settings_email: &str,
) -> Result<git2::Signature<'static>, String> {
    let (name, email) = resolve_git_identity(settings_name, settings_email)?;
    git2::Signature::now(&name, &email).map_err(|e| format!("Failed to create git signature: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_host ────────────────────────────────────────────

    #[test]
    fn extract_host_https() {
        assert_eq!(
            extract_host("https://github.com/user/repo.git"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn extract_host_with_port() {
        assert_eq!(
            extract_host("https://gitlab.example.com:8443/repo.git"),
            Some("gitlab.example.com".to_string())
        );
    }

    #[test]
    fn extract_host_with_user() {
        assert_eq!(
            extract_host("https://token@github.com/user/repo.git"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn extract_host_ssh_scheme() {
        assert_eq!(
            extract_host("ssh://git@github.com/user/repo.git"),
            Some("github.com".to_string())
        );
    }

    #[test]
    fn extract_host_no_scheme() {
        assert_eq!(extract_host("not-a-url"), None);
    }

    #[test]
    fn extract_host_empty() {
        assert_eq!(extract_host(""), None);
    }

    // ── resolve_git_identity ────────────────────────────────────

    #[test]
    fn identity_from_settings() {
        let (name, email) = resolve_git_identity("Alice", "alice@example.com").unwrap();
        assert_eq!(name, "Alice");
        assert_eq!(email, "alice@example.com");
    }

    #[test]
    fn identity_trims_whitespace() {
        let (name, email) = resolve_git_identity("  Alice  ", "  alice@ex.com  ").unwrap();
        assert_eq!(name, "Alice");
        assert_eq!(email, "alice@ex.com");
    }

    #[test]
    fn identity_empty_settings_may_fallback_or_error() {
        // This test doesn't assert a specific outcome because the result
        // depends on whether ~/.gitconfig has user.name/user.email set.
        let result = resolve_git_identity("", "");
        let _ = result; // just ensure no panic
    }

    // ── build_signature ─────────────────────────────────────────

    #[test]
    fn build_signature_from_settings() {
        let sig = build_signature("Alice", "alice@example.com").unwrap();
        assert_eq!(sig.name(), Some("Alice"));
        assert_eq!(sig.email(), Some("alice@example.com"));
    }

    #[test]
    fn build_signature_empty_returns_error_or_fallback() {
        let result = build_signature("", "");
        let _ = result; // depends on git config; no panic
    }
}
