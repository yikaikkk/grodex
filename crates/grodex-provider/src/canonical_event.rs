//! CanonicalModelEvent and CanonicalModelResponse — the unified event surface.
//!
//! These types are what the Agent Loop consumes. Every wire backend (Responses,
//! Chat Completions, Messages) normalizes its raw events into this format.
//! The Agent Loop must NEVER branch on provider or wire protocol type.

use crate::error::ProviderError;
use crate::usage::SettledUsage;
use grodex_core::id::ToolCallId;
use serde::{Deserialize, Serialize};

// ── Streaming events ──────────────────────────────────────────────

/// Opaque provider reasoning envelope. Carries hidden chain-of-thought
/// payload that must not enter the visible transcript. Managed by the
/// provider adapter, never exposed directly to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderReasoningEnvelope {
    pub provider_family: String,
    pub model_family: String,
    pub envelope_kind: String,
    /// Opaque payload reference (encrypted or stored separately).
    pub opaque_payload_ref: Option<String>,
    /// Visible summary (if allowed by retention policy).
    pub visible_summary: Option<String>,
    /// Compatibility tag for model switching.
    pub compatibility_tag: String,
    /// Size of the opaque payload in bytes.
    #[serde(default)]
    pub payload_size_bytes: u64,
    /// Retention policy applied to this envelope ("discard", "summary_only", "full").
    #[serde(default)]
    pub retention_policy: String,
    /// Timestamp when the envelope was created.
    #[serde(default)]
    pub created_at_ms: i64,
}

/// All events emitted during a streaming model response.
#[derive(Debug, Clone)]
pub enum CanonicalModelEvent {
    /// HTTP stream established. Always the first event. Used for TTFB measurement.
    StreamStarted {
        request_id: String,
        /// Monotonic timestamp in milliseconds.
        timestamp_ms: i64,
    },

    /// Provider response metadata (model name, context window size).
    ResponseMetadata { model: String, context_window: Option<u64> },

    /// Incremental text content from the assistant.
    TextDelta {
        /// The new text fragment (rarely empty; empty deltas should be filtered).
        text: String,
        /// Monotonically increasing chunk index across text+reasoning.
        chunk_index: u64,
    },

    /// Incremental reasoning/thinking content.
    ReasoningDelta { text: String, chunk_index: u64 },

    /// A reasoning/thinking envelope completed.
    ReasoningEnvelopeCompleted {
        /// Optional signature for the thinking block.
        signature: Option<String>,
    },

    /// The start of a new tool call. Carries the Grodex-assigned ToolCallId.
    ToolCallStarted {
        call_id: ToolCallId,
        /// The tool name as reported by the model.
        name: String,
        /// Zero-based tool index within this response.
        tool_index: u32,
    },

    /// Incremental tool call arguments. NOT necessarily valid JSON in isolation.
    ToolCallArgumentsDelta {
        call_id: ToolCallId,
        tool_index: u32,
        /// A fragment of the JSON arguments string.
        arguments_delta: String,
    },

    /// A tool call has completed streaming its arguments.
    ToolCallCompleted {
        call_id: ToolCallId,
        tool_index: u32,
        /// The accumulated, complete JSON arguments string.
        arguments: String,
    },

    /// Provider usage update (may appear multiple times as cumulative values).
    UsageDelta { input_tokens: u64, output_tokens: u64 },

    /// Rate limit information from the provider.
    RateLimitUpdated {
        remaining_requests: Option<u64>,
        remaining_tokens: Option<u64>,
        reset_after_secs: Option<u64>,
    },

    /// The response completed successfully. EXACTLY ONE per request.
    ResponseCompleted(CanonicalModelResponse),

    /// The response failed. EXACTLY ONE per request.
    ResponseFailed(ProviderError),
}

// ── Response types ─────────────────────────────────────────────────

/// The complete, assembled response after streaming finishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalModelResponse {
    pub request_id: String,
    pub items: Vec<CanonicalResponseItem>,
    pub stop_reason: Option<StopReason>,
    pub usage: SettledUsage,
}

/// One item produced in a model response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CanonicalResponseItem {
    /// Text content from the assistant.
    AssistantText { content: String },
    /// A completed tool call.
    ToolCall {
        call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    /// Visible reasoning/thinking summary.
    ReasoningSummary { content: String },
    /// The model refused to respond.
    Refusal { content: String },
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// Natural stop (end_turn, stop_sequence).
    Stop,
    /// Max output tokens reached.
    Length,
    /// Model generated tool calls.
    ToolCalls,
    /// Content filtered by the provider.
    ContentFilter,
}

// ── Conversion helpers ─────────────────────────────────────────────

impl CanonicalModelResponse {
    /// Extract the assistant text content, if any.
    pub fn assistant_text(&self) -> Option<&str> {
        for item in &self.items {
            if let CanonicalResponseItem::AssistantText { content } = item {
                return Some(content.as_str());
            }
        }
        None
    }

    /// Extract all tool calls from the response.
    pub fn tool_calls(&self) -> Vec<&CanonicalResponseItem> {
        self.items
            .iter()
            .filter(|i| matches!(i, CanonicalResponseItem::ToolCall { .. }))
            .collect()
    }
}
