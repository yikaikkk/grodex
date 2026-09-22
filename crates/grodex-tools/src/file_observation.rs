//! Session-scoped FileObservationStore — issues unforgeable `observation_id`s
//! that write tools verify before authorizing whole-file overwrites.
//!
//! ## Why
//! Before this module, `write_file`/`apply_patch modify` would happily
//! overwrite an existing file even when the model had only seen a partial
//! view of it (a `grep` match, a `read_file` with `offset`/`limit`, or a
//! truncated large file). The model would then "reconstruct" the file from
//! the partial view and silently drop the unseen bytes. The bare
//! `expected_hash` field was not a safe fence because the model could copy
//! it from anywhere — it was not a proof the model had actually read the
//! whole file.
//!
//! ## How
//! - `read_file` records a `FileObservation` in the store when it returns
//!   content, and hands back an opaque `observation_id`. The id is generated
//!   inside the store (a random UUID the model cannot predict or choose).
//! - `apply_patch modify` and `write_file` overwrite of an *existing* file
//!   require an observation whose `coverage == Full`. A `Partial` observation
//!   (offset/range/truncated/paged read) is rejected for whole-file replace
//!   but may still fence a local `edit_file`.
//! - On verify, the store re-reads the file and checks the content_hash
//!   matches the recorded one — this is the real stale fence. The id is
//!   only a capability that says "you saw this file, fully, at this hash".
//!
//! ## Session scoping (P0 batch 1)
//! The store is process-wide via `OnceLock<Arc<FileObservationStore>>`.
//! Session isolation is achieved by the `content_hash` fence — a Full
//! observation from any session only authorizes overwriting a file whose
//! bytes still match, which is safe. A later batch can tighten this by
//! passing a per-session store through `ToolRegistry`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;
use uuid::Uuid;

/// Coverage class: only `Full` observations authorize whole-file overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadCoverage {
    /// The entire file was returned, untruncated.
    Full,
    /// A subset (offset/range/truncated/paged). Useful as a stale fence for
    /// local edits, but does NOT authorize whole-file overwrite.
    Partial,
}

/// A single observation recorded by a `read_file` call.
#[derive(Debug, Clone)]
pub struct FileObservation {
    /// Opaque capability token; assigned by `issue()`.
    pub observation_id: String,
    /// Canonical `fs://{abs_path}` identity.
    pub canonical_resource_id: String,
    /// SHA-256 hex of the full file contents at observation time.
    pub content_hash: String,
    /// Total file size in bytes.
    pub size: u64,
    /// mtime if measurable.
    pub mtime_secs: Option<i64>,
    /// Full vs Partial coverage.
    pub coverage: ReadCoverage,
    /// Wall-clock time the observation was issued.
    pub issued_at: SystemTime,
    /// Optional session id (P0 batch 1: not strictly enforced).
    pub session_id: Option<String>,
}

/// Process-wide store of live observations.
///
/// Cloning is cheap (inner is `Arc`), so multiple tools can share one store.
#[derive(Clone)]
pub struct FileObservationStore {
    inner: Arc<RwLock<HashMap<String, FileObservation>>>,
}

impl Default for FileObservationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FileObservationStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Global default store. Used by tools that were not explicitly handed
    /// a store at construction time (P0 batch 1 compatibility path).
    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<FileObservationStore>> = OnceLock::new();
        GLOBAL.get_or_init(|| Arc::new(Self::new())).clone()
    }

    /// Issue a new observation. Returns the unforgeable `observation_id`
    /// that the model should pass back to write tools. The id is a random
    /// UUID generated inside the store — the model cannot choose or predict
    /// it. Any `observation_id` the caller tried to set on `obs` is ignored.
    pub fn issue(&self, mut obs: FileObservation) -> String {
        let id = format!("obs_{}", Uuid::new_v4().simple());
        obs.observation_id = id.clone();
        let mut guard = self.inner.write().expect("observation lock poisoned");
        guard.insert(id.clone(), obs);
        id
    }

    /// Verify an `observation_id` authorizes a whole-file overwrite of
    /// `path`. Requirements:
    ///   - the id exists in the store;
    ///   - canonical path matches the recorded one;
    ///   - `coverage == Full`;
    ///   - the current on-disk content hash matches the recorded one.
    /// On success the observation is consumed (single-use) so a stale id
    /// cannot be replayed.
    pub fn verify_full(&self, observation_id: &str, path: &Path) -> Result<FileObservation, VerifyError> {
        let obs = {
            let guard = self.inner.read().expect("observation lock poisoned");
            guard.get(observation_id).cloned().ok_or(VerifyError::UnknownObservation)?
        };
        let canonical = crate::fsutil::canonicalize(path);
        let expected = format!("fs://{}", canonical.display());
        if obs.canonical_resource_id != expected {
            return Err(VerifyError::PathMismatch);
        }
        if obs.coverage != ReadCoverage::Full {
            return Err(VerifyError::PartialCoverage);
        }
        let current = std::fs::read(path).map_err(|e| VerifyError::ReadFailed(e.to_string()))?;
        let current_hash = sha256_hex(&current);
        if current_hash != obs.content_hash {
            return Err(VerifyError::StaleFile);
        }
        // Single-use: consume on success so the id can't be replayed after
        // the file changes.
        {
            let mut guard = self.inner.write().expect("observation lock poisoned");
            guard.remove(observation_id);
        }
        Ok(obs)
    }

    /// Verify an `observation_id` is valid as a stale fence for a *local*
    /// edit (no whole-file replace). Allows `Full` OR `Partial` coverage,
    /// but still requires the current content hash to match. Used by
    /// `edit_file`.
    pub fn verify_partial(&self, observation_id: &str, path: &Path) -> Result<FileObservation, VerifyError> {
        let obs = {
            let guard = self.inner.read().expect("observation lock poisoned");
            guard.get(observation_id).cloned().ok_or(VerifyError::UnknownObservation)?
        };
        let canonical = crate::fsutil::canonicalize(path);
        let expected = format!("fs://{}", canonical.display());
        if obs.canonical_resource_id != expected {
            return Err(VerifyError::PathMismatch);
        }
        let current = std::fs::read(path).map_err(|e| VerifyError::ReadFailed(e.to_string()))?;
        let current_hash = sha256_hex(&current);
        if current_hash != obs.content_hash {
            return Err(VerifyError::StaleFile);
        }
        Ok(obs)
    }

    /// Invalidate all observations for a given canonical resource id.
    /// Called after a successful write so subsequent overwrite attempts must
    /// re-read.
    pub fn invalidate_path(&self, canonical_resource_id: &str) {
        let mut guard = self.inner.write().expect("observation lock poisoned");
        guard.retain(|_, obs| obs.canonical_resource_id != canonical_resource_id);
    }

    /// Number of observations currently held (diagnostics/testing).
    pub fn len(&self) -> usize {
        self.inner.read().expect("observation lock poisoned").len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().expect("observation lock poisoned").is_empty()
    }
}

/// Verification failure reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The `observation_id` is not known to the store.
    UnknownObservation,
    /// The observation was for a different path.
    PathMismatch,
    /// The observation was partial (offset/range/truncated) — not enough to
    /// authorize whole-file overwrite.
    PartialCoverage,
    /// The file changed on disk since the observation was issued.
    StaleFile,
    /// Could not read the current file to verify.
    ReadFailed(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownObservation => write!(
                f,
                "unknown observation_id — call read_file on the whole file first"
            ),
            Self::PathMismatch => write!(f, "observation was for a different path"),
            Self::PartialCoverage => write!(
                f,
                "observation was partial (offset/range/truncated) — re-read the whole file before overwriting"
            ),
            Self::StaleFile => write!(
                f,
                "file changed since the observation — re-read before writing"
            ),
            Self::ReadFailed(e) => write!(f, "failed to read current file: {e}"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Compute SHA-256 hex of a byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_obs(rid: &str, hash: &str, coverage: ReadCoverage) -> FileObservation {
        FileObservation {
            observation_id: String::new(),
            canonical_resource_id: rid.into(),
            content_hash: hash.into(),
            size: 0,
            mtime_secs: None,
            coverage,
            issued_at: SystemTime::now(),
            session_id: None,
        }
    }

    #[test]
    fn full_observation_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello").unwrap();
        let rid = format!("fs://{}", crate::fsutil::canonicalize(&path).display());
        let store = FileObservationStore::new();
        let id = store.issue(make_obs(&rid, &sha256_hex(b"hello"), ReadCoverage::Full));
        assert!(store.verify_full(&id, &path).is_ok());
        // Single-use: a second verify with the same id fails (consumed).
        assert_eq!(
            store.verify_full(&id, &path).unwrap_err(),
            VerifyError::UnknownObservation
        );
    }

    #[test]
    fn partial_rejected_for_full_ok_for_partial() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.txt");
        std::fs::write(&path, "hello").unwrap();
        let rid = format!("fs://{}", crate::fsutil::canonicalize(&path).display());
        let store = FileObservationStore::new();
        let id = store.issue(make_obs(&rid, &sha256_hex(b"hello"), ReadCoverage::Partial));
        assert_eq!(
            store.verify_full(&id, &path).unwrap_err(),
            VerifyError::PartialCoverage
        );
        // Partial verify still works (local edit fence).
        assert!(store.verify_partial(&id, &path).is_ok());
    }

    #[test]
    fn stale_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.txt");
        std::fs::write(&path, "hello").unwrap();
        let rid = format!("fs://{}", crate::fsutil::canonicalize(&path).display());
        let store = FileObservationStore::new();
        let id = store.issue(make_obs(&rid, &sha256_hex(b"hello"), ReadCoverage::Full));
        std::fs::write(&path, "world").unwrap();
        assert_eq!(
            store.verify_full(&id, &path).unwrap_err(),
            VerifyError::StaleFile
        );
    }

    #[test]
    fn unknown_id_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.txt");
        std::fs::write(&path, "hello").unwrap();
        let store = FileObservationStore::new();
        assert_eq!(
            store.verify_full("obs_nonexistent", &path).unwrap_err(),
            VerifyError::UnknownObservation
        );
    }

    #[test]
    fn path_mismatch_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "hello").unwrap();
        std::fs::write(&b, "hello").unwrap();
        let rid_a = format!("fs://{}", crate::fsutil::canonicalize(&a).display());
        let store = FileObservationStore::new();
        let id = store.issue(make_obs(&rid_a, &sha256_hex(b"hello"), ReadCoverage::Full));
        assert_eq!(
            store.verify_full(&id, &b).unwrap_err(),
            VerifyError::PathMismatch
        );
    }

    #[test]
    fn invalidate_path_clears_observations() {
        let store = FileObservationStore::new();
        let rid = "fs:///tmp/x".into();
        let id = store.issue(make_obs(rid, "h", ReadCoverage::Full));
        assert_eq!(store.len(), 1);
        store.invalidate_path(rid);
        assert!(store.is_empty());
        // id no longer usable
        let _ = id;
    }
}
