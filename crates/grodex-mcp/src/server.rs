//! McpServerConfig — configuration for connecting to an MCP server.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// How to connect to an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique server name.
    pub name: String,
    /// Command to spawn the server process.
    pub command: String,
    /// Arguments for the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables for the server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Whether this server is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional OAuth client settings. When present, the server requires an
    /// OAuth authorization flow before use; the runtime registers this
    /// config with the MCP OAuth coordinator (see `crate::oauth`).
    #[serde(default)]
    pub oauth: Option<grodex_auth::OAuthClientConfig>,
}

fn default_true() -> bool {
    true
}

impl McpServerConfig {
    /// Create a new server config.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: HashMap::new(),
            enabled: true,
            oauth: None,
        }
    }

    /// Whether this server requires an OAuth authorization flow.
    pub fn requires_oauth(&self) -> bool {
        self.oauth.is_some()
    }
}
