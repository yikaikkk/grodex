//! McpToolAdapter — wraps an MCP server tool as a `Tool` + `ToolRuntime`.
//!
//! Each adapter holds the server config + tool metadata. On `execute()`,
//! it spawns the MCP server process (if not already running), sends a
//! `tools/call` JSON-RPC request, and returns the result.
//!
//! Tool names are namespaced as `mcp_{server}_{tool}` to avoid collisions
//! with built-in tools.

use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, Tool, ToolMetadata, ToolRuntime};

use crate::process::McpProcess;
use crate::server::McpServerConfig;

/// Adapter that wraps an MCP tool as a Grodex `Tool`.
pub struct McpToolAdapter {
    server_config: McpServerConfig,
    tool_name: String,
    tool_description: String,
    input_schema: serde_json::Value,
    /// Full namespaced name: `mcp_{server}_{tool}`.
    full_name: String,
}

impl McpToolAdapter {
    /// Create a new adapter for a specific MCP tool on a specific server.
    pub fn new(
        server_config: McpServerConfig,
        tool_name: impl Into<String>,
        tool_description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        let tool_name = tool_name.into();
        let full_name = format!("mcp_{}_{}", server_config.name, tool_name);
        Self {
            server_config,
            tool_name,
            tool_description: tool_description.into(),
            input_schema,
            full_name,
        }
    }

    /// The full namespaced tool name.
    pub fn full_name(&self) -> &str {
        &self.full_name
    }
}

impl Tool for McpToolAdapter {
    type Args = serde_json::Value;
    type Output = serde_json::Value;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.full_name.clone(),
            display_name: format!("MCP: {}", self.tool_name),
            description: if self.tool_description.is_empty() {
                format!("MCP tool '{}' on server '{}'", self.tool_name, self.server_config.name)
            } else {
                self.tool_description.clone()
            },
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Ask,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        if self.input_schema.is_null() {
            serde_json::json!({"type": "object", "properties": {}})
        } else {
            self.input_schema.clone()
        }
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "string"})
    }
}

#[async_trait]
impl ToolRuntime for McpToolAdapter {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        // Spawn the MCP server process, call the tool, return the result.
        // The process is killed when McpProcess is dropped (kill_on_drop).
        let mut process = McpProcess::spawn(self.server_config.clone())
            .await
            .map_err(|e| GrodexError::ToolExecution(format!("MCP spawn '{}': {e}", self.server_config.name)))?;

        let result = process
            .call_tool(&self.tool_name, args)
            .await
            .map_err(|e| GrodexError::ToolExecution(format!("MCP call '{}': {e}", self.tool_name)))?;

        Ok(result)
    }
}
