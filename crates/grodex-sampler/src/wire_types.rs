//! Minimal hand-rolled OpenAI Responses API wire types.
//!
//! These cover only the SSE streaming events needed for Phase 1.
//! We avoid depending on `async-openai` to keep the dependency footprint
//! small and to prevent version conflicts.

use serde::{Deserialize, Serialize};

/// A Response object from the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireResponse {
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub output: Vec<WireOutputItem>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
    #[serde(default)]
    pub model: Option<String>,
}

/// One output item in a response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WireOutputItem {
    #[serde(rename = "message")]
    Message {
        id: String,
        status: Option<String>,
        content: Vec<WireContentBlock>,
        role: Option<String>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        status: Option<String>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Option<Vec<WireSummaryText>>,
        #[serde(default)]
        status: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WireContentBlock {
    #[serde(rename = "output_text")]
    OutputText {
        text: String,
        annotations: Vec<serde_json::Value>,
    },
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(other)]
    Unknown,
}

/// Summary text within a reasoning item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireSummaryText {
    pub text: String,
    #[serde(rename = "type")]
    pub summary_type: String,
}

/// Token usage from the provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens_details: Option<WireInputTokenDetails>,
    #[serde(default)]
    pub output_tokens_details: Option<WireOutputTokenDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireInputTokenDetails {
    #[serde(default)]
    pub cached_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireOutputTokenDetails {
    #[serde(default)]
    pub reasoning_tokens: u64,
}

// ── SSE streaming event types ──────────────────────────────────────

/// A named event from the Responses SSE stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponseStreamEvent {
    /// A new output item is being added.
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        #[serde(default)]
        output_index: u32,
        item: WireOutputItem,
    },
    /// Text content is being streamed.
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        delta: String,
    },
    /// Function call arguments are being streamed.
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta {
        #[serde(rename = "item_id")]
        output_index: u32,
        delta: String,
    },
    /// Reasoning text is being streamed.
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta {
        #[serde(default)]
        output_index: u32,
        delta: String,
    },
    /// Reasoning summary text is being streamed.
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta {
        #[serde(default)]
        output_index: u32,
        delta: String,
    },
    /// The response has completed.
    #[serde(rename = "response.completed")]
    ResponseCompleted { response: WireResponse },
    /// The response failed.
    #[serde(rename = "response.failed")]
    ResponseFailed {
        #[serde(default)]
        error: Option<ResponseError>,
    },
    /// Deprecated/alternative error format.
    #[serde(rename = "error")]
    Error { code: Option<String>, message: String },
    /// Heartbeat / queue event — not content-bearing.
    #[serde(rename = "response.queued")]
    ResponseQueued,
    /// Response is in progress.
    #[serde(rename = "response.in_progress")]
    ResponseInProgress { response: Option<WireResponse> },
    /// Annotations are being streamed.
    #[serde(rename = "response.output_text.annotation.added")]
    AnnotationAdded {
        #[serde(default)]
        output_index: u32,
        #[serde(default)]
        content_index: u32,
        annotation: serde_json::Value,
    },
    /// Catch-all for unknown events.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: String,
}

impl ResponseStreamEvent {
    /// Returns true if this event carries meaningful content.
    /// Heartbeat/queue events are not content.
    pub fn has_meaningful_content(&self) -> bool {
        !matches!(
            self,
            Self::ResponseQueued | Self::ResponseInProgress { .. } | Self::Unknown
        )
    }

    /// Returns true if this is a terminal event.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::ResponseCompleted { .. } | Self::ResponseFailed { .. } | Self::Error { .. }
        )
    }
}
