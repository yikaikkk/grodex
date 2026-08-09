//! PendingToolCall — accumulator for in-flight streaming tool calls.
//!
//! Following Grok's pattern: each tool call gets a stable index (tool_index)
//! and accumulates name + arguments from deltas. Arguments are only considered
//! complete when the tool call end event is received.

use grodex_core::id::ToolCallId;

/// Accumulator for a tool call that is streaming in.
#[derive(Debug, Clone)]
pub struct PendingToolCall {
    /// Stable key from the provider (e.g. Responses output_index).
    pub provider_item_key: String,
    /// Grodex-assigned ToolCallId.
    pub canonical_tool_call_id: ToolCallId,
    /// Accumulated tool name.
    pub name_buffer: String,
    /// Accumulated JSON arguments string.
    pub args_buffer: String,
    /// Whether the tool call has completed (end event received).
    pub completed: bool,
}

impl PendingToolCall {
    /// Create a new pending tool call with the given provider key and initial name.
    pub fn new(provider_item_key: String, name: String) -> Self {
        Self {
            provider_item_key,
            canonical_tool_call_id: ToolCallId::new(),
            name_buffer: name,
            args_buffer: String::new(),
            completed: false,
        }
    }

    /// Append an arguments delta fragment.
    pub fn append_args(&mut self, delta: &str) {
        self.args_buffer.push_str(delta);
    }

    /// Mark this tool call as completed.
    pub fn mark_completed(&mut self) {
        self.completed = true;
    }

    /// Check whether the accumulated arguments parse as valid JSON.
    /// Returns false for partial/incomplete JSON.
    pub fn is_valid_json(&self) -> bool {
        serde_json::from_str::<serde::de::IgnoredAny>(&self.args_buffer).is_ok()
    }

    /// Try to parse the accumulated arguments into a serde_json::Value.
    pub fn parse_args(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::from_str(&self.args_buffer)
    }
}
