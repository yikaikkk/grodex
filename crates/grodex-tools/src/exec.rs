//! ExecTool — runs a shell command and captures stdout/stderr.
//!
//! Phase 1: simple process execution with timeout.
//! Phase 2+: process handle for long-running tasks, stdin support.

use crate::common::{
    BuiltInTool, ChangedResource, ChangeType, HeadTailBuffer, ModelContent, PreparedCall,
    ProcessHandle, ProcessState, Retryability, SideEffectHint, ToolResultEnvelope, ToolStatus,
    TruncationInfo,
};
use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use grodex_sandbox::runtime::{
    PreparedOperation, SandboxRuntimeClient, SandboxRuntimeRequest, SandboxRuntimeResponse,
};
use grodex_sandbox_types::profile::SandboxProfile;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecArgs {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    pub command: String,
    /// Process ID for lifecycle tracking (background/kill).
    pub process_id: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub struct ExecTool {
    max_output_bytes: usize,
    #[allow(dead_code)]
    default_timeout: Duration,
    /// Optional sandbox runtime. When set (together with `sandbox_profile`),
    /// `execute` routes the command through `client.run_dispatched()`
    /// (in-process sandbox-exec OR external supervisor) instead of a bare
    /// `tokio::process::Command`. Fail-closed: a `Refused` response is
    /// surfaced as a tool error — the command is **never** silently run
    /// unsandboxed.
    sandbox_runtime: Option<SandboxRuntimeClient>,
    /// The sandbox profile governing exec operations (cloned from the
    /// session's `SandboxManager` effective profile). Required together
    /// with `sandbox_runtime` for the sandboxed path.
    sandbox_profile: Option<SandboxProfile>,
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecTool {
    pub fn new() -> Self {
        Self {
            max_output_bytes: 100_000,
            default_timeout: Duration::from_secs(120),
            sandbox_runtime: None,
            sandbox_profile: None,
        }
    }

    /// Enable sandbox-enforced execution. When set, `execute` builds a
    /// `PreparedOperation` (program `sh -c <command>` + the session's
    /// effective profile) and runs it through the runtime client, so the
    /// kernel enforces `deny_paths`/network rules. The command's
    /// stdout/stderr are captured and returned to the model.
    ///
    /// Fail-closed: if the platform has no enforcement backend
    /// (non-macOS, or sandbox-exec missing), `run_dispatched` returns
    /// `Refused` and the tool returns an error rather than running
    /// unsandboxed. Callers that want a fallback must NOT set this.
    pub fn with_sandbox_runtime(
        mut self,
        client: SandboxRuntimeClient,
        profile: SandboxProfile,
    ) -> Self {
        self.sandbox_runtime = Some(client);
        self.sandbox_profile = Some(profile);
        self
    }
}

impl Tool for ExecTool {
    type Args = ExecArgs;
    type Output = ExecOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "exec".into(),
            display_name: "Execute Command".into(),
            description: "Run a shell command and return stdout, stderr, and exit code.".into(),
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Ask,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The shell command to execute"},
                "cwd": {"type": "string", "description": "Working directory for the command"},
                "timeout_secs": {"type": "integer", "description": "Timeout in seconds"},
                "description": {"type": "string", "description": "Human-readable description of what this command does"}
            },
            "required": ["command"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "stdout": {"type": "string"},
                "stderr": {"type": "string"},
                "exit_code": {"type": "integer"},
                "timed_out": {"type": "boolean"},
                "duration_ms": {"type": "integer"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for ExecTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: ExecArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(120));

        // ── Sandbox-enforced path ──────────────────────────────────
        // When the session wired a `SandboxRuntimeClient` + effective profile
        // into the tool, route the command through `run_dispatched()` so the
        // kernel (sandbox-exec) actually enforces deny/network rules, instead
        // of the bare `tokio::process::Command` used below. The runtime client
        // uses blocking `std::process::Command` internally, so offload it to a
        // blocking thread to avoid stalling the async runtime.
        //
        // Fail-closed: a `Refused` (unsupported platform, missing backend,
        // authority ceiling 0, spawn/IO error) is surfaced as a tool error —
        // the command is never silently run unsandboxed. This honours the
        // project invariant that the sandbox runtime fails closed when the
        // backend is missing or unsupported.
        if let (Some(client), Some(profile)) = (&self.sandbox_runtime, &self.sandbox_profile) {
            let mut op = PreparedOperation::new(
                operation_id.to_string(),
                "sh",
                profile.clone(),
            )
            .with_arg("-c")
            .with_arg(&args.command)
            .with_authority_ceiling(1);
            if let Some(ref cwd) = args.cwd {
                op = op.with_cwd(cwd);
            }
            let req = SandboxRuntimeRequest { operation: op };
            let client = client.clone();
            let join = tokio::task::spawn_blocking(move || client.run_dispatched(req));
            let outcome = tokio::time::timeout(timeout, join).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            return match outcome {
                Ok(Ok(SandboxRuntimeResponse::Completed {
                    exit_status,
                    stdout,
                    stderr,
                    ..
                })) => {
                    let stdout_s = stdout.unwrap_or_default();
                    let stderr_s = stderr.unwrap_or_default();
                    let (stdout_str, stdout_truncated) =
                        Self::truncate(&stdout_s, self.max_output_bytes);
                    let (stderr_str, stderr_truncated) =
                        Self::truncate(&stderr_s, self.max_output_bytes);
                    let exit_code = match exit_status {
                        grodex_sandbox::runtime::ExitStatusStatus::Code(c) => Some(c),
                        _ => None,
                    };
                    let result = ExecOutput {
                        command: args.command,
                        process_id: None,
                        stdout: stdout_str,
                        stderr: stderr_str,
                        exit_code,
                        timed_out: false,
                        duration_ms,
                        stdout_truncated,
                        stderr_truncated,
                    };
                    serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
                }
                Ok(Ok(SandboxRuntimeResponse::Refused { reason, .. })) => {
                    Err(GrodexError::ToolExecution(format!(
                        "sandbox refused exec: {reason}"
                    )))
                }
                Ok(Err(join_e)) => Err(GrodexError::ToolExecution(format!(
                    "sandbox exec task panicked: {join_e}"
                ))),
                Err(_) => {
                    let result = ExecOutput {
                        command: args.command,
                        process_id: None,
                        stdout: String::new(),
                        stderr: format!("command timed out after {timeout:?} (sandboxed)"),
                        exit_code: None,
                        timed_out: true,
                        duration_ms,
                        stdout_truncated: false,
                        stderr_truncated: false,
                    };
                    serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
                }
            };
        }

        // ── Direct-spawn path (no sandbox configured) ───────────────

        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.args(["/C", &args.command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", &args.command]);
            c
        };

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.kill_on_drop(true);

        if let Some(ref cwd) = args.cwd {
            cmd.current_dir(cwd);
        }

        // Spawn the child and capture its PID *before* awaiting completion.
        // Previously this returned `std::process::id()` — the agent's own
        // PID — which is meaningless to callers tracking the child.
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Err(GrodexError::ToolExecution(format!("exec spawn failed: {e}"))),
        };
        let child_pid = child.id();

        let output = tokio::time::timeout(timeout, child.wait_with_output()).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        match output {
            Ok(Ok(proc_output)) => {
                let stdout = String::from_utf8_lossy(&proc_output.stdout);
                let stderr = String::from_utf8_lossy(&proc_output.stderr);

                let (stdout_str, stdout_truncated) = Self::truncate(&stdout, self.max_output_bytes);
                let (stderr_str, stderr_truncated) = Self::truncate(&stderr, self.max_output_bytes);

                let result = ExecOutput {
                    command: args.command,
                    process_id: child_pid,
                    stdout: stdout_str,
                    stderr: stderr_str,
                    exit_code: proc_output.status.code(),
                    timed_out: false,
                    duration_ms,
                    stdout_truncated,
                    stderr_truncated,
                };

                serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
            }
            Ok(Err(e)) => Err(GrodexError::ToolExecution(format!("exec failed: {e}"))),
            Err(_) => {
                let result = ExecOutput {
                    command: args.command,
                    process_id: child_pid,
                    stdout: String::new(),
                    stderr: format!("command timed out after {timeout:?}"),
                    exit_code: None,
                    timed_out: true,
                    duration_ms,
                    stdout_truncated: false,
                    stderr_truncated: false,
                };
                serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
            }
        }
    }
}

impl ExecTool {
    /// Head + tail truncation with a middle elision marker.
    ///
    /// Long command output (e.g. build logs) is shown as a head and a tail
    /// joined by an elision marker, so the user sees both the start and the
    /// final exit/error lines instead of only a head (the old behaviour).
    /// Cuts are char-aligned to avoid splitting UTF-8.
    fn truncate(s: &str, max_bytes: usize) -> (String, bool) {
        if s.len() <= max_bytes {
            return (s.to_string(), false);
        }
        // Reserve room for the marker line, split the rest 50/50.
        const MARKER: &str = "\n... [truncated, N bytes omitted] ...\n";
        let budget = max_bytes.saturating_sub(MARKER.len());
        if budget < 8 {
            // Not enough room for head+tail; fall back to a head-only cut.
            return (Self::char_prefix(s, max_bytes) + "\n... [truncated]", true);
        }
        let head_bytes = budget / 2;
        let tail_bytes = budget - head_bytes;
        let head = Self::char_prefix(s, head_bytes);
        let tail = Self::char_suffix(s, tail_bytes);
        let omitted = s.len() - head.len() - tail.len();
        (
            format!("{head}\n... [truncated, {omitted} bytes omitted] ...\n{tail}"),
            true,
        )
    }

    /// Largest char-aligned prefix not exceeding `max_bytes`.
    fn char_prefix(s: &str, max_bytes: usize) -> String {
        if s.len() <= max_bytes {
            return s.to_string();
        }
        let mut end = max_bytes;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }

    /// Smallest char-aligned suffix fitting in `max_bytes` from the end.
    fn char_suffix(s: &str, max_bytes: usize) -> String {
        if s.len() <= max_bytes {
            return s.to_string();
        }
        let cap = max_bytes.min(s.len());
        let mut start = s.len() - cap;
        while start < s.len() && !s.is_char_boundary(start) {
            start += 1;
        }
        s[start..].to_string()
    }
}

#[derive(Debug, Clone)]
pub struct PreparedExecCall {
    pub args: ExecArgs,
    pub timeout: Duration,
    pub cwd: PathBuf,
    pub process_handle_draft: ProcessHandle,
    pub shell_command: String,
    pub shell_args: Vec<String>,
}

impl PreparedCall for PreparedExecCall {
    fn side_effect_hint(&self) -> SideEffectHint {
        SideEffectHint::ProcessSpawn
    }

    fn plan_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.args.command.as_bytes());
        h.update(self.args.cwd.clone().unwrap_or_default().as_bytes());
        h.update(self.args.timeout_secs.unwrap_or(0).to_le_bytes());
        let full = format!("{:x}", h.finalize());
        full[..16].to_string()
    }
}

impl BuiltInTool for ExecTool {
    type Input = ExecArgs;
    type Prepared = PreparedExecCall;
    type OkResult = ToolResultEnvelope;
    type Error = GrodexError;

    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error> {
        if let Some(t) = input.timeout_secs {
            if t as i64 > i64::MAX / 1000 {
                return Err(GrodexError::ToolExecution(format!(
                    "timeout_secs too large: {t}"
                )));
            }
        }
        let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(120));

        let cwd = if let Some(ref cwd_str) = input.cwd {
            PathBuf::from(cwd_str)
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        };

        let (shell_command, shell_args) = if cfg!(target_os = "windows") {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), input.command.clone()],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), input.command.clone()],
            )
        };

        let now = SystemTime::now();
        let lease_expires = now + timeout + Duration::from_secs(60);
        let process_handle_draft = ProcessHandle {
            process_id: 0,
            operation_id: String::new(),
            environment_id: String::new(),
            created_at: now,
            state: ProcessState::Unknown,
            stdin_open: false,
            tty: false,
            lease_expires_at: lease_expires,
        };

        Ok(PreparedExecCall {
            args: input,
            timeout,
            cwd,
            process_handle_draft,
            shell_command,
            shell_args,
        })
    }

    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error> {
        let start = std::time::Instant::now();

        let mut cmd = std::process::Command::new(&prepared.shell_command);
        cmd.args(&prepared.shell_args);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.current_dir(&prepared.cwd);

        let (tx, rx) = mpsc::channel();
        let cmd_clone = prepared.shell_command.clone();
        let args_clone = prepared.shell_args.clone();
        let cwd_clone = prepared.cwd.clone();

        let handle = std::thread::spawn(move || {
            let mut inner = std::process::Command::new(&cmd_clone);
            inner.args(&args_clone);
            inner.stdout(std::process::Stdio::piped());
            inner.stderr(std::process::Stdio::piped());
            inner.current_dir(&cwd_clone);
            let res = inner.output();
            let _ = tx.send(res);
        });

        let recv_result = rx.recv_timeout(prepared.timeout);

        let duration_ms = start.elapsed().as_millis() as u64;

        match recv_result {
            Ok(Ok(proc_output)) => {
                let _ = handle.join();

                let mut buffer = HeadTailBuffer::default_exec_ratio();
                buffer.append(&proc_output.stdout);
                buffer.append(&proc_output.stderr);

                let stdout = String::from_utf8_lossy(&proc_output.stdout);
                let stderr = String::from_utf8_lossy(&proc_output.stderr);

                let (stdout_str, stdout_truncated) = Self::truncate(&stdout, self.max_output_bytes);
                let (stderr_str, stderr_truncated) = Self::truncate(&stderr, self.max_output_bytes);

                let result = ExecOutput {
                    command: prepared.args.command.clone(),
                    process_id: None,
                    stdout: stdout_str.clone(),
                    stderr: stderr_str.clone(),
                    exit_code: proc_output.status.code(),
                    timed_out: false,
                    duration_ms,
                    stdout_truncated,
                    stderr_truncated,
                };

                let output_serialized = serde_json::to_string(&result).ok();
                let mut structured_data = BTreeMap::new();
                if let Some(s) = output_serialized {
                    structured_data
                        .insert("output_serialized".into(), serde_json::Value::String(s));
                }
                structured_data.insert(
                    "process_handle".into(),
                    serde_json::to_value(&prepared.process_handle_draft)
                        .unwrap_or(serde_json::Value::Null),
                );

                let combined_text = format!(
                    "tool `exec` ok: exit {:?}\nstdout:\n{}\nstderr:\n{}",
                    result.exit_code, stdout_str, stderr_str
                );
                let total_bytes = (stdout_str.len() + stderr_str.len()) as u64;

                let mut process_handle = prepared.process_handle_draft.clone();
                process_handle.state = if proc_output.status.success() {
                    ProcessState::Exited(0)
                } else {
                    ProcessState::Exited(proc_output.status.code().unwrap_or(-1))
                };

                let changed_resources = vec![ChangedResource {
                    resource_id: format!("process://exec/{}", prepared.plan_id()),
                    display_path: PathBuf::from(&prepared.args.command),
                    change_type: ChangeType::Created,
                    before_hash: None,
                    after_hash: Some(format!("exit:{:?}", result.exit_code)),
                }];

                let model_text = format!(
                    "tool `exec` ok: exit code {:?}, {} ms",
                    result.exit_code, duration_ms
                );

                let envelope = ToolResultEnvelope {
                    tool_call_id: String::new(),
                    operation_id: None,
                    capability_id: Some("exec".into()),
                    contract_version: 1,
                    status: if result.exit_code == Some(0) {
                        ToolStatus::Ok
                    } else {
                        ToolStatus::ToolError
                    },
                    summary: model_text.clone(),
                    model_content: vec![ModelContent::Text(combined_text)],
                    structured_data,
                    artifacts: vec![],
                    changed_resources,
                    truncation: TruncationInfo {
                        original_bytes: total_bytes,
                        retained_bytes: buffer.retained_bytes().min(total_bytes),
                        strategy: if stdout_truncated || stderr_truncated {
                            crate::common::TruncationStrategy::HeadTail
                        } else {
                            crate::common::TruncationStrategy::None
                        },
                        omitted: total_bytes.saturating_sub(buffer.retained_bytes()),
                    },
                    wall_time: start.elapsed(),
                    retryability: Retryability::NotRetryable,
                    diagnostics: vec![],
                };

                Ok(envelope)
            }
            Ok(Err(e)) => Err(GrodexError::ToolExecution(format!(
                "exec spawn failed: {e}"
            ))),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = handle.join().ok();

                let result = ExecOutput {
                    command: prepared.args.command.clone(),
                    process_id: None,
                    stdout: String::new(),
                    stderr: format!("command timed out after {:?}", prepared.timeout),
                    exit_code: None,
                    timed_out: true,
                    duration_ms,
                    stdout_truncated: false,
                    stderr_truncated: false,
                };

                let output_serialized = serde_json::to_string(&result).ok();
                let mut structured_data = BTreeMap::new();
                if let Some(s) = output_serialized {
                    structured_data
                        .insert("output_serialized".into(), serde_json::Value::String(s));
                }

                let model_text = format!(
                    "tool `exec` timed out after {:?}",
                    prepared.timeout
                );

                let envelope = ToolResultEnvelope {
                    tool_call_id: String::new(),
                    operation_id: None,
                    capability_id: Some("exec".into()),
                    contract_version: 1,
                    status: ToolStatus::Cancelled,
                    summary: model_text.clone(),
                    model_content: vec![ModelContent::Text(model_text)],
                    structured_data,
                    artifacts: vec![],
                    changed_resources: vec![],
                    truncation: TruncationInfo::default(),
                    wall_time: start.elapsed(),
                    retryability: Retryability::NotRetryable,
                    diagnostics: vec![],
                };

                Ok(envelope)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(GrodexError::ToolExecution(
                    "exec: thread channel disconnected".into(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::tool::ToolRuntime;

    #[tokio::test]
    async fn exec_echo() {
        let tool = ExecTool::new();
        let result = ToolRuntime::execute(&tool, serde_json::json!({"command": "echo hello"}), OperationId::new())
            .await
            .unwrap();

        let output: ExecOutput = serde_json::from_value(result).unwrap();
        assert!(output.stdout.contains("hello"));
        assert_eq!(output.exit_code, Some(0));
        assert!(!output.timed_out);
    }

    #[tokio::test]
    async fn exec_with_error() {
        let tool = ExecTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"command": "cat /nonexistent_file_xyz"}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: ExecOutput = serde_json::from_value(result).unwrap();
        assert_ne!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn exec_returns_child_pid_not_self() {
        // 断链修复: process_id 必须是子进程 PID,不能再返回 agent 自身 PID。
        let self_pid = std::process::id();
        // `sh -c 'echo $$'` prints the shell's PID, which equals our captured
        // child PID (sh is the spawned process).
        let tool = ExecTool::new();
        let result = ToolRuntime::execute(&tool, serde_json::json!({"command": "echo $$"}), OperationId::new())
            .await
            .unwrap();
        let output: ExecOutput = serde_json::from_value(result).unwrap();
        let reported = output
            .process_id
            .expect("child PID must be captured, not None");
        assert_eq!(
            output.stdout.trim().parse::<u32>().ok(),
            Some(reported),
            "reported pid must match the shell's printed PID ($$)"
        );
        assert_ne!(reported, self_pid, "must not report the agent's own PID");
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = "HEAD".to_string() + &"x".repeat(10_000) + "TAIL";
        let max = 200usize;
        let (out, truncated) = ExecTool::truncate(&s, max);
        assert!(truncated);
        assert!(out.starts_with("HEAD"), "head preserved: {}", &out[..20]);
        assert!(out.ends_with("TAIL"), "tail preserved: {}", &out[out.len().saturating_sub(20)..]);
        assert!(out.contains("[truncated"), "elision marker present");
    }

    #[test]
    fn truncate_short_output_passthrough() {
        let (out, truncated) = ExecTool::truncate("hello", 100);
        assert!(!truncated);
        assert_eq!(out, "hello");
    }
}
