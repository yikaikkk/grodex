//! Blob store — offloads large tool results from the inline context.
//!
//! When a tool produces output exceeding the inline budget (e.g. large
//! build logs, test output, binary artifacts), the result is written to
//! the blob store and a `BlobRef` is returned to the model instead.
//! The model can later request a bounded view (head+tail) via the
//! `blob_ref` without re-executing the tool.
//!
//! Design Doc 15 §12: "bounded view + blob ref".

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::Mutex;

use crate::blob_refs::{BlobOwnerKind, BlobRefKind, BlobRefLedger};
use sha2::{Digest, Sha256};

/// A reference to a blob stored outside the inline context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobRef {
    /// Unique identifier for the blob.
    pub blob_id: String,
    /// Size of the full blob in bytes.
    pub size_bytes: u64,
    /// MIME type hint (e.g. "text/plain", "application/octet-stream").
    pub mime_type: String,
    /// A short preview (head of the content) for the model.
    pub preview: String,
    /// When the blob was stored.
    pub stored_at: SystemTime,
}

/// Trait for blob storage backends.
///
/// The agent loop provides a concrete implementation; tools receive
/// it via the tool runtime context.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Store a blob and return a reference.
    async fn store(&self, data: Vec<u8>, mime_type: String) -> BlobRef;

    /// Retrieve the full blob by ID. Returns None if not found or evicted.
    async fn retrieve(&self, blob_id: &str) -> Option<Vec<u8>>;

    /// Retrieve a bounded view (head + tail) of the blob.
    async fn retrieve_bounded(
        &self,
        blob_id: &str,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> Option<BoundedView>;

    /// Delete a blob. Returns true if it existed.
    async fn delete(&self, blob_id: &str) -> bool;

    /// List all blob IDs.
    async fn list(&self) -> Vec<String>;
}

/// File-backed blob store — the production backend (Doc 15 §12).
///
/// Content-addressable: the blob id is the SHA-256 of the content, so
/// identical outputs dedupe to one file and re-store is idempotent.
/// Each blob is one file under `root`; metadata (mime/preview/stored_at)
/// lives in memory — after a restart the files remain but are only
/// reachable through a rebuilt [`BlobRefLedger`] projection (Doc 11 §22:
/// liveness comes from refs, never from scanning the directory).
pub struct FileBlobStore {
    root: std::path::PathBuf,
    meta: Mutex<FileBlobMeta>,
}

struct FileBlobMeta {
    /// blob_id → (mime_type, size, stored_at).
    known: HashMap<String, (String, u64, SystemTime)>,
}

impl FileBlobStore {
    /// Create the store rooted at `root` (directory created on demand).
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            meta: Mutex::new(FileBlobMeta {
                known: HashMap::new(),
            }),
        }
    }

    /// On-disk location of a blob (used to hand the model a readable
    /// path for the offloaded content).
    pub fn path_of(&self, blob_id: &str) -> std::path::PathBuf {
        self.root.join(format!("{blob_id}.blob"))
    }

    fn blob_id_for(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }
}

#[async_trait::async_trait]
impl BlobStore for FileBlobStore {
    async fn store(&self, data: Vec<u8>, mime_type: String) -> BlobRef {
        let blob_id = Self::blob_id_for(&data);
        let path = self.path_of(&blob_id);
        let size = data.len() as u64;
        let now = SystemTime::now();
        // Best effort: a write failure surfaces as a missing blob on
        // retrieve rather than a panic — callers already treat missing
        // blobs as evicted.
        if tokio::fs::create_dir_all(&self.root).await.is_ok() {
            let _ = tokio::fs::write(&path, &data).await;
        }
        let preview = String::from_utf8_lossy(&data[..data.len().min(200)]).to_string();
        self.meta
            .lock()
            .await
            .known
            .insert(blob_id.clone(), (mime_type.clone(), size, now));
        BlobRef {
            blob_id,
            size_bytes: size,
            mime_type,
            preview,
            stored_at: now,
        }
    }

    async fn retrieve(&self, blob_id: &str) -> Option<Vec<u8>> {
        tokio::fs::read(self.path_of(blob_id)).await.ok()
    }

    async fn retrieve_bounded(
        &self,
        blob_id: &str,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> Option<BoundedView> {
        let data = tokio::fs::read(self.path_of(blob_id)).await.ok()?;
        let total = data.len();
        if total <= head_bytes + tail_bytes {
            return Some(BoundedView {
                head: data,
                tail: Vec::new(),
                omitted_bytes: 0,
                total_bytes: total as u64,
            });
        }
        let head = data[..head_bytes].to_vec();
        let tail_start = total.saturating_sub(tail_bytes);
        let tail = data[tail_start..].to_vec();
        Some(BoundedView {
            head,
            tail,
            omitted_bytes: (tail_start - head_bytes) as u64,
            total_bytes: total as u64,
        })
    }

    async fn delete(&self, blob_id: &str) -> bool {
        let removed = tokio::fs::remove_file(self.path_of(blob_id)).await.is_ok();
        if removed {
            self.meta.lock().await.known.remove(blob_id);
        }
        removed
    }

    async fn list(&self) -> Vec<String> {
        // The in-memory view is authoritative for live blobs; files on
        // disk without metadata are orphans awaiting a rebuilt ledger.
        self.meta.lock().await.known.keys().cloned().collect()
    }
}

/// A bounded view of a blob: head + tail with an elision marker.
#[derive(Debug, Clone)]
pub struct BoundedView {
    /// The head portion.
    pub head: Vec<u8>,
    /// The tail portion.
    pub tail: Vec<u8>,
    /// Number of bytes omitted between head and tail.
    pub omitted_bytes: u64,
    /// Total size of the original blob.
    pub total_bytes: u64,
}

impl BoundedView {
    /// Render as a UTF-8 string with elision marker.
    pub fn render_text(&self) -> String {
        let head_str = String::from_utf8_lossy(&self.head);
        let tail_str = String::from_utf8_lossy(&self.tail);
        if self.omitted_bytes == 0 {
            format!("{head_str}{tail_str}")
        } else {
            format!(
                "{head_str}\n... [{}, {} bytes omitted] ...\n{tail_str}",
                self.omitted_bytes, self.omitted_bytes
            )
        }
    }
}

/// In-memory blob store for testing and single-session use.
///
/// Production deployments should use a file-backed or remote store.
#[derive(Clone)]
pub struct InMemoryBlobStore {
    inner: Arc<Mutex<InMemoryBlobStoreInner>>,
}

struct InMemoryBlobStoreInner {
    blobs: HashMap<String, (Vec<u8>, String, SystemTime)>,
    next_id: u64,
    /// Maximum total bytes across all stored blobs.
    max_total_bytes: u64,
    current_total_bytes: u64,
}

impl Default for InMemoryBlobStore {
    fn default() -> Self {
        Self::new(100 * 1024 * 1024) // 100 MB default cap
    }
}

impl InMemoryBlobStore {
    pub fn new(max_total_bytes: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(InMemoryBlobStoreInner {
                blobs: HashMap::new(),
                next_id: 0,
                max_total_bytes,
                current_total_bytes: 0,
            })),
        }
    }
}

#[async_trait::async_trait]
impl BlobStore for InMemoryBlobStore {
    async fn store(&self, data: Vec<u8>, mime_type: String) -> BlobRef {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;
        let blob_id = format!("blob-{id}");
        let size = data.len() as u64;

        // Evict oldest blobs if over capacity.
        while inner.current_total_bytes + size > inner.max_total_bytes && !inner.blobs.is_empty()
        {
            // Find the oldest blob by stored_at.
            let oldest = inner
                .blobs
                .iter()
                .min_by_key(|(_, (_, _, t))| *t)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest {
                if let Some((data, _, _)) = inner.blobs.remove(&key) {
                    inner.current_total_bytes -= data.len() as u64;
                }
            }
        }

        let preview = String::from_utf8_lossy(&data[..data.len().min(200)]).to_string();
        let now = SystemTime::now();

        inner.blobs.insert(blob_id.clone(), (data, mime_type.clone(), now));
        inner.current_total_bytes += size;

        BlobRef {
            blob_id,
            size_bytes: size,
            mime_type,
            preview,
            stored_at: now,
        }
    }

    async fn retrieve(&self, blob_id: &str) -> Option<Vec<u8>> {
        self.inner
            .lock()
            .await
            .blobs
            .get(blob_id)
            .map(|(data, _, _)| data.clone())
    }

    async fn retrieve_bounded(
        &self,
        blob_id: &str,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> Option<BoundedView> {
        let inner = self.inner.lock().await;
        let (data, _, _) = inner.blobs.get(blob_id)?;
        let total = data.len();

        if total <= head_bytes + tail_bytes {
            return Some(BoundedView {
                head: data.clone(),
                tail: Vec::new(),
                omitted_bytes: 0,
                total_bytes: total as u64,
            });
        }

        let head = data[..head_bytes].to_vec();
        let tail_start = total.saturating_sub(tail_bytes);
        let tail = data[tail_start..].to_vec();
        let omitted = (tail_start - head_bytes) as u64;

        Some(BoundedView {
            head,
            tail,
            omitted_bytes: omitted,
            total_bytes: total as u64,
        })
    }

    async fn delete(&self, blob_id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        if let Some((data, _, _)) = inner.blobs.remove(blob_id) {
            inner.current_total_bytes -= data.len() as u64;
            true
        } else {
            false
        }
    }

    async fn list(&self) -> Vec<String> {
        self.inner.lock().await.blobs.keys().cloned().collect()
    }
}

/// A blob store whose liveness is governed by the rebuildable
/// [`BlobRefLedger`] projection instead of LRU/dir-scan guessing
/// (Doc 11 §22).
///
/// Wraps any backing [`BlobStore`] and makes reference counting + the
/// retention grace period the ONLY path that deletes a blob:
///
/// - [`store_owned`](Self::store_owned) stores the blob and registers the
///   owner's reference in one step;
/// - [`grant_ref`](Self::grant_ref) / [`revoke_owner`](Self::revoke_owner)
///   add / remove references (candidate void, Session/Memory delete);
/// - [`gc_at`](Self::gc_at) deletes exactly the blobs whose ref count hit
///   zero and stayed there for the grace period (and whose TTL passed),
///   then forgets them from the projection.
///
/// The backing store's `blob_id` is the deletion identity; the ledger is
/// keyed by the content `blob_hash`, with an internal hash→id map so GC
/// removes the right backing entry. The ledger stays a PROJECTION — it can
/// be rebuilt via [`BlobRefLedger::rebuild`] without touching the store.
#[derive(Clone)]
pub struct ManagedBlobStore<S: BlobStore> {
    inner: S,
    /// Ledger + hash→blob_id map. Held in a `std` Mutex because no
    /// `.await` ever happens while it is locked.
    state: Arc<std::sync::Mutex<ManagedState>>,
    /// Retention grace period after a blob's ref count hits zero.
    grace: Duration,
}

struct ManagedState {
    ledger: BlobRefLedger,
    /// content hash → backing store blob_id.
    id_by_hash: HashMap<String, String>,
}

impl<S: BlobStore> ManagedBlobStore<S> {
    /// Wrap `inner` with a fresh (empty) ledger and a `grace` window.
    pub fn new(inner: S, grace: Duration) -> Self {
        Self {
            inner,
            state: Arc::new(std::sync::Mutex::new(ManagedState {
                ledger: BlobRefLedger::new(),
                id_by_hash: HashMap::new(),
            })),
            grace,
        }
    }

    /// Content-addressable identity used as the ledger key.
    pub fn content_hash(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    /// Store a blob AND register `owner`'s reference in one step.
    /// Returns the backing store's [`BlobRef`] plus the content hash.
    pub async fn store_owned(
        &self,
        data: Vec<u8>,
        mime_type: String,
        owner_kind: BlobOwnerKind,
        owner_id: impl Into<String>,
        ref_kind: BlobRefKind,
        expires_at: Option<SystemTime>,
    ) -> (BlobRef, String) {
        let blob_hash = Self::content_hash(&data);
        let blob_ref = self.inner.store(data, mime_type).await;
        let owner_id = owner_id.into();
        let mut st = self.state.lock().unwrap();
        st.id_by_hash.insert(blob_hash.clone(), blob_ref.blob_id.clone());
        st.ledger
            .add_ref(&blob_hash, owner_kind, owner_id, ref_kind, SystemTime::now(), expires_at);
        (blob_ref, blob_hash)
    }

    /// Register an additional owner's reference to an already-stored blob.
    pub fn grant_ref(
        &self,
        blob_hash: &str,
        owner_kind: BlobOwnerKind,
        owner_id: impl Into<String>,
        ref_kind: BlobRefKind,
        expires_at: Option<SystemTime>,
    ) {
        let mut st = self.state.lock().unwrap();
        st.ledger
            .add_ref(blob_hash, owner_kind, owner_id, ref_kind, SystemTime::now(), expires_at);
    }

    /// Remove ONLY the references owned by `(owner_kind, owner_id)`
    /// (candidate void / Session delete / Memory delete semantics).
    /// Returns the touched blob hashes; deletion still waits for GC.
    pub fn revoke_owner(&self, owner_kind: BlobOwnerKind, owner_id: &str) -> Vec<String> {
        let mut st = self.state.lock().unwrap();
        st.ledger
            .remove_owner_refs(owner_kind, owner_id, SystemTime::now())
    }

    /// Live reference count for a blob hash (0 if untracked).
    pub fn ref_count(&self, blob_hash: &str) -> usize {
        self.state.lock().unwrap().ledger.ref_count(blob_hash)
    }

    /// GC at an explicit clock (testable core of [`gc`](Self::gc)).
    /// Deletes from the backing store and forgets from the projection
    /// exactly the blobs eligible at `now`. Returns deleted hashes.
    pub async fn gc_at(&self, now: SystemTime) -> Vec<String> {
        // Collect while holding the ledger, then delete without it.
        let eligible = self
            .state
            .lock()
            .unwrap()
            .ledger
            .collect_garbage(now, self.grace);
        if eligible.is_empty() {
            return Vec::new();
        }
        let mut deleted = Vec::new();
        for hash in &eligible {
            let blob_id = self
                .state
                .lock()
                .unwrap()
                .id_by_hash
                .get(hash)
                .cloned();
            if let Some(id) = blob_id {
                self.inner.delete(&id).await;
            }
            deleted.push(hash.clone());
        }
        let mut st = self.state.lock().unwrap();
        for hash in &deleted {
            st.id_by_hash.remove(hash);
        }
        st.ledger.forget(&deleted);
        deleted
    }

    /// GC at the current wall clock.
    pub async fn gc(&self) -> Vec<String> {
        self.gc_at(SystemTime::now()).await
    }

    /// The retention grace period this store applies.
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Access the backing store (e.g. to resolve a blob's on-disk path).
    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// Rebuild the projection from source-of-truth records. The hash→id
    /// map is preserved (it mirrors the backing store), only the ledger
    /// state is replaced.
    pub fn rebuild_refs(&self, sources: &[crate::blob_refs::BlobRefRecord]) {
        let mut st = self.state.lock().unwrap();
        st.ledger.rebuild(sources, SystemTime::now());
    }
}

#[async_trait::async_trait]
impl<S: BlobStore> BlobStore for ManagedBlobStore<S> {
    /// Unowned store: registered as a Session-scoped attachment keyed by
    /// the blob id so the blob is tracked (not immediately GC-able) until
    /// the owning session revokes it.
    async fn store(&self, data: Vec<u8>, mime_type: String) -> BlobRef {
        let (blob_ref, _hash) = self
            .store_owned(
                data,
                mime_type,
                BlobOwnerKind::Session,
                format!("session:{}", uuid::Uuid::new_v4()),
                BlobRefKind::SessionAttachment,
                None,
            )
            .await;
        blob_ref
    }

    async fn retrieve(&self, blob_id: &str) -> Option<Vec<u8>> {
        self.inner.retrieve(blob_id).await
    }

    async fn retrieve_bounded(
        &self,
        blob_id: &str,
        head_bytes: usize,
        tail_bytes: usize,
    ) -> Option<BoundedView> {
        self.inner.retrieve_bounded(blob_id, head_bytes, tail_bytes).await
    }

    /// Forced single-blob deletion: only succeeds once the blob is
    /// unreferenced, preserving the "never delete a live blob" invariant.
    /// Returns false while references remain.
    async fn delete(&self, blob_id: &str) -> bool {
        // Find the hash backing this blob_id; if untracked, fall through
        // to a plain delete (nothing to protect).
        let hash = {
            let st = self.state.lock().unwrap();
            st.id_by_hash
                .iter()
                .find(|(_, id)| id.as_str() == blob_id)
                .map(|(h, _)| h.clone())
        };
        if let Some(h) = hash.as_deref() {
            if self.ref_count(h) > 0 {
                return false;
            }
        }
        let removed = self.inner.delete(blob_id).await;
        if removed {
            let mut st = self.state.lock().unwrap();
            if let Some(h) = hash {
                st.id_by_hash.remove(&h);
                st.ledger.forget(&[h]);
            }
        }
        removed
    }

    async fn list(&self) -> Vec<String> {
        self.inner.list().await
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blob_store_round_trip() {
        let store = InMemoryBlobStore::new(1024);
        let data = b"hello world".to_vec();
        let blob_ref = store.store(data.clone(), "text/plain".into()).await;

        assert_eq!(blob_ref.size_bytes, 11);
        assert_eq!(blob_ref.mime_type, "text/plain");
        assert!(blob_ref.preview.contains("hello"));

        let retrieved = store.retrieve(&blob_ref.blob_id).await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn blob_store_bounded_view() {
        let store = InMemoryBlobStore::new(10240);
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let blob_ref = store.store(data.clone(), "application/octet-stream".into()).await;

        let view = store
            .retrieve_bounded(&blob_ref.blob_id, 100, 100)
            .await
            .unwrap();
        assert_eq!(view.head.len(), 100);
        assert_eq!(view.tail.len(), 100);
        assert_eq!(view.total_bytes, 1000);
        assert!(view.omitted_bytes > 0);
    }

    #[tokio::test]
    async fn blob_store_eviction() {
        let store = InMemoryBlobStore::new(100); // tiny cap
        let _r1 = store.store(vec![0u8; 60], "text/plain".into()).await;
        let r2 = store.store(vec![1u8; 60], "text/plain".into()).await;

        // First blob should be evicted.
        let list = store.list().await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], r2.blob_id);
    }

    #[tokio::test]
    async fn blob_store_delete() {
        let store = InMemoryBlobStore::new(1024);
        let r = store.store(b"data".to_vec(), "text/plain".into()).await;
        assert!(store.delete(&r.blob_id).await);
        assert!(!store.delete(&r.blob_id).await); // already deleted
        assert!(store.retrieve(&r.blob_id).await.is_none());
    }

    // ── ManagedBlobStore: ledger-governed lifecycle (Doc 11 §22) ──

    fn managed() -> ManagedBlobStore<InMemoryBlobStore> {
        // Large cap: the backing LRU must never fire — liveness is the
        // ledger's job.
        ManagedBlobStore::new(InMemoryBlobStore::new(64 * 1024 * 1024), Duration::from_secs(60))
    }

    #[tokio::test]
    async fn managed_store_gc_never_touches_referenced_blobs() {
        let store = managed();
        let (blob_ref, hash) = store
            .store_owned(
                b"important output".to_vec(),
                "text/plain".into(),
                BlobOwnerKind::ToolResult,
                "call-1",
                BlobRefKind::ToolOutputBody,
                None,
            )
            .await;
        assert_eq!(store.ref_count(&hash), 1);

        // Far future GC: still referenced → nothing deleted.
        let deleted = store
            .gc_at(SystemTime::now() + Duration::from_secs(10_000))
            .await;
        assert!(deleted.is_empty());
        assert!(store.retrieve(&blob_ref.blob_id).await.is_some());
    }

    #[tokio::test]
    async fn managed_store_revoked_blob_survives_grace_then_gcs() {
        let store = managed();
        let (blob_ref, hash) = store
            .store_owned(
                b"temporary".to_vec(),
                "text/plain".into(),
                BlobOwnerKind::ToolResult,
                "call-1",
                BlobRefKind::ToolOutputBody,
                None,
            )
            .await;

        // Owner voided → ref count zero, grace clock starts.
        store.revoke_owner(BlobOwnerKind::ToolResult, "call-1");
        assert_eq!(store.ref_count(&hash), 0);

        // Inside the grace window: still protected.
        let deleted = store
            .gc_at(SystemTime::now() + Duration::from_secs(30))
            .await;
        assert!(deleted.is_empty());
        assert!(store.retrieve(&blob_ref.blob_id).await.is_some());

        // After the grace window: deleted from the backing store AND
        // forgotten from the projection.
        let deleted = store
            .gc_at(SystemTime::now() + Duration::from_secs(61))
            .await;
        assert_eq!(deleted, vec![hash.clone()]);
        assert!(store.retrieve(&blob_ref.blob_id).await.is_none());
        // Second GC finds nothing (projection purged).
        let again = store
            .gc_at(SystemTime::now() + Duration::from_secs(10_000))
            .await;
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn managed_store_revoke_is_owner_scoped() {
        let store = managed();
        // Same content shared by two owners → one hash, two refs.
        let (blob_ref, hash) = store
            .store_owned(
                b"shared".to_vec(),
                "text/plain".into(),
                BlobOwnerKind::CandidateCompaction,
                "cand-1",
                BlobRefKind::CompactionPayload,
                None,
            )
            .await;
        store.grant_ref(
            &hash,
            BlobOwnerKind::Checkpoint,
            "cp-1",
            BlobRefKind::CheckpointPayload,
            None,
        );
        assert_eq!(store.ref_count(&hash), 2);

        // Candidate voided: checkpoint still holds the blob.
        store.revoke_owner(BlobOwnerKind::CandidateCompaction, "cand-1");
        assert_eq!(store.ref_count(&hash), 1);
        let deleted = store
            .gc_at(SystemTime::now() + Duration::from_secs(10_000))
            .await;
        assert!(deleted.is_empty());
        assert!(store.retrieve(&blob_ref.blob_id).await.is_some());
    }

    #[tokio::test]
    async fn managed_store_delete_refuses_live_blobs() {
        let store = managed();
        let (blob_ref, _hash) = store
            .store_owned(
                b"keep me".to_vec(),
                "text/plain".into(),
                BlobOwnerKind::ToolResult,
                "call-1",
                BlobRefKind::ToolOutputBody,
                None,
            )
            .await;
        // Referenced → forced delete refused.
        assert!(!store.delete(&blob_ref.blob_id).await);
        assert!(store.retrieve(&blob_ref.blob_id).await.is_some());
        // After revoke the explicit delete succeeds.
        store.revoke_owner(BlobOwnerKind::ToolResult, "call-1");
        assert!(store.delete(&blob_ref.blob_id).await);
        assert!(store.retrieve(&blob_ref.blob_id).await.is_none());
    }

    #[tokio::test]
    async fn managed_store_ttl_blocks_gc_past_grace() {
        let store = managed();
        let ttl = SystemTime::now() + Duration::from_secs(500);
        let (_blob_ref, hash) = store
            .store_owned(
                b"ttl-protected".to_vec(),
                "text/plain".into(),
                BlobOwnerKind::MemoryEvidence,
                "mem-1",
                BlobRefKind::EvidenceCitation,
                Some(ttl),
            )
            .await;
        store.revoke_owner(BlobOwnerKind::MemoryEvidence, "mem-1");
        // Grace elapsed but the blob's TTL still holds.
        let deleted = store
            .gc_at(SystemTime::now() + Duration::from_secs(120))
            .await;
        assert!(deleted.is_empty());
        // Past the TTL: eligible.
        let deleted = store
            .gc_at(SystemTime::now() + Duration::from_secs(501))
            .await;
        assert_eq!(deleted, vec![hash]);
    }

    #[tokio::test]
    async fn managed_store_rebuild_preserves_protection() {
        let store = managed();
        let (blob_ref, hash) = store
            .store_owned(
                b"rebuilt".to_vec(),
                "text/plain".into(),
                BlobOwnerKind::ToolResult,
                "call-1",
                BlobRefKind::ToolOutputBody,
                None,
            )
            .await;
        // Rebuild from source-of-truth records: same blob, same owner.
        store.rebuild_refs(&[crate::blob_refs::BlobRefRecord {
            blob_hash: hash.clone(),
            owner_kind: BlobOwnerKind::ToolResult,
            owner_id: "call-1".into(),
            ref_kind: BlobRefKind::ToolOutputBody,
            created_seq: 7,
            expires_at: None,
        }]);
        assert_eq!(store.ref_count(&hash), 1);
        // GC must not delete it after the rebuild.
        let deleted = store
            .gc_at(SystemTime::now() + Duration::from_secs(10_000))
            .await;
        assert!(deleted.is_empty());
        assert!(store.retrieve(&blob_ref.blob_id).await.is_some());
    }

    // ── FileBlobStore: file-backed production backend ──

    #[tokio::test]
    async fn file_store_round_trip_and_dedupe() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FileBlobStore::new(dir.path().join("blobs"));
        let r1 = store.store(b"same content".to_vec(), "text/plain".into()).await;
        let r2 = store.store(b"same content".to_vec(), "text/plain".into()).await;
        // Content-addressable: identical content → same blob id.
        assert_eq!(r1.blob_id, r2.blob_id);
        assert_eq!(store.list().await.len(), 1);
        // The file exists at path_of and reads back intact.
        assert!(store.path_of(&r1.blob_id).exists());
        assert_eq!(store.retrieve(&r1.blob_id).await.unwrap(), b"same content");
    }

    #[tokio::test]
    async fn file_store_bounded_view_and_delete() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = FileBlobStore::new(dir.path());
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        let r = store.store(data, "application/octet-stream".into()).await;
        let view = store.retrieve_bounded(&r.blob_id, 100, 100).await.unwrap();
        assert_eq!(view.head.len(), 100);
        assert_eq!(view.tail.len(), 100);
        assert!(view.omitted_bytes > 0);
        assert!(store.delete(&r.blob_id).await);
        assert!(!store.path_of(&r.blob_id).exists());
        assert!(store.retrieve(&r.blob_id).await.is_none());
    }

    #[tokio::test]
    async fn managed_file_store_gcs_real_files() {
        // End-to-end production shape: ledger-governed GC removes the
        // actual file from disk after the grace period.
        let dir = tempfile::TempDir::new().unwrap();
        let store = ManagedBlobStore::new(
            FileBlobStore::new(dir.path()),
            Duration::from_secs(30),
        );
        let (blob_ref, hash) = store
            .store_owned(
                b"large tool output".to_vec(),
                "text/plain".into(),
                BlobOwnerKind::ToolResult,
                "session-1",
                BlobRefKind::ToolOutputBody,
                None,
            )
            .await;
        let file = store.inner().path_of(&blob_ref.blob_id);
        assert!(file.exists());

        // Session ended → refs revoked; inside grace the file survives.
        store.revoke_owner(BlobOwnerKind::ToolResult, "session-1");
        assert!(store.gc_at(SystemTime::now() + Duration::from_secs(10)).await.is_empty());
        assert!(file.exists());

        // Past grace: the file is removed from disk.
        assert_eq!(
            store.gc_at(SystemTime::now() + Duration::from_secs(31)).await,
            vec![hash]
        );
        assert!(!file.exists());
    }
}
