//! The core Tool trait and supporting types.
//!
//! Grodex adopts Grok Build's strongly-typed registration pattern: every
//! Tool declares associated `Args` and `Output` types for compile-time
//! type safety. The `ToolRuntime` trait is separate so that capability
//! management (owning definitions) and execution (owning runtime instances)
//! have different lifetimes.

use crate::id::OperationId;
use crate::policy::PolicyDecision;
use serde::{Deserialize, Serialize};

/// Metadata describing a tool for registration, discovery, and auditing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolMetadata {
    /// Canonical name (e.g. `builtin.read_file`).
    pub name: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Description shown to the model and in capability listings.
    pub description: String,
    /// Whether this tool can run in parallel with others.
    pub concurrency_class: ConcurrencyClass,
    /// Whether repeated calls produce the same result.
    pub side_effect_class: SideEffectClass,
    /// Default permission required when no policy override exists.
    pub default_policy: PolicyDecision,
}

/// Controls whether multiple tool calls can execute concurrently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConcurrencyClass {
    /// Safe to run alongside other parallel-safe tools.
    Parallel,
    /// Must run exclusively; no other tool may execute at the same time.
    Serial,
}

/// Describes idempotency for retry and recovery decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideEffectClass {
    /// No side effects; always safe to retry.
    ReadOnly,
    /// Repeated execution with the same arguments yields the same result.
    Idempotent,
    /// Each execution may produce different results; retry with caution.
    NonIdempotent,
}

/// The central Tool trait.
///
/// Implementors define compile-time `Args` and `Output` types. The trait
/// is object-safe *enough* to store in registries via the schema methods,
/// but consumers that need full type information should use the generic
/// methods on concrete implementations.
pub trait Tool: Send + Sync + 'static {
    /// The deserializable argument type for this tool.
    type Args: serde::de::DeserializeOwned + Send + 'static;
    /// The serializable output type for this tool.
    type Output: serde::Serialize + Send + 'static;

    /// Static metadata about this tool.
    fn metadata(&self) -> ToolMetadata;

    /// JSON Schema describing the expected arguments.
    fn input_schema(&self) -> serde_json::Value;

    /// JSON Schema describing the expected output.
    fn output_schema(&self) -> serde_json::Value;
}

/// The runtime that actually executes a tool.
///
/// Separating `Tool` (definition) from `ToolRuntime` (execution) allows
/// the CapabilityManager to hold tool definitions as long-lived state
/// while execution handles can be created per-call or per-sandbox.
#[async_trait::async_trait]
pub trait ToolRuntime: Send + Sync + 'static {
    /// Execute the tool with pre-validated arguments.
    ///
    /// The `operation_id` is a unique idempotency key that allows
    /// recovery without double-executing side effects.
    async fn execute(
        &self,
        args: serde_json::Value,
        operation_id: OperationId,
    ) -> Result<serde_json::Value, crate::error::GrodexError>;
}
