//! Configuration layers — each represents one source of config values.

use serde::{Deserialize, Serialize};

/// Where a configuration layer originated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConfigLayerSource {
    /// Hard-coded defaults in the binary.
    Builtin,
    /// System-level configuration (e.g. `/etc/grodex/config.toml`).
    System,
    /// Enterprise-managed configuration (IT-enforced, non-overridable constraints).
    EnterpriseManaged,
    /// User-level configuration (`~/.grodex/config.toml`).
    User,
    /// Named profile within user config.
    Profile(String),
    /// Project/workspace configuration (`.grodex/config.toml`).
    Workspace {
        /// Whether the workspace has been explicitly trusted by the user.
        trusted: bool,
    },
    /// One-off flags set for this session only.
    SessionFlag,
}

/// One layer in the configuration stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigLayer {
    /// Where this layer came from.
    pub source: ConfigLayerSource,
    /// The raw TOML values provided by this layer.
    pub values: toml::Value,
    /// Content hash for change detection.
    pub fingerprint: String,
    /// If set, this layer was disabled and the reason why.
    pub disabled_reason: Option<String>,
}

/// Tracks which layer provided the winning value for a config key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeTrace {
    /// The layer that contributed this value.
    pub origin: ConfigLayerSource,
    /// Version / fingerprint of the source at merge time.
    pub source_version: String,
}

impl MergeTrace {
    pub fn new(origin: ConfigLayerSource, source_version: impl Into<String>) -> Self {
        Self {
            origin,
            source_version: source_version.into(),
        }
    }
}
