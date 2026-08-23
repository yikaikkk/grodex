//! Secret Store — durable storage for master credentials.
//!
//! The [`CredentialBroker`](crate::lease::CredentialBroker) previously held
//! master tokens in a `HashMap<String, String>` in memory, so a process
//! restart lost every token. This module defines a [`SecretStore`] trait that
//! offloads secret material to a durable backend so tokens survive restarts.
//!
//! Design decision: credentials are **file-hosted, not OS-keychain-hosted**.
//! Grodex intentionally never reads the system keychain; the user-level
//! config file (`~/.grodex/credentials.json`, mode 0600) is the single
//! durable backend on every platform.
//!
//! Two implementations ship here:
//! - [`InMemorySecretStore`] — for tests and as a fail-soft fallback.
//! - [`FileSecretStore`] — JSON file with 0600 permissions, atomic writes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Errors returned by [`SecretStore`] backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretStoreError {
    /// The requested secret does not exist in the store.
    NotFound,
    /// The caller is not permitted to read/write this secret.
    AccessDenied,
    /// The OS secret backend is missing or unreachable (kept for trait
    /// compatibility; the file backend does not produce this variant).
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

/// A durable key/value store for secret material.
///
/// Keys are opaque strings; values are secret strings (e.g. master tokens).
/// Implementations MUST NOT log the value anywhere.
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
/// when no durable backend is available. Secrets do NOT survive a process
/// restart.
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

// ── FileSecretStore ─────────────────────────────────────────────────

/// File-hosted `SecretStore`：凭证以 JSON 落盘（默认
/// `~/.grodex/credentials.json`），重启存活且**不触碰系统钥匙串**。
/// 文件权限 0600（仅当前用户可读写），写入走“临时文件 + rename”原子
/// 路径，崩溃不会留下半截文件。
///
/// 文件格式：`{ "master:<provider>": "<token>", ... }`（键名由
/// [`CredentialBroker::persist_provider`](crate::lease::CredentialBroker::persist_provider)
/// 约定）。
#[derive(Debug)]
pub struct FileSecretStore {
    path: PathBuf,
    /// Serializes read-modify-write cycles within this process.
    io: Mutex<()>,
}

impl FileSecretStore {
    /// Create a store persisting to `path` (created lazily on first write).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            io: Mutex::new(()),
        }
    }

    fn load(&self) -> Result<HashMap<String, String>, SecretStoreError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(e) => return Err(SecretStoreError::IoError(e.to_string())),
        };
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw)
            .map_err(|e| SecretStoreError::IoError(format!("credentials file corrupted: {e}")))?;
        Ok(map
            .into_iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
            .collect())
    }

    fn save(&self, secrets: &HashMap<String, String>) -> Result<(), SecretStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| SecretStoreError::IoError(e.to_string()))?;
        }
        let json = serde_json::to_string_pretty(secrets)
            .map_err(|e| SecretStoreError::IoError(e.to_string()))?;
        // 原子写：先写临时文件、收紧权限，再 rename 到位。
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| SecretStoreError::IoError(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| SecretStoreError::IoError(e.to_string()))?;
        }
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| SecretStoreError::IoError(e.to_string()))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SecretStore for FileSecretStore {
    async fn store(&self, key: &str, value: &str) -> Result<(), SecretStoreError> {
        let _guard = self.io.lock().expect("FileSecretStore mutex poisoned");
        let mut secrets = self.load()?;
        secrets.insert(key.to_string(), value.to_string());
        self.save(&secrets)
    }

    async fn retrieve(&self, key: &str) -> Result<Option<String>, SecretStoreError> {
        let _guard = self.io.lock().expect("FileSecretStore mutex poisoned");
        Ok(self.load()?.remove(key))
    }

    async fn delete(&self, key: &str) -> Result<(), SecretStoreError> {
        let _guard = self.io.lock().expect("FileSecretStore mutex poisoned");
        let mut secrets = self.load()?;
        if secrets.remove(key).is_none() {
            return Err(SecretStoreError::NotFound);
        }
        self.save(&secrets)
    }
}

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

    /// Unique temp file per test run — avoids cross-talk between tests and
    /// prior runs without pulling in a tempfile dependency.
    fn tmp_credentials_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "grodex-auth-test-{}-{}-{}.json",
            tag,
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[tokio::test]
    async fn file_store_retrieve_delete_roundtrip() {
        let path = tmp_credentials_path("roundtrip");
        let store = FileSecretStore::new(&path);

        // Missing file reads as empty (no error).
        assert_eq!(store.retrieve("master:openai").await.unwrap(), None);

        // Store → retrieve round-trip.
        store.store("master:openai", "sk-secret-1").await.unwrap();
        assert_eq!(
            store.retrieve("master:openai").await.unwrap(),
            Some("sk-secret-1".to_string())
        );

        // Overwrite.
        store.store("master:openai", "sk-secret-2").await.unwrap();
        assert_eq!(
            store.retrieve("master:openai").await.unwrap(),
            Some("sk-secret-2".to_string())
        );

        // Keys are isolated.
        store.store("master:anthropic", "sk-ant").await.unwrap();
        store.delete("master:openai").await.unwrap();
        assert_eq!(store.retrieve("master:openai").await.unwrap(), None);
        assert_eq!(
            store.retrieve("master:anthropic").await.unwrap(),
            Some("sk-ant".to_string())
        );

        // Deleting a missing key yields NotFound.
        let err = store.delete("master:openai").await.unwrap_err();
        assert_eq!(err, SecretStoreError::NotFound);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn file_store_survives_reopen() {
        // Durability across "restarts": a fresh store instance pointing at
        // the same path sees the previously persisted secret.
        let path = tmp_credentials_path("reopen");
        FileSecretStore::new(&path)
            .store("master:openai", "sk-durable")
            .await
            .unwrap();
        let reopened = FileSecretStore::new(&path);
        assert_eq!(
            reopened.retrieve("master:openai").await.unwrap(),
            Some("sk-durable".to_string())
        );
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_store_writes_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_credentials_path("perm");
        FileSecretStore::new(&path)
            .store("master:openai", "sk-perm")
            .await
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be owner-only");
        let _ = std::fs::remove_file(&path);
    }
}
