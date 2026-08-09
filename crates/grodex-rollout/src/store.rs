//! RolloutStore trait and FileRolloutStore implementation.
//!
//! FileRolloutStore writes `rollout.jsonl` and blobs to disk under
//! `~/.grodex/sessions/{session_id}/`.

use crate::event::RolloutEvent;
use grodex_core::error::GrodexError;
use std::io::Write;
use std::path::{Path, PathBuf};
// ── Trait ──────────────────────────────────────────────────────────

/// Storage backend for rollout events and binary blobs.
#[async_trait::async_trait]
pub trait RolloutStore: Send + Sync + 'static {
    async fn append_event(&self, event: RolloutEvent) -> Result<u64, GrodexError>;
    async fn write_blob_async(&self, content: &[u8], mime_type: &str) -> Result<BlobRef, GrodexError>;
    async fn read_blob_async(&self, blob_id: &BlobId) -> Result<Vec<u8>, GrodexError>;
    async fn replay_from(&self, seq: u64) -> Result<Vec<RolloutEvent>, GrodexError>;
}

// ── Types ──────────────────────────────────────────────────────────

/// Blob reference and storage types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobId(pub String);

impl BlobId {
    pub fn new(hash: impl Into<String>) -> Self {
        Self(hash.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct BlobRef {
    pub blob_id: BlobId,
    pub size_bytes: u64,
    pub mime_type: String,
    pub preview: String,
}

/// File-backed implementation of the RolloutStore trait.
///
/// Each session gets a directory under `~/.grodex/sessions/{id}/`:
///   - `rollout.jsonl` — append-only event log
///   - `blobs/` — content-addressed binary artifacts
pub struct FileRolloutStore {
    #[allow(dead_code)]
    session_dir: PathBuf,
    jsonl_path: PathBuf,
    blobs_dir: PathBuf,
}

impl FileRolloutStore {
    /// Create a new file store for the given session id.
    pub fn new(base_dir: &Path, session_id: &str) -> Result<Self, GrodexError> {
        let session_dir = base_dir.join(session_id);
        let blobs_dir = session_dir.join("blobs");
        let jsonl_path = session_dir.join("rollout.jsonl");

        std::fs::create_dir_all(&blobs_dir)
            .map_err(|e| GrodexError::Internal(anyhow::anyhow!("cannot create session dir: {e}")))?;

        Ok(Self {
            session_dir,
            jsonl_path,
            blobs_dir,
        })
    }

    /// Default store location: `~/.grodex/sessions/`
    pub fn default_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grodex")
            .join("sessions")
    }

    /// Append one event to the journal.
    ///
    /// The event's `seq` field (assigned by `RolloutWriter::next_seq()`) is
    /// the SINGLE source of truth for sequence numbers. The store does NOT
    /// maintain its own counter — it returns `event.seq` directly so the
    /// writer's counter and the on-disk seq stay in sync.
    pub fn append_event(&self, event: RolloutEvent) -> Result<u64, GrodexError> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.jsonl_path)
            .map_err(|e| GrodexError::Internal(anyhow::anyhow!("cannot open rollout: {e}")))?;

        let line = serde_json::to_string(&event)
            .map_err(|e| GrodexError::Internal(anyhow::anyhow!("serialize event: {e}")))?;

        writeln!(file, "{line}").map_err(|e| GrodexError::Internal(anyhow::anyhow!("write event: {e}")))?;

        // Return the event's own seq — no separate counter.
        Ok(event.seq)
    }

    /// Store binary content and return a reference.
    pub fn write_blob(&self, content: &[u8], mime_type: &str) -> Result<BlobRef, GrodexError> {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(content);
        let hash = format!("{:x}", hasher.finalize());
        let blob_id = BlobId::new(&hash);

        let blob_path = self.blobs_dir.join(&hash);
        std::fs::write(&blob_path, content).map_err(|e| GrodexError::Internal(anyhow::anyhow!("write blob: {e}")))?;

        let preview = String::from_utf8_lossy(&content[..content.len().min(200)]).to_string();

        Ok(BlobRef {
            blob_id,
            size_bytes: content.len() as u64,
            mime_type: mime_type.to_string(),
            preview,
        })
    }

    /// Read a blob by id.
    pub fn read_blob(&self, blob_id: &BlobId) -> Result<Vec<u8>, GrodexError> {
        let path = self.blobs_dir.join(blob_id.as_str());
        std::fs::read(&path).map_err(|e| GrodexError::Internal(anyhow::anyhow!("read blob: {e}")))
    }

    /// Replay events from the given sequence number.
    pub fn replay_from(&self, from_seq: u64) -> Result<Vec<RolloutEvent>, GrodexError> {
        if !self.jsonl_path.exists() {
            return Ok(Vec::new());
        }

        let content = std::fs::read_to_string(&self.jsonl_path).unwrap_or_default();
        let mut events = Vec::new();

        for (i, line) in content.lines().enumerate() {
            if (i as u64) >= from_seq {
                if let Ok(event) = serde_json::from_str::<RolloutEvent>(line) {
                    events.push(event);
                }
            }
        }

        Ok(events)
    }
}

#[async_trait::async_trait]
impl RolloutStore for FileRolloutStore {
    async fn append_event(&self, event: RolloutEvent) -> Result<u64, GrodexError> {
        self.append_event(event)
    }

    async fn write_blob_async(&self, content: &[u8], mime_type: &str) -> Result<BlobRef, GrodexError> {
        self.write_blob(content, mime_type)
    }

    async fn read_blob_async(&self, blob_id: &BlobId) -> Result<Vec<u8>, GrodexError> {
        self.read_blob(blob_id)
    }

    async fn replay_from(&self, seq: u64) -> Result<Vec<RolloutEvent>, GrodexError> {
        self.replay_from(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{RolloutEventType, SensitivityLevel};
    use grodex_core::id::SessionId;

    #[test]
    fn append_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionId::new().to_string();
        let store = FileRolloutStore::new(dir.path(), &session_id).unwrap();

        let event = RolloutEvent {
            schema_version: 2,
            seq: 0,
            session_id: SessionId::new(),
            turn_id: None,
            step_id: None,
            generation: None,
            timestamp: chrono::Utc::now(),
            event_type: RolloutEventType::RuntimeStateChanged,
            payload: serde_json::json!({"state": "idle"}),
            sensitivity: SensitivityLevel::Normal,
        };

        let seq = store.append_event(event).unwrap();
        assert_eq!(seq, 0);

        let events = store.replay_from(0).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn blob_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRolloutStore::new(dir.path(), "test-blobs").unwrap();

        let blob = store.write_blob(b"hello world", "text/plain").unwrap();
        assert_eq!(blob.size_bytes, 11);

        let data = store.read_blob(&blob.blob_id).unwrap();
        assert_eq!(data, b"hello world");
    }
}
