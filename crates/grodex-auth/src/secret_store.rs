//! OS Secret Store — durable, OS-backed storage for master credentials.
//!
//! The [`CredentialBroker`](crate::lease::CredentialBroker) previously held
//! master tokens in a `HashMap<String, String>` in memory, so a process
//! restart lost every token. This module defines a [`SecretStore`] trait that
//! offloads secret material to the operating system's native credential
//! vault (macOS Keychain / Windows CredMan / Linux Secret Service), so tokens
//! survive restarts and are never written to plaintext config.
//!
//! Two implementations ship here:
//! - [`InMemorySecretStore`] — for tests and as a fail-soft fallback.
//! - [`MacOSKeychainStore`] — shells out to the `security` CLI (macOS only).

use std::collections::HashMap;
use std::sync::Mutex;

/// Errors returned by [`SecretStore`] backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretStoreError {
    /// The requested secret does not exist in the store.
    NotFound,
    /// The caller is not permitted to read/write this secret.
    AccessDenied,
    /// The OS secret backend is missing or unreachable (e.g. the `security`
    /// binary is absent, or the D-Bus secret service is not running).
    BackendUnavailable,
    /// An I/O or backend-internal failure with a descriptive message.
    IoError(String),
}

impl std::fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "secret not found"),
            Self::AccessDenied => write!(f, "access denied by secret store"),
            Self::BackendUnavailable => write!(f, "secret store backend unavailable"),
            Self::IoError(msg) => write!(f, "secret store I/O error: {msg}"),
        }
    }
}

impl std::error::Error for SecretStoreError {}

/// A durable, OS-backed key/value store for secret material.
///
/// Keys are opaque strings; values are secret strings (e.g. master tokens).
/// Implementations MUST NOT log or persist the value outside the OS vault.
#[async_trait::async_trait]
pub trait SecretStore: Send + Sync {
    /// Persist `value` under `key`. Overwrites any existing value.
    async fn store(&self, key: &str, value: &str) -> Result<(), SecretStoreError>;
    /// Retrieve the secret for `key`. Returns `Ok(None)` if absent.
    async fn retrieve(&self, key: &str) -> Result<Option<String>, SecretStoreError>;
    /// Delete the secret for `key`. Returns `NotFound` if absent.
    async fn delete(&self, key: &str) -> Result<(), SecretStoreError>;
}

// ── InMemorySecretStore ─────────────────────────────────────────────

/// Trivial in-memory `SecretStore` backed by a `HashMap` behind a
/// [`std::sync::Mutex`]. Intended for tests and as a fail-soft fallback
/// when no OS backend is available. Secrets do NOT survive a process restart.
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    secrets: Mutex<HashMap<String, String>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SecretStore for InMemorySecretStore {
    async fn store(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        self.secrets
            .lock()
            .expect("InMemorySecretStore mutex poisoned")
            .insert(key.to_string(), value.to_string());
        Ok(())
    }

    async fn retrieve(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        Ok(self
            .secrets
            .lock()
            .expect("InMemorySecretStore mutex poisoned")
            .get(key)
            .cloned())
    }

    async fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        match self
            .secrets
            .lock()
            .expect("InMemorySecretStore mutex poisoned")
            .remove(key)
        {
            Some(_) => Ok(()),
            None => Err(SecretStoreError::NotFound),
        }
    }
}

// ── MacOSKeychainStore ──────────────────────────────────────────────

/// macOS Keychain-backed `SecretStore`. Shells out to the system `security`
/// CLI, storing each secret as a generic-password item with a fixed service
/// name (`grodex` by default) and the caller-supplied key as the account.
///
/// Only compiled on `target_os = "macos"`.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct MacOSKeychainStore {
    /// Keychain service name — prefixed with `grodex` to avoid collisions
    /// with other applications' keychain items.
    service: String,
}

#[cfg(target_os = "macos")]
impl MacOSKeychainStore {
    /// Create a store using the default service name `grodex`.
    pub fn new() -> Self {
        Self {
            service: "grodex".to_string(),
        }
    }

    /// Create a store with a custom service name (e.g. for tests that must
    /// not collide with production items).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// macOS `security` exit code for `errSecItemNotFound`.
    const EXIT_ITEM_NOT_FOUND: i32 = 44;
}

#[cfg(target_os = "macos")]
impl Default for MacOSKeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl SecretStore for MacOSKeychainStore {
    async fn store(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        let output = Command::new("security")
            .args([
                "add-generic-password",
                "-s",
                &self.service,
                "-a",
                key,
                "-w",
                value,
                "-U",
            ])
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SecretStoreError::BackendUnavailable
                } else {
                    SecretStoreError::IoError(e.to_string())
                }
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output.status.code().unwrap_or(-1);
            // Exit 45 is errSecDuplicateItem — shouldn't happen with -U, but
            // treat as a benign conflict rather than a hard failure.
            if code == 45 {
                return Ok(());
            }
            return Err(SecretStoreError::IoError(format!(
                "security add-generic-password failed (exit {code}): {stderr}"
            )));
        }
        Ok(())
    }

    async fn retrieve(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        let output = Command::new("security")
            .args([
                "find-generic-password",
                "-s",
                &self.service,
                "-a",
                key,
                "-w",
            ])
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SecretStoreError::BackendUnavailable
                } else {
                    SecretStoreError::IoError(e.to_string())
                }
            })?;

        let code = output.status.code().unwrap_or(-1);
        if code == Self::EXIT_ITEM_NOT_FOUND {
            return Ok(None);
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SecretStoreError::IoError(format!(
                "security find-generic-password failed (exit {code}): {stderr}"
            )));
        }
        // `-w` prints the password to stdout with a trailing newline.
        let value = String::from_utf8_lossy(&output.stdout);
        let value = value.trim_end_matches('\n');
        Ok(Some(value.to_string()))
    }

    async fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        let output = Command::new("security")
            .args([
                "delete-generic-password",
                "-s",
                &self.service,
                "-a",
                key,
            ])
            .output()
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    SecretStoreError::BackendUnavailable
                } else {
                    SecretStoreError::IoError(e.to_string())
                }
            })?;

        let code = output.status.code().unwrap_or(-1);
        if code == Self::EXIT_ITEM_NOT_FOUND {
            return Err(SecretStoreError::NotFound);
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(SecretStoreError::IoError(format!(
                "security delete-generic-password failed (exit {code}): {stderr}"
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
use tokio::process::Command;

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_memory_store_retrieve_delete_roundtrip() {
        let store = InMemorySecretStore::new();

        // Absent key retrieves as None.
        assert_eq!(store.retrieve("missing").await.unwrap(), None);

        // Store then retrieve.
        store.store("openai", "sk-master-secret").await.unwrap();
        assert_eq!(
            store.retrieve("openai").await.unwrap(),
            Some("sk-master-secret".to_string())
        );

        // Overwrite via store.
        store.store("openai", "sk-rotated").await.unwrap();
        assert_eq!(
            store.retrieve("openai").await.unwrap(),
            Some("sk-rotated".to_string())
        );

        // Delete then confirm it's gone.
        store.delete("openai").await.unwrap();
        assert_eq!(store.retrieve("openai").await.unwrap(), None);

        // Deleting a missing key yields NotFound.
        let err = store.delete("openai").await.unwrap_err();
        assert_eq!(err, SecretStoreError::NotFound);
    }

    #[tokio::test]
    async fn in_memory_store_isolates_keys() {
        let store = InMemorySecretStore::new();
        store.store("a", "alpha").await.unwrap();
        store.store("b", "beta").await.unwrap();
        assert_eq!(store.retrieve("a").await.unwrap(), Some("alpha".to_string()));
        assert_eq!(store.retrieve("b").await.unwrap(), Some("beta".to_string()));
        // Removing one does not touch the other.
        store.delete("a").await.unwrap();
        assert_eq!(store.retrieve("a").await.unwrap(), None);
        assert_eq!(store.retrieve("b").await.unwrap(), Some("beta".to_string()));
    }

    /// Real integration test: actually calls the macOS `security` CLI against
    /// the user's login keychain. Gated to `target_os = "macos"` only.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_keychain_store_retrieve_delete_roundtrip() {
        // Use a dedicated service name so we never collide with production
        // items, and a unique key suffix to avoid cross-talk between test
        // runs on shared machines.
        let store = MacOSKeychainStore::with_service("grodex-test");
        let key = "roundtrip-key";
        let value = "sk-test-secret-12345";

        // Clean up any leftover from a prior run before asserting.
        let _ = store.delete(key).await;

        // Store the secret.
        store.store(key, value).await.expect("store should succeed");

        // Retrieve must round-trip the exact value.
        let got = store.retrieve(key).await.expect("retrieve should succeed");
        assert_eq!(got.as_deref(), Some(value));

        // Delete cleans it up.
        store.delete(key).await.expect("delete should succeed");

        // Now retrieve returns None.
        let after = store.retrieve(key).await.expect("retrieve after delete");
        assert_eq!(after, None);

        // Deleting a missing key yields NotFound.
        let err = store.delete(key).await.unwrap_err();
        assert_eq!(err, SecretStoreError::NotFound);
    }
}
