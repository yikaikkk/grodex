//! ModelBinding — the frozen, immutable snapshot of provider + model + codec.
//!
//! Created once at the start of a Sampling Step. Never mutated. A new
//! Step with a different provider or model MUST create a new ModelBinding.
//! This is the key abstraction from Design Doc 14, Section 8.3.

use crate::descriptor::WireProtocol;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Strongly-typed identifier for a ModelBinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelBindingId(Uuid);

impl ModelBindingId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn from_string(s: &str) -> Result<Self, grodex_core::error::GrodexError> {
        Uuid::parse_str(s)
            .map(Self)
            .map_err(|_| grodex_core::error::GrodexError::InvalidId(s.to_string()))
    }
}

impl std::fmt::Display for ModelBindingId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for ModelBindingId {
    fn default() -> Self {
        Self::new()
    }
}

/// How reasoning/thinking content is handled for this binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningPolicy {
    /// Reasoning is visible to the user and transcript.
    Visible,
    /// Reasoning is hidden (encrypted/opaque envelope).
    Hidden,
    /// Model does not emit reasoning content.
    None,
}

/// Immutable snapshot binding together provider, model, codec, and tokenizer
/// for the duration of one Sampling Step.
///
/// This is a frozen value — once `created_at` is set, no field may change.
/// Auth credential rotations replace only the `credential_lease_id`, not the
/// binding itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelBinding {
    /// Unique identifier for this binding.
    pub binding_id: ModelBindingId,
    /// Which provider serves the model.
    pub provider_id: String,
    /// Revision of the ProviderDescriptor at binding time.
    pub provider_revision: u64,
    /// Which model is being called.
    pub model_id: String,
    /// Revision of the ModelDescriptor at binding time.
    pub model_revision: u64,
    /// Which wire protocol to use.
    pub wire_protocol: WireProtocol,
    /// Revision of the wire encoder used.
    pub encoder_revision: u64,
    /// Revision of the streaming decoder used.
    pub decoder_revision: u64,
    /// Tokenizer identifier (if known).
    pub tokenizer_id: Option<String>,
    /// Tokenizer version (if known).
    pub tokenizer_version: Option<String>,
    /// Opaque credential lease id.
    pub credential_lease_id: Option<String>,
    /// How reasoning content is handled.
    pub reasoning_policy: ReasoningPolicy,
    /// When this binding was created.
    pub created_at: DateTime<Utc>,
}

impl ModelBinding {
    /// Create a new ModelBinding for the given provider, model, and wire protocol.
    pub fn new(
        provider_id: String,
        provider_revision: u64,
        model_id: String,
        model_revision: u64,
        wire_protocol: WireProtocol,
    ) -> Self {
        Self {
            binding_id: ModelBindingId::new(),
            provider_id,
            provider_revision,
            model_id,
            model_revision,
            wire_protocol,
            encoder_revision: 1,
            decoder_revision: 1,
            tokenizer_id: None,
            tokenizer_version: None,
            credential_lease_id: None,
            reasoning_policy: ReasoningPolicy::None,
            created_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_id_unique() {
        let a = ModelBindingId::new();
        let b = ModelBindingId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn binding_id_roundtrip() {
        let id = ModelBindingId::new();
        let json = serde_json::to_string(&id).unwrap();
        let id2: ModelBindingId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, id2);
    }
}
