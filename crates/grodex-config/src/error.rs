//! ConfigError — unified error type for config loading, parsing, and merging.

use crate::values::ConfigDiagnostic;
use std::path::PathBuf;

/// Errors that can occur during config loading and merging.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse {path}: {error}")]
    Parse { path: PathBuf, error: String },

    #[error("merge error: {message}")]
    Merge { message: String },

    #[error("validation failed")]
    Validation { diagnostics: Vec<ConfigDiagnostic> },

    #[error("requirement constraint violated: {message}")]
    RequirementViolation { message: String },

    #[error("fs watcher error: {message}")]
    Watch { message: String },
}
