//! CanonicalModelRequest — the Grodex-internal model request type.
//!
//! This is NEVER a raw provider type. The Agent Loop constructs this, and
//! wire-specific adapters map it to their native format.

use crate::binding::ModelBindingId;
use grodex_core::context::ContextItem;
use grodex_core::id::{SessionId, StepId, TurnId};
use serde::{Deserialize, Serialize};

/// Typed instruction block with role and priority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionBlock {
    pub role: InstructionRole,
    pub content: String,
    /// Higher = higher priority. Used for ordering instructions in the prompt.
    pub priority: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstructionRole {
    System,
    Developer,
}

/// Tool specification sent to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
    /// Required parameter names.
    pub required: Vec<String>,
}

/// Controls how the model chooses tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolChoice {
    /// Model may call any tool or none.
    Auto,
    /// Model must call a specific tool.
    Required { name: String },
    /// Model must NOT call any tools.
    None,
}

/// Request for reasoning/thinking output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningRequest {
    pub effort: Option<String>,
    pub summary: Option<String>,
}

/// Request for structured output format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFormat {
    /// JSON Schema for the expected output format.
    pub json_schema: Option<serde_json::Value>,
}

/// The Grodex-internal canonical model request.
///
/// Constructed by the Agent Loop before each sampling Step. Wire-specific
/// adapters map this to their native API format. This type must never contain
/// provider-specific fields — those go in `provider_state_in`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalModelRequest {
    /// Unique request identifier for tracing.
    pub request_id: String,
    /// The session making the request.
    pub session_id: SessionId,
    /// The current Turn.
    pub turn_id: TurnId,
    /// The current Step.
    pub step_id: StepId,
    /// The frozen ModelBinding for this Step.
    pub model_binding_id: ModelBindingId,
    /// SHA-256 hash of the prompt content. Used for caching and equivalence checks.
    pub prompt_snapshot_hash: Option<String>,
    /// High-priority instructions (base instructions, managed policy).
    pub instructions: Vec<InstructionBlock>,
    /// The conversation context to send.
    pub context_items: Vec<ContextItem>,
    /// Tools available to the model.
    pub tool_specs: Vec<ToolSpec>,
    /// How the model may use tools.
    pub tool_choice: ToolChoice,
    /// Whether parallel tool calls are allowed.
    pub parallel_tool_calls: bool,
    /// Optional reasoning request.
    pub reasoning_request: Option<ReasoningRequest>,
    /// Optional structured output format.
    pub response_format: Option<ResponseFormat>,
    /// Maximum output tokens (None = model default).
    pub max_output_tokens: Option<u64>,
    /// Opaque provider state envelope. Only readable by the same provider family.
    /// Carries sticky routing tokens, continuation IDs, etc.
    pub provider_state_in: Option<serde_json::Value>,
}
