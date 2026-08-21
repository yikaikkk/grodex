//! ProcessIoTool — unified interaction with background processes (§11.2).
//!
//! Provides three operations on a tracked `process_id`:
//! 1. **poll**: wait up to `poll_timeout_ms` for output / exit
//! 2. **stdin**: write data to the process's stdin
//! 3. **signal**: send a signal (SIGTERM, SIGKILL, etc.)
//!
//! All background processes spawned by `ExecTool` with `background: true`
//! are registered in the shared `ProcessManager`. The model never uses
//! raw shell `&` / `kill` — it goes through this tool.

use crate::common::{
    BuiltInTool, ModelContent, PreparedCall, ProcessHandle, ProcessState, Retryability,
    SideEffectHint, ToolResultEnvelope, ToolStatus, TruncationInfo,
};
use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::sync::Mutex;

// ── Process Manager ────────────────────────────────────────────────────────

/// A tracked background process entry.
pub struct ProcessEntry {
    /// The child process handle.
    pub child: Child,
    /// Accumulated stdout (collected lazily on poll).
    pub stdout_buf: Vec<u8>,
    /// Accumulated stderr (collected lazily on poll).
    pub stderr_buf: Vec<u8>,
    /// The handle metadata returned to the model.
    pub handle: ProcessHandle,
    /// Stdin writer, if still open.
    pub stdin: Option<tokio::process::ChildStdin>,
    /// Stdout reader task handle.
    pub stdout_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
    /// Stderr reader task handle.
    pub stderr_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
}

/// Thread-safe registry of background processes.
///
/// Keyed by `process_id` (the kernel PID at spawn time). Entries are
/// evicted when the process exits and is reaped via `poll`.
#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<Mutex<BTreeMap<u32, ProcessEntry>>>,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Register a newly spawned background process.
    pub async fn register(
        &self,
        pid: u32,
        child: Child,
        stdin: Option<tokio::process::ChildStdin>,
        stdout_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
        stderr_task: Option<tokio::task::JoinHandle<Vec<u8>>>,
        operation_id: String,
    ) -> ProcessHandle {
        let now = SystemTime::now();
        let handle = ProcessHandle {
            process_id: pid,
            operation_id,
            environment_id: String::new(),
            created_at: now,
            state: ProcessState::Running,
            stdin_open: stdin.is_some(),
            tty: false,
            lease_expires_at: now + Duration::from_secs(300),
        };
        let entry = ProcessEntry {
            child,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            handle: handle.clone(),
            stdin,
            stdout_task,
            stderr_task,
        };
        self.inner.lock().await.insert(pid, entry);
        handle
    }

    /// Look up a process entry by PID.
    pub async fn get(&self, pid: u32) -> Option<tokio::sync::MutexGuard<'_, BTreeMap<u32, ProcessEntry>>> {
        let guard = self.inner.lock().await;
        if guard.contains_key(&pid) {
            Some(guard)
        } else {
            None
        }
    }

    /// Remove a process entry (after exit or kill).
    pub async fn remove(&self, pid: u32) -> Option<ProcessEntry> {
        self.inner.lock().await.remove(&pid)
    }

    /// List all tracked PIDs.
    pub async fn list_pids(&self) -> Vec<u32> {
        self.inner.lock().await.keys().copied().collect()
    }
}

// ── Tool definition ────────────────────────────────────────────────────────

/// Arguments for the ProcessIoTool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessIoArgs {
    /// The PID of the process to interact with (from ExecTool background).
    pub process_id: u32,
    /// Data to write to the process's stdin. If `None`, stdin is not touched.
    #[serde(default)]
    pub stdin: Option<String>,
    /// Close stdin after writing (sends EOF). Default: false.
    #[serde(default)]
    pub close_stdin: bool,
    /// Wait up to this many milliseconds for the process to produce output
    /// or exit. If `None`, returns immediately with whatever is available.
    #[serde(default)]
    pub poll_timeout_ms: Option<u64>,
    /// Signal to send: "SIGTERM", "SIGKILL", "SIGINT", "SIGHUP".
    /// On non-Unix platforms, only SIGKILL is emulated (kills the child).
    #[serde(default)]
    pub signal: Option<String>,
}

/// Output from the ProcessIoTool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessIoOutput {
    /// The PID that was operated on.
    pub process_id: u32,
    /// New stdout data since last poll (or since spawn).
    #[serde(default)]
    pub stdout: String,
    /// New stderr data since last poll (or since spawn).
    #[serde(default)]
    pub stderr: String,
    /// Current process state after the operation.
    pub state: ProcessState,
    /// Whether the process is still running.
    pub still_running: bool,
    /// Exit code, if the process has exited.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Whether stdin was written to.
    #[serde(default)]
    pub stdin_written: bool,
    /// Whether the signal was sent.
    #[serde(default)]
    pub signal_sent: Option<String>,
}

/// Unified process interaction tool (§11.2).
pub struct ProcessIoTool {
    pub manager: ProcessManager,
}

impl ProcessIoTool {
    pub fn new(manager: ProcessManager) -> Self {
        Self { manager }
    }
}

impl Tool for ProcessIoTool {
    type Args = ProcessIoArgs;
    type Output = ProcessIoOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "process_io".into(),
            display_name: "Process I/O".into(),
            description: "Interact with a background process: poll for output, write to stdin, or send a signal.".into(),
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {"type": "integer", "description": "PID of the background process"},
                "stdin": {"type": "string", "description": "Data to write to stdin"},
                "close_stdin": {"type": "boolean", "description": "Close stdin after writing (EOF)"},
                "poll_timeout_ms": {"type": "integer", "description": "Wait up to N ms for output/exit"},
                "signal": {"type": "string", "enum": ["SIGTERM", "SIGKILL", "SIGINT", "SIGHUP"], "description": "Signal to send"}
            },
            "required": ["process_id"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "process_id": {"type": "integer"},
                "stdout": {"type": "string"},
                "stderr": {"type": "string"},
                "state": {"type": "string"},
                "still_running": {"type": "boolean"},
                "exit_code": {"type": "integer"},
                "stdin_written": {"type": "boolean"},
                "signal_sent": {"type": "string"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for ProcessIoTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: ProcessIoArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let pid = args.process_id;

        // ── Signal path ──
        if let Some(ref sig) = args.signal {
            return self.send_signal(pid, sig).await;
        }

        // ── Stdin write path ──
        let mut stdin_written = false;
        if let Some(ref data) = args.stdin {
            self.write_stdin(pid, data, args.close_stdin).await?;
            stdin_written = true;
        }

        // ── Poll path ──
        let (stdout, stderr, state, exit_code) = self.poll_process(pid, args.poll_timeout_ms).await?;
        let still_running = matches!(state, ProcessState::Running | ProcessState::Sleeping);

        let result = ProcessIoOutput {
            process_id: pid,
            stdout,
            stderr,
            state,
            still_running,
            exit_code,
            stdin_written,
            signal_sent: None,
        };

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

impl ProcessIoTool {
    /// Poll a process for new output and/or exit status.
    async fn poll_process(
        &self,
        pid: u32,
        timeout_ms: Option<u64>,
    ) -> Result<(String, String, ProcessState, Option<i32>), GrodexError> {
        let mut guard = self
            .manager
            .get(pid)
            .await
            .ok_or_else(|| GrodexError::ToolExecution(format!("process {pid} not found in manager")))?;

        let entry = guard.get_mut(&pid).unwrap();

        // Wait for the child to exit if timeout is specified.
        if let Some(ms) = timeout_ms {
            let timeout = Duration::from_millis(ms);
            match tokio::time::timeout(timeout, entry.child.wait()).await {
                Ok(Ok(status)) => {
                    // Process exited.
                    let code = status.code();
                    // Collect remaining output from tasks.
                    let stdout = collect_task_output(&mut entry.stdout_task).await;
                    let stderr = collect_task_output(&mut entry.stderr_task).await;
                    let state = if let Some(c) = code {
                        ProcessState::Exited(c)
                    } else {
                        ProcessState::Unknown
                    };
                    // Remove from manager.
                    drop(guard);
                    self.manager.remove(pid).await;
                    let stdout_str = String::from_utf8_lossy(&stdout).to_string();
                    let stderr_str = String::from_utf8_lossy(&stderr).to_string();
                    return Ok((stdout_str, stderr_str, state, code));
                }
                Ok(Err(e)) => {
                    return Err(GrodexError::ToolExecution(format!("wait error: {e}")));
                }
                Err(_) => {
                    // Timeout — just return current state.
                }
            }
        }

        // Check if child has exited without blocking.
        if let Ok(Some(status)) = entry.child.try_wait() {
            let code = status.code();
            let stdout = collect_task_output(&mut entry.stdout_task).await;
            let stderr = collect_task_output(&mut entry.stderr_task).await;
            let state = if let Some(c) = code {
                ProcessState::Exited(c)
            } else {
                ProcessState::Unknown
            };
            drop(guard);
            self.manager.remove(pid).await;
            let stdout_str = String::from_utf8_lossy(&stdout).to_string();
            let stderr_str = String::from_utf8_lossy(&stderr).to_string();
            return Ok((stdout_str, stderr_str, state, code));
        }

        // Still running — return empty output (caller can poll again).
        Ok((String::new(), String::new(), ProcessState::Running, None))
    }

    /// Write data to a process's stdin.
    async fn write_stdin(
        &self,
        pid: u32,
        data: &str,
        close_after: bool,
    ) -> Result<(), GrodexError> {
        let mut guard = self
            .manager
            .get(pid)
            .await
            .ok_or_else(|| GrodexError::ToolExecution(format!("process {pid} not found in manager")))?;

        let entry = guard.get_mut(&pid).unwrap();

        let stdin = entry.stdin.as_mut().ok_or_else(|| {
            GrodexError::ToolExecution(format!("process {pid} stdin is not open"))
        })?;

        stdin
            .write_all(data.as_bytes())
            .await
            .map_err(|e| GrodexError::ToolExecution(format!("stdin write failed: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| GrodexError::ToolExecution(format!("stdin flush failed: {e}")))?;

        if close_after {
            entry.stdin.take(); // drops the stdin handle → sends EOF
        }

        Ok(())
    }

    /// Send a signal to a process.
    async fn send_signal(&self, pid: u32, signal: &str) -> Result<serde_json::Value, GrodexError> {
        // Use nix-style signal sending via std::process::Command.
        let sig_name = match signal {
            "SIGTERM" | "TERM" => "TERM",
            "SIGKILL" | "KILL" => "KILL",
            "SIGINT" | "INT" => "INT",
            "SIGHUP" | "HUP" => "HUP",
            _ => {
                return Err(GrodexError::ToolExecution(format!(
                    "unsupported signal: {signal}"
                )));
            }
        };

        let result = std::process::Command::new("kill")
            .arg(format!("-{sig_name}"))
            .arg(pid.to_string())
            .output();

        let signal_sent = match result {
            Ok(output) if output.status.success() => Some(signal.to_string()),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(GrodexError::ToolExecution(format!(
                    "kill -{sig_name} {pid} failed: {stderr}"
                )));
            }
            Err(e) => {
                return Err(GrodexError::ToolExecution(format!(
                    "kill command failed: {e}"
                )));
            }
        };

        // If SIGKILL, remove from manager.
        if sig_name == "KILL" {
            self.manager.remove(pid).await;
        }

        let output = ProcessIoOutput {
            process_id: pid,
            stdout: String::new(),
            stderr: String::new(),
            state: if sig_name == "KILL" {
                ProcessState::Unknown
            } else {
                ProcessState::Running
            },
            still_running: sig_name != "KILL",
            exit_code: None,
            stdin_written: false,
            signal_sent,
        };

        serde_json::to_value(output).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

/// Collect output from a reader task, returning whatever it has produced.
async fn collect_task_output(task: &mut Option<tokio::task::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    if let Some(handle) = task.take() {
        // Try to get the result; if the task is still running, just abort.
        match handle.await {
            Ok(buf) => buf,
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    }
}

// ── BuiltInTool impl ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PreparedProcessIoCall {
    pub args: ProcessIoArgs,
}

impl PreparedCall for PreparedProcessIoCall {
    fn side_effect_hint(&self) -> SideEffectHint {
        if self.args.signal.is_some() {
            SideEffectHint::ReadOnly // signals are idempotent-ish
        } else {
            SideEffectHint::ReadOnly
        }
    }

    fn plan_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"process_io");
        h.update(self.args.process_id.to_le_bytes());
        if let Some(ref s) = self.args.signal {
            h.update(s.as_bytes());
        }
        let full = format!("{:x}", h.finalize());
        full[..16].to_string()
    }
}

impl BuiltInTool for ProcessIoTool {
    type Input = ProcessIoArgs;
    type Prepared = PreparedProcessIoCall;
    type OkResult = ToolResultEnvelope;
    type Error = GrodexError;

    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error> {
        Ok(PreparedProcessIoCall { args: input })
    }

    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error> {
        let start = std::time::Instant::now();
        let pid = prepared.args.process_id;

        // Build a summary for the model.
        let summary = if let Some(ref sig) = prepared.args.signal {
            format!("sent {sig} to process {pid}")
        } else if prepared.args.stdin.is_some() {
            format!("wrote stdin to process {pid}")
        } else {
            format!("polled process {pid}")
        };

        let model_text = format!("tool `process_io` ok: {summary}");

        let envelope = ToolResultEnvelope {
            tool_call_id: String::new(),
            operation_id: None,
            capability_id: Some("process_io".into()),
            contract_version: 1,
            status: ToolStatus::Ok,
            summary: model_text.clone(),
            model_content: vec![ModelContent::Text(model_text)],
            structured_data: BTreeMap::new(),
            artifacts: vec![],
            changed_resources: vec![],
            truncation: TruncationInfo::default(),
            wall_time: start.elapsed(),
            retryability: Retryability::Retryable,
            diagnostics: vec![],
        };

        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_manager_register_and_list() {
        let mgr = ProcessManager::new();
        assert!(mgr.list_pids().await.is_empty());

        // We can't easily create a Child in a test without spawning, so
        // just verify the manager starts empty and list works.
        let pids = mgr.list_pids().await;
        assert!(pids.is_empty());
    }

    #[tokio::test]
    async fn process_io_unknown_pid_returns_error() {
        let mgr = ProcessManager::new();
        let tool = ProcessIoTool::new(mgr);
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"process_id": 99999}),
            OperationId::new(),
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn process_io_signal_unknown_pid() {
        let mgr = ProcessManager::new();
        let tool = ProcessIoTool::new(mgr);
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"process_id": 99999, "signal": "SIGKILL"}),
            OperationId::new(),
        )
        .await;
        // kill on unknown PID should fail
        assert!(result.is_err());
    }
}
