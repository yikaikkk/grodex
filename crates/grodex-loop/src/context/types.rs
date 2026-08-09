//! Compaction types — plans, results, and the CompactionItem trait.
//!
//! Following Grok's `CompactionItem` pattern: a read-only view of context
//! items that the compaction engine works with, without coupling to the
//! full ContextItem enum.

use grodex_core::context::ContextItem;

/// A plan describing what to compact and what to keep.
#[derive(Debug, Clone)]
pub struct CompactionPlan {
    /// Items that will be summarized (oldest, exceeding budget).
    pub items_to_compact: Vec<ContextItem>,
    /// Items that will be kept verbatim (most recent).
    pub items_to_keep: Vec<ContextItem>,
    /// Estimated tokens before compaction.
    pub estimated_tokens_before: u64,
    /// Estimated tokens in items_to_compact.
    pub compact_tokens: u64,
    /// Estimated tokens in items_to_keep.
    pub keep_tokens: u64,
}

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The generated summary text.
    pub summary: String,
    /// Token count before compaction.
    pub tokens_before: u64,
    /// Token count after compaction (summary + kept items).
    pub tokens_after: u64,
    /// Reduction ratio: tokens_after / tokens_before.
    /// 0.5 means 50% reduction. Values near 1.0 mean compaction was ineffective.
    pub reduction_ratio: f64,
}

impl CompactionResult {
    /// Whether compaction achieved meaningful reduction.
    /// Following Grok: must achieve at least 20% reduction to commit.
    pub fn is_effective(&self) -> bool {
        self.reduction_ratio < 0.8
    }
}

/// Trait for providing a read-only view of a context item for compaction.
///
/// Following Grok's `CompactionItem` trait: the compaction engine works
/// through this interface, so it never depends on the concrete ContextItem type.
pub trait CompactionItemView {
    /// Human-readable role label.
    fn role_label(&self) -> &str;
    /// The text content visible to the compaction model.
    fn visible_text(&self) -> &str;
    /// Whether this item is a compaction summary (from a prior compaction).
    fn is_compaction_summary(&self) -> bool;
    /// Whether this item is a tool call.
    fn is_tool_call(&self) -> bool;
    /// Whether this item is a tool result.
    fn is_tool_result(&self) -> bool;
    /// Estimated token count.
    fn token_count(&self) -> u64;
}

impl CompactionItemView for ContextItem {
    fn role_label(&self) -> &str {
        match self {
            ContextItem::System { .. } => "system",
            ContextItem::Developer { .. } => "developer",
            ContextItem::User { .. } => "user",
            ContextItem::Assistant { .. } => "assistant",
            ContextItem::ToolCall { .. } => "tool_call",
            ContextItem::ToolResult { .. } => "tool_result",
            ContextItem::CompactionSummary { .. } => "compaction_summary",
            ContextItem::ReasoningSummary { .. } => "reasoning",
            ContextItem::ImagePlaceholder { .. } => "image",
        }
    }

    fn visible_text(&self) -> &str {
        match self {
            ContextItem::System { content }
            | ContextItem::Developer { content }
            | ContextItem::User { content, .. }
            | ContextItem::Assistant { content } => content.as_str(),
            ContextItem::ToolCall { .. } => {
                ""
            }
            ContextItem::ToolResult { content, .. } => content.as_str(),
            ContextItem::CompactionSummary { summary, .. } => summary.as_str(),
            ContextItem::ReasoningSummary { content } => content.as_str(),
            ContextItem::ImagePlaceholder { .. } => "[image]",
        }
    }

    fn is_compaction_summary(&self) -> bool {
        matches!(self, ContextItem::CompactionSummary { .. })
    }

    fn is_tool_call(&self) -> bool {
        matches!(self, ContextItem::ToolCall { .. })
    }

    fn is_tool_result(&self) -> bool {
        matches!(self, ContextItem::ToolResult { .. })
    }

    fn token_count(&self) -> u64 {
        let chars = match self {
            ContextItem::System { content }
            | ContextItem::Developer { content }
            | ContextItem::User { content, .. }
            | ContextItem::Assistant { content }
            | ContextItem::ToolResult { content, .. } => content.len(),
            ContextItem::ToolCall { .. } => {
                20 // rough estimate for tool call overhead
            }
            ContextItem::CompactionSummary { summary, .. } => summary.len(),
            ContextItem::ReasoningSummary { content } => content.len(),
            ContextItem::ImagePlaceholder { .. } => 85, // rough image token estimate in chars
        };
        (chars as u64).div_ceil(4)
    }
}

impl CompactionPlan {
    /// Build a formatted text representation of items to compact.
    /// This is what gets fed into the compaction prompt.
    pub fn format_for_prompt(&self) -> String {
        let mut out = String::new();
        for item in &self.items_to_compact {
            let role = item.role_label();
            let text = match item {
                ContextItem::ToolCall { name, arguments, .. } => {
                    format!("Tool Call: {name}({arguments})")
                }
                _ => item.visible_text().to_string(),
            };
            if !text.is_empty() {
                out.push_str(&format!("[{role}]: {text}\n\n"));
            }
        }
        out
    }
}
