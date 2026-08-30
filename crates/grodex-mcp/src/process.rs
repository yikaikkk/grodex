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

/// Per-call JSON-RPC timeout — the one unguarded await class in the
/// runtime before this: a hung MCP server used to block tools/call forever.
const RPC_TIMEOUT_SECS: u64 = 60;

/// A connected MCP server process.
pub struct McpProcess {
    config: McpServerConfig,
    child: Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
    /// Responses that arrived out of order — buffered so a later caller
    /// finds them (arrival-order matching desyncs on servers that emit
    /// notifications between request and response).
    pending: HashMap<u64, JsonRpcResponse>,
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
#[derive(Debug, Clone, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<u64>,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
}

impl McpProcess {
    /// Spawn an MCP server process and initialize the connection.
    pub async fn spawn(config: McpServerConfig) -> Result<Self, String> {
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        // stderr 不继承 TTY：如果 MCP server 持有 TTY fd 且主进程
        // 异常退出（信号/崩溃），子进程会变成孤儿继续持有 TTY fd，
        // 导致终端无法释放。piped 后丢弃即可，MCP server 日志不重要。
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().map_err(|e| format!("cannot spawn {}: {e}", config.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let reader = BufReader::new(stdout);
        // Drain stderr in the background — a chatty server that fills the
        // pipe would otherwise deadlock itself, and its logs end up in
        // tracing instead of an unread pipe buffer.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => tracing::debug!(target: "grodex_mcp_stderr", "{line}"),
                        _ => break,
                    }
                }
            });
        }

        Ok(Self {
            config,
            child,
            stdin,
            reader,
            next_id: 1,
            pending: HashMap::new(),
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

        // Read until the response with OUR id arrives, buffering any
        // out-of-order responses (notifications and interleaved replies
        // are skipped/buffered — arrival-order matching desyncs on servers
        // that emit notifications between request and response).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(RPC_TIMEOUT_SECS);
        loop {
            // Drain the out-of-order buffer FIRST — a response that arrived
            // early (before its caller was waiting) must not leave us
            // stalling on the wire for a reply that will never come.
            if let Some(response) = self.pending.remove(&id) {
                if let Some(err) = &response.error {
                    return Err(format!("rpc error: {err}"));
                }
                return response
                    .result
                    .ok_or_else(|| format!("rpc {method}: empty result"));
            }
            let mut line = String::new();
            match tokio::time::timeout_at(deadline, self.reader.read_line(&mut line)).await {
                Err(_) => return Err(format!("rpc {method} timed out after {RPC_TIMEOUT_SECS}s")),
                Ok(Err(e)) => return Err(format!("read: {e}")),
                Ok(Ok(0)) => return Err(format!("rpc {method}: server closed stdout")),
                Ok(Ok(_)) => {}
            }
            let response: JsonRpcResponse = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(_) => continue, // not a response line (log line etc.)
            };
            match response.id {
                Some(rid) if rid == id => {
                    if let Some(err) = &response.error {
                        return Err(format!("rpc error: {err}"));
                    }
                    return response
                        .result
                        .ok_or_else(|| format!("rpc {method}: empty result"));
                }
                Some(rid) => {
                    self.pending.insert(rid, response);
                }
                None => continue, // notification
            }
        }
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
        // stderr 已改为 piped，子进程不再持有 TTY fd。
        // kill_on_drop(true) 会在 Child drop 时发送 SIGKILL。
        // 这里额外尝试 start_kill + 阻塞 wait 确保子进程退出，
        // 防止僵尸进程残留（tokio 的 kill_on_drop 不保证 wait）。
        let _ = self.child.start_kill();
        // 阻塞等待退出（Drop 中不能 await，用 try_wait 轮询）
        for _ in 0..100 {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(_) => break,
            }
        }
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
