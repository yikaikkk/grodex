//! Grodex Provider — Provider/Model adapter types.
//!
//! This crate defines the canonical model interaction types (request, event,
//! response, usage) and the Provider/Model descriptor + binding model.
//! It has zero runtime dependencies — just pure data types for the Agent Loop
//! and Sampler to share.

pub mod binding;
pub mod canonical_event;
pub mod canonical_request;
pub mod codec;
pub mod descriptor;
pub mod error;
pub mod lossiness;
pub mod prompt_snapshot;
pub mod switch;
pub mod usage;

// Re-export key types for convenience.
pub use binding::ModelBinding;
pub use binding::ModelBindingId;
pub use canonical_event::CanonicalModelEvent;
pub use canonical_event::CanonicalModelResponse;
pub use canonical_event::CanonicalResponseItem;
pub use canonical_event::StopReason;
pub use canonical_request::CanonicalModelRequest;
pub use canonical_request::InstructionBlock;
pub use canonical_request::InstructionRole;
pub use canonical_request::ToolChoice;
pub use canonical_request::ToolSpec;
pub use descriptor::ModelDescriptor;
pub use descriptor::ProviderDescriptor;
pub use descriptor::WireProtocol;
pub use error::ProviderError;
pub use lossiness::{LossinessClass, LossinessGate, LossinessManifest, ModelCapabilityDegradedEvent};
pub use usage::EstimatedUsage;
pub use usage::SettledUsage;
pub use usage::TokenUsage;
pub use usage::UsageRecord;
