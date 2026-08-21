//! PromptSnapshot — exact request view sent to the model at one sampling step.
//!
//! Design Doc 11 §3: the snapshot is one of four separated data types.
//! It captures exactly what the model saw (context items, tool schemas, token count)
//! with a content hash for cache invalidation and audit.

use crate::canonical_request::ToolSpec;
use chrono::{DateTime, Utc};
use grodex_core::context::ContextItem;
use serde::{Deserialize, Serialize};

/// The exact request sent to the model at one sampling step.
///
/// Immutable after creation. The content hash covers all items + tool schemas.
/// Used for:
///   - Cache invalidation (same hash = same request)
///   - Audit trail (what did the model actually see?)
///   - Compaction validation (is the rebuilt context equivalent?)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSnapshot {
    /// Unique snapshot identifier.
    pub snapshot_id: String,
    /// SHA-256 hash of (items + tool_schemas).
    pub content_hash: String,
    /// Estimated token count at snapshot time.
    pub token_count: u64,
    /// Tokenizer identifier used for the estimate.
    pub tokenizer_id: Option<String>,
    /// Tokenizer version.
    pub tokenizer_version: Option<String>,
    /// The context items sent to the model.
    pub items: Vec<ContextItem>,
    /// Tool schemas advertised to the model.
    pub tool_schemas: Vec<ToolSpec>,
    /// When this snapshot was captured.
    pub created_at: DateTime<Utc>,
    /// Session identifier this snapshot belongs to.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Turn identifier at the time of snapshot.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Step identifier at the time of snapshot.
    #[serde(default)]
    pub step_id: Option<String>,
    /// Version of the context history at snapshot time.
    #[serde(default)]
    pub context_history_version: Option<u64>,
    /// Identifier of the capability snapshot used.
    #[serde(default)]
    pub capability_snapshot_id: Option<String>,
    /// Identifier of the memory snapshot used.
    #[serde(default)]
    pub memory_snapshot_id: Option<String>,
}

impl PromptSnapshot {
    /// Build a snapshot from the request context.
    pub fn capture(
        items: &[ContextItem],
        tool_schemas: &[ToolSpec],
    ) -> Self {
        use sha2::{Digest, Sha256};

        let token_count: u64 = items.iter().map(|i| i.estimated_tokens() as u64).sum();
        let tool_tokens: u64 = tool_schemas
            .iter()
            .map(|t| t.parameters.to_string().len() as u64 / 4)
            .sum();

        let mut hasher = Sha256::new();
        for item in items {
            hasher.update(serde_json::to_string(item).unwrap_or_default());
        }
        for tool in tool_schemas {
            hasher.update(tool.name.as_bytes());
            hasher.update(tool.parameters.to_string().as_bytes());
        }
        let content_hash = format!("{:x}", hasher.finalize());

        Self {
            snapshot_id: format!("snap_{}", content_hash.chars().take(12).collect::<String>()),
            content_hash,
            token_count: token_count + tool_tokens,
            tokenizer_id: None,
            tokenizer_version: None,
            items: items.to_vec(),
            tool_schemas: tool_schemas.to_vec(),
            created_at: Utc::now(),
            session_id: None,
            turn_id: None,
            step_id: None,
            context_history_version: None,
            capability_snapshot_id: None,
            memory_snapshot_id: None,
        }
    }

    /// Whether this snapshot's content matches another (for compaction equivalence check).
    pub fn content_matches(&self, other: &PromptSnapshot) -> bool {
        self.content_hash == other.content_hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_with_same_input_have_same_hash() {
        let items = vec![ContextItem::User { content: "hello".into(), message_id: None }];
        let tools = vec![];
        let snap1 = PromptSnapshot::capture(&items, &tools);
        let snap2 = PromptSnapshot::capture(&items, &tools);
        assert_eq!(snap1.content_hash, snap2.content_hash);
    }

    #[test]
    fn different_inputs_have_different_hash() {
        let items1 = vec![ContextItem::User { content: "hello".into(), message_id: None }];
        let items2 = vec![ContextItem::User { content: "world".into(), message_id: None }];
        let snap1 = PromptSnapshot::capture(&items1, &[]);
        let snap2 = PromptSnapshot::capture(&items2, &[]);
        assert_ne!(snap1.content_hash, snap2.content_hash);
    }
}
