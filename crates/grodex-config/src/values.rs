//! Effective configuration — the merged result of all layers.

use crate::layer::MergeTrace;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A configuration validation diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigDiagnostic {
    pub level: DiagnosticLevel,
    pub key_path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

/// The fully-merged effective configuration.
///
/// This is the result of merging all config layers in precedence order,
/// then constraining the result by enterprise requirements. Every key
/// carries a `MergeTrace` so the system can answer "why is this value set?"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveConfig {
    /// The resolved values after layer merging.
    pub values: toml::Value,
    /// For each key, which layer provided the winning value.
    pub merge_traces: HashMap<String, MergeTrace>,
    /// Monotonic generation counter for the entire config.
    pub generation: u64,
    /// Diagnostics produced during loading and merging.
    pub diagnostics: Vec<ConfigDiagnostic>,
}

impl EffectiveConfig {
    /// Create an empty effective config at generation 0.
    pub fn empty() -> Self {
        Self {
            values: toml::Value::Table(toml::value::Table::new()),
            merge_traces: HashMap::new(),
            generation: 0,
            diagnostics: Vec::new(),
        }
    }
}
