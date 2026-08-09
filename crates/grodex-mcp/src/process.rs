//! MCP process manager — spawns MCP server processes and communicates via JSON-RPC over stdio.
//!
//! Following the MCP specification: each server is a subprocess that
//! communicates via stdin/stdout JSON-RPC. The client sends `tools/list`
//! to discover tools and `tools/call` to invoke them.

use crate::server::McpServerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// A connected MCP server process.
pub struct McpProcess {
    config: McpServerConfig,
    child: Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

/// A JSON-RPC request sent to the MCP server.
#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: serde_json::Value,
}

/// A JSON-RPC response from the MCP server.
#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[allow(dead_code)]
    id: Option<u64>,
    result: Option<serde_json::Value>,
    #[allow(dead_code)]
    error: Option<serde_json::Value>,
}

impl McpProcess {
    /// Spawn an MCP server process and initialize the connection.
    pub async fn spawn(config: McpServerConfig) -> Result<Self, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);

        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().map_err(|e| format!("cannot spawn {}: {e}", config.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let reader = BufReader::new(stdout);

        Ok(Self {
            config,
            child,
            stdin,
            reader,
            next_id: 1,
        })
    }

    /// Send a JSON-RPC request and wait for the response.
    async fn rpc_call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = self.next_id;
        self.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        };

        let mut json = serde_json::to_string(&request).map_err(|e| format!("serialize: {e}"))?;
        json.push('\n');
        self.stdin.write_all(json.as_bytes()).await.map_err(|e| format!("write: {e}"))?;

        // Read response.
        let mut line = String::new();
        self.reader.read_line(&mut line).await.map_err(|e| format!("read: {e}"))?;

        let response: JsonRpcResponse = serde_json::from_str(&line).map_err(|e| format!("parse: {e}"))?;
        response.result.ok_or_else(|| format!("rpc error: {line}"))
    }

    /// List tools from the MCP server.
    pub async fn list_tools(&mut self) -> Result<Vec<crate::client::McpTool>, String> {
        let result = self.rpc_call("tools/list", serde_json::json!({})).await?;
        let tools: Vec<crate::client::McpTool> = result
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(crate::client::McpTool {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t.get("description")?.as_str().unwrap_or("").to_string(),
                            input_schema: t.get("inputSchema")?.clone(),
                            server_name: self.config.name.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tools)
    }

    /// Call a tool on the MCP server.
    pub async fn call_tool(&mut self, tool_name: &str, arguments: serde_json::Value) -> Result<serde_json::Value, String> {
        self.rpc_call(
            "tools/call",
            serde_json::json!({"name": tool_name, "arguments": arguments}),
        )
        .await
    }

    /// Get the server config.
    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// Check if the process is still running.
    pub async fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        // Child is kill_on_drop, so it will be terminated.
    }
}

/// Manages multiple MCP server connections.
pub struct McpProcessManager {
    processes: HashMap<String, McpProcess>,
}

impl Default for McpProcessManager {
    fn default() -> Self { Self::new() }
}

impl McpProcessManager {
    pub fn new() -> Self {
        Self { processes: HashMap::new() }
    }

    /// Connect to an MCP server.
    pub async fn connect(&mut self, config: McpServerConfig) -> Result<&McpProcess, String> {
        let name = config.name.clone();
        let process = McpProcess::spawn(config).await?;
        self.processes.insert(name.clone(), process);
        Ok(&self.processes[&name]) // re-borrow to satisfy borrow checker
    }

    /// Get a connected process by name.
    pub fn get(&self, name: &str) -> Option<&McpProcess> {
        self.processes.get(name)
    }

    /// Get a mutable reference.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut McpProcess> {
        self.processes.get_mut(name)
    }

    /// Disconnect from a server.
    pub fn disconnect(&mut self, name: &str) {
        self.processes.remove(name);
    }

    /// Number of connected servers.
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}
