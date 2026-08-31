//! Platform credential management via the OS keyring.
//!
//! Wraps the `keyring` crate to store / retrieve / delete API keys
//! for Nexus Mods and mod.io in the OS credential manager.

use keyring::Entry;

use super::errors::PlatformError;

/// Service name used for all platform credentials in the OS keyring.
const SERVICE_NAME: &str = "bg3-cmty-studio";

/// Maximum characters per keyring entry on Windows.
/// Windows Credential Manager limits passwords to 2560 bytes (UTF-16),
/// which is ~1280 characters. We use 1200 to leave a safety margin.
const CHUNK_MAX_CHARS: usize = 1200;

/// Store a credential in the OS keyring.
///
/// If the value exceeds the platform limit, it is automatically split
/// across multiple keyring entries (`key`, `key:1`, `key:2`, …).
pub fn store_credential(service: &str, username: &str, value: &str) -> Result<(), PlatformError> {
    let key = format!("{service}:{username}");

    if value.len() <= CHUNK_MAX_CHARS {
        // Fast path: fits in a single entry — clear any old chunks first.
        delete_chunks(&key);
        let entry = Entry::new(SERVICE_NAME, &key).map_err(|e| {
            PlatformError::KeyringError(format!("Failed to create keyring entry: {e}"))
        })?;
        entry
            .set_password(value)
            .map_err(|e| PlatformError::KeyringError(format!("Failed to store credential: {e}")))?;
        return Ok(());
    }

    // Chunked path: split into CHUNK_MAX_CHARS-sized pieces.
    let chunks: Vec<&str> = value
        .as_bytes()
        .chunks(CHUNK_MAX_CHARS)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();

    // Store the chunk count in the base entry so we know how to reassemble.
    let base_entry = Entry::new(SERVICE_NAME, &key)
        .map_err(|e| PlatformError::KeyringError(format!("Failed to create keyring entry: {e}")))?;
    base_entry
        .set_password(&format!("__chunked:{}", chunks.len()))
        .map_err(|e| {
            PlatformError::KeyringError(format!("Failed to store credential header: {e}"))
        })?;

    for (i, chunk) in chunks.iter().enumerate() {
        let chunk_key = format!("{key}:{i}");
        let entry = Entry::new(SERVICE_NAME, &chunk_key).map_err(|e| {
            PlatformError::KeyringError(format!(
                "Failed to create keyring entry for chunk {i}: {e}"
            ))
        })?;
        entry.set_password(chunk).map_err(|e| {
            PlatformError::KeyringError(format!("Failed to store credential chunk {i}: {e}"))
        })?;
    }

    Ok(())
}

/// Retrieve a credential from the OS keyring.
///
/// Transparently reassembles chunked credentials.
/// Returns `Ok(None)` when no entry exists for the given service/username pair.
pub fn get_credential(service: &str, username: &str) -> Result<Option<String>, PlatformError> {
    let key = format!("{service}:{username}");
    let entry = Entry::new(SERVICE_NAME, &key)
        .map_err(|e| PlatformError::KeyringError(format!("Failed to create keyring entry: {e}")))?;
    let base_value = match entry.get_password() {
        Ok(value) => value,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(e) => {
            return Err(PlatformError::KeyringError(format!(
                "Failed to read credential: {e}"
            )))
        }
    };

    // Check if this is a chunked credential.
    if let Some(count_str) = base_value.strip_prefix("__chunked:") {
        let count: usize = count_str.parse().map_err(|_| {
            PlatformError::KeyringError("Corrupted chunked credential header".into())
        })?;
        let mut assembled = String::new();
        for i in 0..count {
            let chunk_key = format!("{key}:{i}");
            let chunk_entry = Entry::new(SERVICE_NAME, &chunk_key).map_err(|e| {
                PlatformError::KeyringError(format!("Failed to open chunk {i}: {e}"))
            })?;
            let chunk = chunk_entry.get_password().map_err(|e| {
                PlatformError::KeyringError(format!("Failed to read chunk {i}: {e}"))
            })?;
            assembled.push_str(&chunk);
        }
        return Ok(Some(assembled));
    }

    Ok(Some(base_value))
}

/// Delete a credential from the OS keyring.
///
/// Silently succeeds if the credential does not exist.
/// Also cleans up any chunked entries.
pub fn delete_credential(service: &str, username: &str) -> Result<(), PlatformError> {
    let key = format!("{service}:{username}");

    // Check if chunked and clean up chunk entries first.
    let entry = Entry::new(SERVICE_NAME, &key)
        .map_err(|e| PlatformError::KeyringError(format!("Failed to create keyring entry: {e}")))?;
    if let Ok(val) = entry.get_password() {
        if let Some(count_str) = val.strip_prefix("__chunked:") {
            if let Ok(count) = count_str.parse::<usize>() {
                for i in 0..count {
                    let chunk_key = format!("{key}:{i}");
                    if let Ok(ce) = Entry::new(SERVICE_NAME, &chunk_key) {
                        let _ = ce.delete_credential();
                    }
                }
            }
        }
    }

    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(PlatformError::KeyringError(format!(
            "Failed to delete credential: {e}"
        ))),
    }
}

/// Delete any leftover chunk entries for a key (used when overwriting).
fn delete_chunks(key: &str) {
    if let Ok(entry) = Entry::new(SERVICE_NAME, key) {
        if let Ok(val) = entry.get_password() {
            if let Some(count_str) = val.strip_prefix("__chunked:") {
                if let Ok(count) = count_str.parse::<usize>() {
                    for i in 0..count {
                        let chunk_key = format!("{key}:{i}");
                        if let Ok(ce) = Entry::new(SERVICE_NAME, &chunk_key) {
                            let _ = ce.delete_credential();
                        }
                    }
                }
            }
        }
    }
}

/// Read a credential from the legacy "cmtystudio" keyring service.
/// Used during one-time migration only.
pub fn get_legacy_credential(key: &str) -> Result<Option<String>, PlatformError> {
    const LEGACY_SERVICE: &str = "cmtystudio";
    let entry = Entry::new(LEGACY_SERVICE, key)
        .map_err(|e| PlatformError::KeyringError(format!("Legacy keyring init error: {e}")))?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(PlatformError::KeyringError(format!(
            "Failed to read legacy credential: {e}"
        ))),
    }
}

/// Delete a credential from the legacy "cmtystudio" keyring service.
/// Used during one-time migration only.
pub fn delete_legacy_credential(key: &str) -> Result<(), PlatformError> {
    const LEGACY_SERVICE: &str = "cmtystudio";
    let entry = Entry::new(LEGACY_SERVICE, key)
        .map_err(|e| PlatformError::KeyringError(format!("Legacy keyring init error: {e}")))?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(PlatformError::KeyringError(format!(
            "Failed to delete legacy credential: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: These tests interact with the real OS keyring.
    // They use a unique username to avoid collisions.
    const TEST_USERNAME: &str = "cmty-studio-test-credential";

    /// Returns `true` when the OS keyring backend is available.
    /// On Linux CI there is typically no `org.freedesktop.secrets` provider,
    /// so we skip tests gracefully instead of failing.
    fn keyring_available() -> bool {
        match Entry::new(SERVICE_NAME, "cmty-studio-probe") {
            Ok(entry) => {
                // Attempt a read — NoEntry is fine, any platform error means
                // the backend is not wired up.
                match entry.get_password() {
                    Ok(_) | Err(keyring::Error::NoEntry) => true,
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    }

    macro_rules! skip_without_keyring {
        () => {
            if !keyring_available() {
                eprintln!("SKIPPED: OS keyring backend not available (e.g. no org.freedesktop.secrets on Linux CI)");
                return;
            }
        };
    }

    #[test]
    fn round_trip_store_get_delete() {
        skip_without_keyring!();

        let service = "platform-test";
        let secret = "test-api-key-12345";

        // Store
        store_credential(service, TEST_USERNAME, secret).expect("store should succeed");

        // Get
        let retrieved = get_credential(service, TEST_USERNAME).expect("get should succeed");
        assert_eq!(retrieved, Some(secret.to_string()));

        // Delete
        delete_credential(service, TEST_USERNAME).expect("delete should succeed");

        // Verify deleted
        let after_delete =
            get_credential(service, TEST_USERNAME).expect("get after delete should succeed");
        assert_eq!(after_delete, None);
    }

    #[test]
    fn get_nonexistent_returns_none() {
        skip_without_keyring!();

        let result =
            get_credential("platform-test", "nonexistent-key-xyz").expect("should not error");
        assert_eq!(result, None);
    }

    #[test]
    fn delete_nonexistent_succeeds() {
        skip_without_keyring!();

        delete_credential("platform-test", "nonexistent-key-xyz")
            .expect("delete nonexistent should succeed");
    }
}
