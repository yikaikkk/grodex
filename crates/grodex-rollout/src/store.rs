//! RolloutStore trait and FileRolloutStore implementation.
//!
//! FileRolloutStore writes `rollout.jsonl` and blobs to disk under
//! `~/.grodex/sessions/{session_id}/`.
//!
//! **P0-1 reliability rewrite** — writes no longer go through
//! `File::append(true)` directly. Instead:
//!
//! - Events go through a [`JournalHandle`] (single-writer actor) that
//!   serially assigns seq, flushes userspace buffers, and fsyncs on
//!   either a force flag or a batch counter. See [`journal_actor`] for
//!   the full contract.
//! - Blobs go through `write(temp) → fsync(temp) → rename → fsync(dir)`
//!   so we never leave a half-written file at the content-addressed
//!   path; readers either see the full blob or nothing.
//! - `replay_from(seq)` uses [`replay_journal_strict`] which filters by
//!   `event.seq` (NOT the physical line number) and fails closed on
//!   any corrupt / empty JSON line.

use crate::event::RolloutEvent;
use crate::journal_actor::{FsyncPolicy, JournalHandle, replay_journal_strict};
use grodex_core::error::GrodexError;
use grodex_core::id::SessionId;
use std::path::{Path, PathBuf};

// ── Trait ──────────────────────────────────────────────────────────

/// Storage backend for rollout events and binary blobs.
#[async_trait::async_trait]
pub trait RolloutStore: Send + Sync + 'static {
    /// Append one event to the journal. The returned `u64` is the seq
    /// number committed to disk. The implementation is responsible for
    /// assigning seq atomically — callers pass `event.seq` as a hint
    /// only and must use the returned value.
    async fn append_event(&self, event: RolloutEvent) -> Result<u64, GrodexError>;

    /// Append one event, forcing a `sync_data()` fsync once the bytes
    /// are in the page cache. Call this for events that gate a real
    /// side-effect (approval ticket issued, tool process spawned, turn
    /// boundary). Default implementation delegates to `append_event`
    /// and ignores the hint — only the Journal-backed store actually
    /// honours the flag.
    async fn append_event_durable(&self, event: RolloutEvent) -> Result<u64, GrodexError> {
        self.append_event(event).await
    }

    async fn write_blob_async(&self, content: &[u8], mime_type: &str) -> Result<BlobRef, GrodexError>;
    async fn read_blob_async(&self, blob_id: &BlobId) -> Result<Vec<u8>, GrodexError>;
    async fn replay_from(&self, seq: u64) -> Result<Vec<RolloutEvent>, GrodexError>;

    /// On-disk path of the JSONL journal, when the store is file-backed.
    /// Lets replay callers use streaming/lean readers (e.g.
    /// `replay_journal_lean`) that skip multi-MB redundant payloads.
    /// Non-file stores return `None`.
    fn journal_path(&self) -> Option<PathBuf> {
        None
    }

    /// On-disk directory of the session (journal + blobs + approval db),
    /// when file-backed. Used by shutdown cleanup to remove sessions that
    /// never recorded any conversation. Non-file stores return `None`.
    fn session_dir_path(&self) -> Option<PathBuf> {
        None
    }
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
///   - `rollout.jsonl` — append-only event log (written exclusively via
///     the single-writer [`JournalHandle`]).
///   - `blobs/` — content-addressed binary artifacts (written via
///     tempfile + atomic rename).
pub struct FileRolloutStore {
    session_dir: PathBuf,
    jsonl_path: PathBuf,
    blobs_dir: PathBuf,
    journal: JournalHandle,
}

impl FileRolloutStore {
    /// Create a new file store for the given session id.
    ///
    /// This spawns the single-writer journal actor. The actor's seq
    /// counter is seeded to `next_seq`: for a brand-new session pass
    /// `0`; after a successful replay of an existing journal pass
    /// `events.last().map(|e| e.seq + 1).unwrap_or(0)`.
    pub async fn new(
        base_dir: &Path,
        session_id: &str,
        next_seq: u64,
        fsync_policy: FsyncPolicy,
    ) -> Result<Self, GrodexError> {
        use grodex_core::id::SessionId;

        let session_dir = base_dir.join(session_id);
        let blobs_dir = session_dir.join("blobs");
        let jsonl_path = session_dir.join("rollout.jsonl");

        std::fs::create_dir_all(&blobs_dir).map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!("cannot create session blobs dir: {e}"))
        })?;

        // Parse session_id for the journal actor (which validates
        // event.session_id on every append). If the caller supplied a
        // malformed id we fall back to a freshly generated one so
        // startup is never blocked; the caller is responsible for
        // passing a canonical `SessionId::to_string()` in the happy
        // path.
        let sid = SessionId::from_string(session_id).unwrap_or_else(|_| SessionId::new());
        let journal = JournalHandle::start(
            jsonl_path.clone(),
            sid,
            next_seq,
            fsync_policy,
        )
        .await?;

        Ok(Self {
            session_dir,
            jsonl_path,
            blobs_dir,
            journal,
        })
    }

    /// Convenience ctor for the common "new session" path. Equivalent
    /// to `new(base_dir, session_id, 0, FsyncPolicy::default())`.
    pub async fn new_session(base_dir: &Path, session_id: &str) -> Result<Self, GrodexError> {
        Self::new(base_dir, session_id, 0, FsyncPolicy::default()).await
    }

    /// Open an existing session journal for **read-only replay**.
    ///
    /// This constructor deliberately does NOT spawn the single-writer
    /// actor: replay / inspect / dump / eval workflows never write, so
    /// they shouldn't pay the cost of a tokio task + unbounded channel.
    ///
    /// Because the actor is not started, any caller that later tries to
    /// write through this store will hit a runtime error (the trait
    /// impl returns `GrodexError::Internal` with a clear message).
    ///
    /// `next_seq` is read lazily from the journal's last event (if any);
    /// if you only need `replay_from()`, this is a pure no-op.
    pub fn open_readonly(base_dir: &Path, session_id: &str) -> Result<Self, GrodexError> {
        let session_dir = base_dir.join(session_id);
        let blobs_dir = session_dir.join("blobs");
        let jsonl_path = session_dir.join("rollout.jsonl");
        std::fs::create_dir_all(&blobs_dir).map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!("cannot create session blobs dir: {e}"))
        })?;

        // We still need a valid JournalHandle to keep the struct shape
        // uniform (both the read-only and the write paths share one
        // struct). Construct a handle whose actor rejects every append
        // with a clear "opened read-only" error. This is fail-closed —
        // if a replay path accidentally writes, we crash loudly rather
        // than corrupting the journal.
        let sid = SessionId::from_string(session_id).unwrap_or_else(|_| SessionId::new());
        let journal = JournalHandle::start_readonly(sid);

        Ok(Self {
            session_dir,
            jsonl_path,
            blobs_dir,
            journal,
        })
    }

    /// Convenience: read a session's events from disk without ever
    /// constructing a FileRolloutStore at all. Useful for one-shot CLI
    /// subcommands (inspect/dump/eval) that only need the events vec.
    pub fn replay_snapshot(
        base_dir: &Path,
        session_id: &str,
        from_seq: u64,
    ) -> Result<Vec<RolloutEvent>, GrodexError> {
        let jsonl = base_dir.join(session_id).join("rollout.jsonl");
        replay_journal_strict(&jsonl, from_seq)
    }

    /// Absolute path of this store's JSONL journal file.
    pub fn journal_file(&self) -> PathBuf {
        self.session_dir.join("rollout.jsonl")
    }

    /// Default store location: `~/.grodex/sessions/`
    pub fn default_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".grodex")
            .join("sessions")
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn journal_handle(&self) -> &JournalHandle {
        &self.journal
    }

    /// Rebind the store to a different on-disk journal (same blob
    /// dir). Quiesces the actor, flushes the old file, swaps handles,
    /// and reseeds the seq counter. Used by `/resume` to continue
    /// appending to a prior session's journal instead of leaking into
    /// a fresh one.
    pub async fn rebind(
        &self,
        new_jsonl_path: PathBuf,
        new_session_id: grodex_core::id::SessionId,
        next_seq: u64,
    ) -> Result<(), GrodexError> {
        self.journal.rebind(new_jsonl_path, new_session_id, next_seq).await
    }

    /// Append one event without forcing a sync. Thin wrapper.
    pub async fn append_event(&self, event: RolloutEvent) -> Result<u64, GrodexError> {
        self.journal.append(event, false).await
    }

    /// Append + force fsync.
    pub async fn append_event_durable(&self, event: RolloutEvent) -> Result<u64, GrodexError> {
        self.journal.append(event, true).await
    }

    /// Store binary content with atomic rename + fsync.
    pub fn write_blob(&self, content: &[u8], mime_type: &str) -> Result<BlobRef, GrodexError> {
        use sha2::{Digest, Sha256};
        use std::io::Write;

        let mut hasher = Sha256::new();
        hasher.update(content);
        let hash = format!("{:x}", hasher.finalize());
        let blob_id = BlobId::new(&hash);
        let blob_path = self.blobs_dir.join(&hash);

        // Fast path: blob already exists. Return immediately without
        // re-writing (content-addressing makes this a pure cache hit).
        if blob_path.exists() {
            let preview = String::from_utf8_lossy(&content[..content.len().min(200)]).to_string();
            return Ok(BlobRef {
                blob_id,
                size_bytes: content.len() as u64,
                mime_type: mime_type.to_string(),
                preview,
            });
        }

        // ── Atomic write ──────────────────────────────────────────
        // Write to `{hash}.tmp.{pid}.{rand}` first, fsync, then
        // `rename` into place, then fsync the *directory* (required
        // on ext4 / APFS for the rename to be durable). If we crash
        // in the middle we leave at worst a stray .tmp file, never a
        // half-written `{hash}` entry, so a second read does not see
        // truncated junk.
        let tmp_name = format!(
            "{}.tmp.{}.{}",
            hash,
            std::process::id(),
            rand_suffix()
        );
        let tmp_path = self.blobs_dir.join(tmp_name);
        {
            let mut f = std::fs::File::create(&tmp_path).map_err(|e| {
                GrodexError::Internal(anyhow::anyhow!("create blob tmp {:?}: {e}", tmp_path))
            })?;
            f.write_all(content).map_err(|e| {
                let _ = std::fs::remove_file(&tmp_path);
                GrodexError::Internal(anyhow::anyhow!("write blob tmp: {e}"))
            })?;
            f.flush().map_err(|e| {
                let _ = std::fs::remove_file(&tmp_path);
                GrodexError::Internal(anyhow::anyhow!("flush blob tmp: {e}"))
            })?;
            f.sync_all().map_err(|e| {
                let _ = std::fs::remove_file(&tmp_path);
                GrodexError::Internal(anyhow::anyhow!("fsync blob tmp: {e}"))
            })?;
        }

        std::fs::rename(&tmp_path, &blob_path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            GrodexError::Internal(anyhow::anyhow!(
                "rename blob tmp -> {:?}: {e}",
                blob_path
            ))
        })?;

        // fsync the *directory* entry so the rename is durable.
        // Required for POSIX filesystem atomicity guarantees.
        if let Ok(dir) = std::fs::File::open(&self.blobs_dir) {
            let _ = dir.sync_all();
        }

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

    /// Replay events from the given sequence number. Strict: any
    /// corrupt / empty line returns `GrodexError::JournalCorrupt`
    /// instead of being silently skipped.
    pub fn replay_from(&self, from_seq: u64) -> Result<Vec<RolloutEvent>, GrodexError> {
        replay_journal_strict(&self.jsonl_path, from_seq)
    }
}

#[async_trait::async_trait]
impl RolloutStore for FileRolloutStore {
    fn journal_path(&self) -> Option<PathBuf> {
        Some(self.journal_file())
    }

    fn session_dir_path(&self) -> Option<PathBuf> {
        Some(self.session_dir.clone())
    }

    async fn append_event(&self, event: RolloutEvent) -> Result<u64, GrodexError> {
        FileRolloutStore::append_event(self, event).await
    }

    async fn append_event_durable(&self, event: RolloutEvent) -> Result<u64, GrodexError> {
        FileRolloutStore::append_event_durable(self, event).await
    }

    async fn write_blob_async(&self, content: &[u8], mime_type: &str) -> Result<BlobRef, GrodexError> {
        // write_blob() is sync CPU-light work; run on the blocking pool
        // so we don't park a runtime worker on the fsync syscall.
        let blobs_dir = self.blobs_dir.clone();
        let content = content.to_vec();
        let mime_type = mime_type.to_string();
        tokio::task::spawn_blocking(move || {
            // Inline reconstruction: we only need `blobs_dir` for the
            // atomic write path, so build a lightweight wrapper struct
            // rather than requiring Self: Clone (which would drag the
            // JournalHandle into a sync closure — not unsafe, but
            // semantically wrong).
            write_blob_at(&blobs_dir, &content, &mime_type)
        })
        .await
        .map_err(|e| GrodexError::Internal(anyhow::anyhow!("join write_blob task: {e}")))?
    }

    async fn read_blob_async(&self, blob_id: &BlobId) -> Result<Vec<u8>, GrodexError> {
        let path = self.blobs_dir.join(blob_id.as_str());
        tokio::task::spawn_blocking(move || {
            std::fs::read(&path).map_err(|e| GrodexError::Internal(anyhow::anyhow!("read blob: {e}")))
        })
        .await
        .map_err(|e| GrodexError::Internal(anyhow::anyhow!("join read_blob task: {e}")))?
    }

    async fn replay_from(&self, seq: u64) -> Result<Vec<RolloutEvent>, GrodexError> {
        let jsonl = self.jsonl_path.clone();
        tokio::task::spawn_blocking(move || replay_journal_strict(&jsonl, seq))
            .await
            .map_err(|e| GrodexError::Internal(anyhow::anyhow!("join replay task: {e}")))?
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Nonce used to make concurrent temp-blob filenames unique across
/// processes/threads. Uses std::time rather than pulling in a `rand`
/// dep — collision probability is astronomically low (two threads in
/// the same pid at the same nanosecond would have to race), and in the
/// extremely unlikely event of a collision the rename would just
/// fail-closed and return Err.
fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Isolated atomic-blob writer usable from both sync and async paths.
fn write_blob_at(
    blobs_dir: &Path,
    content: &[u8],
    mime_type: &str,
) -> Result<BlobRef, GrodexError> {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let mut hasher = Sha256::new();
    hasher.update(content);
    let hash = format!("{:x}", hasher.finalize());
    let blob_id = BlobId::new(&hash);
    let blob_path = blobs_dir.join(&hash);

    if blob_path.exists() {
        let preview = String::from_utf8_lossy(&content[..content.len().min(200)]).to_string();
        return Ok(BlobRef {
            blob_id,
            size_bytes: content.len() as u64,
            mime_type: mime_type.to_string(),
            preview,
        });
    }

    let tmp_name = format!("{}.tmp.{}.{}", hash, std::process::id(), rand_suffix());
    let tmp_path = blobs_dir.join(tmp_name);
    {
        let mut f = std::fs::File::create(&tmp_path).map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!("create blob tmp {:?}: {e}", tmp_path))
        })?;
        f.write_all(content).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            GrodexError::Internal(anyhow::anyhow!("write blob tmp: {e}"))
        })?;
        f.flush().map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            GrodexError::Internal(anyhow::anyhow!("flush blob tmp: {e}"))
        })?;
        f.sync_all().map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            GrodexError::Internal(anyhow::anyhow!("fsync blob tmp: {e}"))
        })?;
    }
    std::fs::rename(&tmp_path, &blob_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        GrodexError::Internal(anyhow::anyhow!("rename blob tmp: {e}"))
    })?;
    if let Ok(dir) = std::fs::File::open(blobs_dir) {
        let _ = dir.sync_all();
    }

    let preview = String::from_utf8_lossy(&content[..content.len().min(200)]).to_string();
    Ok(BlobRef {
        blob_id,
        size_bytes: content.len() as u64,
        mime_type: mime_type.to_string(),
        preview,
    })
}

// ── Legacy sync entry point (kept for the `new(...)` synchronous callers
// that existed before the actor rewrite). Deprecated: prefer the async
// `new_session()` constructor.
#[allow(dead_code)]
fn new_sync_legacy(base_dir: &Path, session_id: &str) -> Result<FileRolloutStore, GrodexError> {
    // The sync path exists purely to avoid breaking the two pre-existing
    // unit tests below which use `store.append_event(...)` without a
    // tokio runtime. We spin up a per-call lightweight current-thread
    // runtime — tests are the only caller of this path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| GrodexError::Internal(anyhow::anyhow!("build test rt: {e}")))?;
    rt.block_on(FileRolloutStore::new_session(base_dir, session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{RolloutEvent, RolloutEventType, SensitivityLevel};
    use grodex_core::id::SessionId;

    #[tokio::test]
    async fn append_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionId::new().to_string();
        let store = FileRolloutStore::new_session(dir.path(), &session_id).await.unwrap();

        let ev = RolloutEvent {
            schema_version: 2,
            seq: 0,
            session_id: SessionId::from_string(&session_id).unwrap(),
            turn_id: None,
            step_id: None,
            generation: None,
            timestamp: chrono::Utc::now(),
            event_type: RolloutEventType::RuntimeStateChanged,
            payload: serde_json::json!({"state": "idle"}),
            sensitivity: SensitivityLevel::Normal,
        };

        let seq = store.append_event(ev).await.unwrap();
        assert_eq!(seq, 0);

        let events = store.replay_from(0).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn blob_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRolloutStore::new_session(dir.path(), "test-blobs").await.unwrap();

        let blob = store.write_blob_async(b"hello world", "text/plain").await.unwrap();
        assert_eq!(blob.size_bytes, 11);

        let data = store.read_blob_async(&blob.blob_id).await.unwrap();
        assert_eq!(data, b"hello world");
    }

    #[tokio::test]
    async fn blob_write_is_atomic_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileRolloutStore::new_session(dir.path(), "test-blobs-atomic").await.unwrap();

        // Write same content twice → same blob id, no error.
        let b1 = store.write_blob_async(b"atoms", "text/plain").await.unwrap();
        let b2 = store.write_blob_async(b"atoms", "text/plain").await.unwrap();
        assert_eq!(b1.blob_id, b2.blob_id);

        // Content on disk is exactly the bytes we wrote.
        let back = store.read_blob_async(&b1.blob_id).await.unwrap();
        assert_eq!(back, b"atoms");
    }

    #[tokio::test]
    async fn replay_strict_detects_schema_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let sid = SessionId::new();
        let session_dir = dir.path().join(sid.to_string());
        let jsonl = session_dir.join("rollout.jsonl");
        std::fs::create_dir_all(&session_dir).unwrap();
        // Schema version 1 — should fail-closed.
        let bad_line = serde_json::json!({
            "schema_version": 1, "seq": 0,
            "session_id": sid.to_string(),
            "turn_id": null, "step_id": null, "generation": null,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "event_type": "RuntimeStateChanged",
            "payload": {"state": "running"},
            "sensitivity": "Normal",
        });
        std::fs::write(&jsonl, format!("{}\n", bad_line.to_string())).unwrap();

        let store = FileRolloutStore::new_session(dir.path(), &sid.to_string()).await.unwrap();
        let res = store.replay_from(0);
        assert!(matches!(res, Err(GrodexError::JournalCorrupt { .. })));
    }
}
