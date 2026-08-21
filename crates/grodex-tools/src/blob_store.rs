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
use std::time::SystemTime;
use tokio::sync::Mutex;

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
}
