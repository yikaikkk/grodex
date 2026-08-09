//! MemoryEntry — a single long-term memory record.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A memory entry stored for future reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Unique identifier.
    pub id: String,
    /// Memory content.
    pub content: String,
    /// Tags for categorization and retrieval.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Importance (0-100).
    #[serde(default)]
    pub importance: u8,
    /// When this memory was created.
    pub created_at: DateTime<Utc>,
    /// When this memory was last accessed.
    #[serde(default)]
    pub last_accessed: Option<DateTime<Utc>>,
    /// Number of times accessed.
    #[serde(default)]
    pub access_count: u64,
}

impl MemoryEntry {
    /// Create a new memory entry.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.into(),
            tags: Vec::new(),
            importance: 0,
            created_at: Utc::now(),
            last_accessed: None,
            access_count: 0,
        }
    }

    /// Add tags to the entry.
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Set importance.
    pub fn with_importance(mut self, importance: u8) -> Self {
        self.importance = importance.min(100);
        self
    }

    /// Record an access.
    pub fn record_access(&mut self) {
        self.access_count += 1;
        self.last_accessed = Some(Utc::now());
    }
}
