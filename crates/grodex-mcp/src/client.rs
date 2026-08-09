//! McpClient — manages an MCP server connection and provides tool access.

use crate::server::McpServerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// An MCP tool as seen by the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub server_name: String,
}

/// Status of an MCP server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// Client for interacting with MCP servers.
///
/// Phase 1: register server configs, list expected tools based on config.
/// Phase 2+: actual JSON-RPC process communication.
#[derive(Debug)]
pub struct McpClient {
    servers: HashMap<String, McpServerConfig>,
    #[allow(dead_code)]
    status: HashMap<String, McpConnectionStatus>,
}

impl McpClient {
    /// Create a new MCP client.
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
            status: HashMap::new(),
        }
    }

    /// Register a server configuration.
    pub fn register_server(&mut self, config: McpServerConfig) {
        let name = config.name.clone();
        self.status.insert(name.clone(), McpConnectionStatus::Disconnected);
        self.servers.insert(name, config);
    }

    /// List all registered servers.
    pub fn servers(&self) -> Vec<&McpServerConfig> {
        self.servers.values().collect()
    }

    /// Get a server config by name.
    pub fn get_server(&self, name: &str) -> Option<&McpServerConfig> {
        self.servers.get(name)
    }

    /// List tools by spawning a process and calling tools/list via JSON-RPC.
    pub async fn list_tools_async(&mut self, server_name: &str) -> Result<Vec<McpTool>, String> {
        let config = self.servers.get(server_name).ok_or("server not found")?.clone();
        let mut process = crate::process::McpProcess::spawn(config).await?;
        process.list_tools().await
    }

    /// Whether any servers are configured.
    pub fn has_servers(&self) -> bool {
        !self.servers.is_empty()
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_list_servers() {
        let mut client = McpClient::new();
        client.register_server(McpServerConfig::new("filesystem", "mcp-server-filesystem"));
        client.register_server(McpServerConfig::new("github", "mcp-server-github"));

        assert!(client.has_servers());
        assert_eq!(client.servers().len(), 2);
        assert!(client.get_server("github").is_some());
    }

    #[test]
    fn empty_client() {
        let client = McpClient::new();
        assert!(!client.has_servers());
    }
}
