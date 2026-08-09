//! Provider and model descriptors.
//!
//! Provider = connection config (endpoint, wire protocol, auth strategy).
//! Model = capability declaration (context window, tool support, reasoning).
//! They change at different frequencies and must be versioned independently.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which wire protocol a provider endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WireProtocol {
    /// OpenAI Chat Completions compatible.
    ChatCompletions,
    /// OpenAI Responses API.
    Responses,
    /// Anthropic Messages compatible.
    Messages,
}

/// Transport capabilities of a provider endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportCapabilities {
    pub sse: bool,
    pub websocket: bool,
    pub unary: bool,
}

/// Privacy boundary for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivacyBoundary {
    /// Data may be used for training/improvement.
    Standard,
    /// Data is not used for training (business/API terms).
    NoTraining,
    /// Data stays within a specific region/network.
    DataResidency,
}

/// Connection-level configuration for a provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    /// Unique provider identifier.
    pub provider_id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Base URL for API requests.
    pub endpoint: String,
    /// Which wire protocol this endpoint speaks.
    pub wire_protocol: WireProtocol,
    /// Available transport mechanisms.
    pub transport_capabilities: TransportCapabilities,
    /// Reference to the auth strategy for this provider.
    pub auth_strategy_id: String,
    /// Default headers added to every request (no secrets — values are references).
    pub headers_template: HashMap<String, String>,
    /// Default query parameters added to every request.
    pub query_params: HashMap<String, String>,
    /// Optional retry policy reference.
    pub retry_policy_id: Option<String>,
    /// Privacy boundary.
    pub privacy_boundary: PrivacyBoundary,
    /// Monotonic revision — changes when endpoint, wire, auth strategy, or
    /// behavior-affecting headers change. API key rotations do NOT bump this.
    pub provider_revision: u64,
}

/// Compaction capabilities of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactionCapabilities {
    /// Model does not support compaction.
    None,
    /// Model supports local (client-side) compaction.
    Local,
    /// Model supports remote compaction via a dedicated endpoint.
    Remote,
}

/// Capability declaration for a specific model.
///
/// Fields are conservative by default. Missing fields produce diagnostic
/// warnings and the system assumes the capability is absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDescriptor {
    /// Unique model identifier within the grodex system.
    pub model_id: String,
    /// The provider that serves this model.
    pub provider_id: String,
    /// The model name to send on the wire (e.g. "gpt-5", "claude-opus-4-8").
    pub wire_model_name: String,
    /// Total context window in tokens.
    pub context_window: u64,
    /// Maximum output tokens per request.
    pub max_output_tokens: u64,
    /// Identifier for the tokenizer used by this model.
    pub tokenizer_id: Option<String>,
    /// Version of the tokenizer.
    pub tokenizer_version: Option<String>,
    /// Whether the model supports tool/function calling.
    pub supports_tools: bool,
    /// Whether the model supports parallel tool calls.
    pub supports_parallel_tool_calls: bool,
    /// Whether the model supports reasoning/thinking output.
    pub supports_reasoning: bool,
    /// Supported reasoning modes (e.g. "low", "medium", "high").
    pub reasoning_modes: Vec<String>,
    /// Whether the model supports image inputs.
    pub supports_images: bool,
    /// Whether the model supports prompt caching.
    pub supports_prompt_cache: bool,
    /// Whether the model supports structured/JSON output.
    pub supports_structured_output: bool,
    /// What compaction capabilities the model has.
    pub compaction_capabilities: CompactionCapabilities,
    /// Monotonic revision — changes when model capabilities change.
    pub model_revision: u64,
}
