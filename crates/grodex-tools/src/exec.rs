//! ExecTool — runs a shell command and captures stdout/stderr.
//!
//! Phase 1: simple process execution with timeout.
//! Phase 2+: process handle for long-running tasks, stdin support.

use crate::cancel::CancelRegistry;
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
    /// Run the command in the background. When true, the tool returns
    /// immediately with a `process_id` the caller can use to poll/kill.
    #[serde(default)]
    pub background: bool,
    /// Wait this many milliseconds before returning partial output + handle.
    /// Useful for long build commands: the caller gets intermediate progress.
    #[serde(default)]
    pub yield_time_ms: Option<u64>,
    /// Shell execution mode. "auto" picks the platform default shell;
    /// "bash"/"sh"/"zsh" force a specific shell; "none" executes the
    /// command directly without a shell wrapper.
    #[serde(default = "default_shell_mode")]
    pub shell_mode: String,
    /// Environment variable overrides for the child process. Each entry
    /// is "KEY=VALUE". These are merged on top of the parent env.
    #[serde(default)]
    pub env_delta: Vec<String>,
    /// Per-call memory cap in MB (RLIMIT_AS on unix). Overrides the
    /// tool-level limit. Linux enforces; macOS treats it as advisory.
    #[serde(default)]
    pub memory_limit_mb: Option<u64>,
    /// Per-call CPU seconds (RLIMIT_CPU). Overrides the tool-level limit.
    #[serde(default)]
    pub cpu_limit_secs: Option<u64>,
}

fn default_shell_mode() -> String {
    "auto".to_string()
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
    /// Whether the command is still running (background or yield_time mode).
    #[serde(default)]
    pub still_running: bool,
    /// Retained head bytes when stdout was truncated (HeadTailBuffer).
    #[serde(default)]
    pub retained_head: Option<String>,
    /// Retained tail bytes when stdout was truncated (HeadTailBuffer).
    #[serde(default)]
    pub retained_tail: Option<String>,
    /// Shell mode actually used for this execution.
    #[serde(default)]
    pub shell_mode_used: String,
    /// Environment variables that were set for the child process.
    #[serde(default)]
    pub env_delta_applied: Vec<String>,
}

/// Resource limits applied to every exec child via `setrlimit`
/// (unix, pre-exec). Doc 13 §19 quotas — previously only a wall-clock
/// timeout existed, so a runaway `yes` or a memory-hungry build could
/// consume unbounded CPU/RAM.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// RLIMIT_AS — max address space (MB). The primary memory cap on
    /// Linux; macOS treats RLIMIT_AS as advisory (documented).
    pub memory_limit_mb: u64,
    /// RLIMIT_CPU — max CPU seconds (soft; SIGXCPU at the cap).
    pub cpu_limit_secs: u64,
    /// RLIMIT_FSIZE — max bytes a child may write to any file (MB).
    pub file_size_limit_mb: u64,
    /// RLIMIT_NPROC — max child processes/threads.
    pub max_processes: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            memory_limit_mb: 8 * 1024,      // 8 GB
            cpu_limit_secs: 600,            // 10 min CPU (wall timeout is tighter)
            file_size_limit_mb: 1024,       // 1 GB per written file
            max_processes: 4096,
        }
    }
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
    /// Cancel registry for OperationId-level cancellation (§11.4).
    /// When set, the tool registers a CancellationToken before spawning
    /// and checks it during execution.
    cancel_registry: Option<CancelRegistry>,
    /// Resource limits applied pre-exec to every spawned child.
    resource_limits: ResourceLimits,
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
            cancel_registry: None,
            resource_limits: ResourceLimits::default(),
        }
    }

    /// Override the per-child resource limits (rlimits). Per-call
    /// `ExecArgs` overrides take precedence when tighter.
    pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = limits;
        self
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

    /// Enable OperationId-level cancellation (§11.4).
    ///
    /// When set, the tool registers a `CancellationToken` before spawning
    /// a process. The agent loop can call `cancel_registry.cancel(op_id)`
    /// to trigger the cancel pipeline (SIGINT → grace → SIGKILL).
    pub fn with_cancel_registry(mut self, registry: CancelRegistry) -> Self {
        self.cancel_registry = Some(registry);
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
            description: "Run a shell command and return stdout, stderr, and exit code. Default timeout 120s; output capped (head+tail) at 100KB. Resource limits: 8GB memory, 600s CPU, process-group kill on timeout. Credential-looking env vars are stripped from the inherited env — pass env_delta to re-add them.".into(),
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
                "description": {"type": "string", "description": "Human-readable description of what this command does"},
                "background": {"type": "boolean", "description": "Run in background; returns process_id immediately (default: false)"},
                "yield_time_ms": {"type": "integer", "description": "Wait this many ms then return partial output + handle for long-running commands"},
                "shell_mode": {"type": "string", "enum": ["auto", "bash", "sh", "zsh"], "description": "Shell used to run the command (default: auto = sh on unix, cmd on windows). Output shell_mode_used reports the shell actually used."},
                "env_delta": {"type": "array", "items": {"type": "string"}, "description": "Environment overrides as KEY=VALUE entries, merged on top of the (sanitized) parent env. NOTE: credential-looking vars (API_KEY/TOKEN/SECRET/...) are stripped from the inherited env; re-add any the command genuinely needs here."},
                "memory_limit_mb": {"type": "integer", "description": "Per-call memory cap in MB (RLIMIT_AS; default 8192). Enforced on Linux; advisory on macOS."},
                "cpu_limit_secs": {"type": "integer", "description": "Per-call CPU seconds cap (RLIMIT_CPU; default 600)"}
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
        // Fail fast on shell_mode the runtime cannot honor truthfully.
        let actual_shell = Self::resolve_shell(&args.shell_mode)?;


        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(120));

        // ── Cancel token registration (§11.4) ──
        let cancel_token = if let Some(ref registry) = self.cancel_registry {
            let token = registry.register(operation_id.to_string()).await;
            Some(token)
        } else {
            None
        };

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
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: Vec::new(), // sandbox runtime does not apply env_delta — report honestly
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
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: Vec::new(), // sandbox runtime does not apply env_delta — report honestly
                    };
                    serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
                }
            };
        }

        // ── Direct-spawn path (no sandbox configured) ───────────────

        // Build as std Command so we can pre_exec rlimits + setsid
        // (tokio::process::Command does not expose pre_exec), then convert.
        let mut std_cmd = if actual_shell == "cmd" {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", &args.command]);
            c
        } else {
            let mut c = std::process::Command::new(actual_shell);
            c.args(["-c", &args.command]);
            c
        };
        // Env hygiene: credential-looking vars stripped, env_delta re-added.
        Self::apply_env_hygiene(&mut std_cmd, &args.env_delta);
        // Resource limits + own process group (unix).
        let limits = self.effective_limits(&args);
        Self::apply_rlimits_and_group(&mut std_cmd, &limits);

        std_cmd.stdout(std::process::Stdio::piped());
        std_cmd.stderr(std::process::Stdio::piped());

        if let Some(ref cwd) = args.cwd {
            std_cmd.current_dir(cwd);
        }

        let mut cmd = Command::from(std_cmd);
        cmd.kill_on_drop(true);

        // Spawn the child and capture its PID.
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Err(GrodexError::ToolExecution(format!("exec spawn failed: {e}"))),
        };
        let child_pid = child.id();

        // ── Background mode: return immediately with PID ──
        if args.background {
            let duration_ms = start.elapsed().as_millis() as u64;
            let result = ExecOutput {
                command: args.command,
                process_id: child_pid,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
                timed_out: false,
                duration_ms,
                stdout_truncated: false,
                stderr_truncated: false,
                still_running: true,
                retained_head: None,
                retained_tail: None,
                shell_mode_used: actual_shell.to_string(),
                env_delta_applied: args.env_delta.clone(),
            };
            // Detach the child process (don't kill on drop).
            // The child continues running in the background.
            std::mem::forget(child);
            return serde_json::to_value(result)
                .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
        }

        // ── Yield-time mode: wait specified duration, return partial output ──
        if let Some(yield_ms) = args.yield_time_ms {
            // We cannot use `wait_with_output` here because it consumes the
            // child, making it impossible to detach on timeout. Instead we
            // take stdout/stderr, spawn collectors, and wait on the child
            // separately so we can `forget` it if the yield expires.
            let mut child = child;
            let stdout_child = child.stdout.take();
            let stderr_child = child.stderr.take();
            let cap = self
                .max_output_bytes
                .saturating_mul(2)
                .saturating_add(64 * 1024);
            let stdout_handle = tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                if let Some(r) = stdout_child {
                    Self::read_capped(r, cap).await.0
                } else {
                    Vec::new()
                }
            });
            let stderr_handle = tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                if let Some(r) = stderr_child {
                    Self::read_capped(r, cap).await.0
                } else {
                    Vec::new()
                }
            });

            let yield_dur = Duration::from_millis(yield_ms);
            match tokio::time::timeout(yield_dur, child.wait()).await {
                Ok(Ok(status)) => {
                    // Completed within yield time — collect output.
                    let stdout_bytes = stdout_handle.await.unwrap_or_default();
                    let stderr_bytes = stderr_handle.await.unwrap_or_default();
                    let stdout = String::from_utf8_lossy(&stdout_bytes);
                    let stderr = String::from_utf8_lossy(&stderr_bytes);
                    let (stdout_str, stdout_truncated) = Self::truncate(&stdout, self.max_output_bytes);
                    let (stderr_str, stderr_truncated) = Self::truncate(&stderr, self.max_output_bytes);
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let result = ExecOutput {
                        command: args.command,
                        process_id: child_pid,
                        stdout: stdout_str,
                        stderr: stderr_str,
                        exit_code: status.code(),
                        timed_out: false,
                        duration_ms,
                        stdout_truncated,
                        stderr_truncated,
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: args.env_delta.clone(),
                    };
                    return serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
                }
                Ok(Err(e)) => {
                    return Err(GrodexError::ToolExecution(format!("exec failed: {e}")));
                }
                Err(_) => {
                    // Yield time elapsed — return PID so caller can poll/kill.
                    // Detach the child so it keeps running.
                    std::mem::forget(child);
                    // Abort the collectors (they'll never finish since we
                    // detached the child's stdout/stderr pipes).
                    stdout_handle.abort();
                    stderr_handle.abort();
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let result = ExecOutput {
                        command: args.command,
                        process_id: child_pid,
                        stdout: String::new(),
                        stderr: format!("yield_time_ms={yield_ms} elapsed; command still running (pid={child_pid:?})"),
                        exit_code: None,
                        timed_out: false,
                        duration_ms,
                        stdout_truncated: false,
                        stderr_truncated: false,
                        still_running: true,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: args.env_delta.clone(),
                    };
                    return serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
                }
            }
        }

        // ── Normal mode: wait for completion with timeout + cancel ──
        let _output = if let Some(ref token) = cancel_token {
            // When cancel is enabled, we can't use wait_with_output() because
            // it consumes child. Instead, take stdout/stderr, spawn collectors,
            // and wait on child separately so we can kill it on cancel.
            let stdout_child = child.stdout.take();
            let stderr_child = child.stderr.take();
            let cap = self
                .max_output_bytes
                .saturating_mul(2)
                .saturating_add(64 * 1024);
            let stdout_handle = tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                if let Some(r) = stdout_child {
                    Self::read_capped(r, cap).await.0
                } else {
                    Vec::new()
                }
            });
            let stderr_handle = tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                if let Some(r) = stderr_child {
                    Self::read_capped(r, cap).await.0
                } else {
                    Vec::new()
                }
            });

            let token_clone = token.clone();
            let wait_result = tokio::select! {
                result = tokio::time::timeout(timeout, child.wait()) => {
                    match result {
                        Ok(Ok(status)) => Ok(Some(status)),
                        Ok(Err(e)) => Err(e),
                        Err(_) => {
                            // Timeout — kill the whole process group.
                            if let Some(pid) = child_pid {
                                Self::kill_process_group(pid);
                            }
                            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout"))
                        }
                    }
                }
                _ = token_clone.cancelled() => {
                    // Cancel requested — kill the child's WHOLE process
                    // group (setsid'd: pid == pgid), so grandchildren die too.
                    if let Some(pid) = child_pid {
                        Self::kill_process_group(pid);
                    }
                    let _ = child.start_kill();
                    let _ = child.wait().await; // reap
                    Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"))
                }
            };

            let stdout_bytes = stdout_handle.await.unwrap_or_default();
            let stderr_bytes = stderr_handle.await.unwrap_or_default();
            let stdout_str = String::from_utf8_lossy(&stdout_bytes).to_string();
            let stderr_str = String::from_utf8_lossy(&stderr_bytes).to_string();

            match wait_result {
                Ok(Some(status)) => {
                    let (stdout_t, stdout_trunc) = Self::truncate(&stdout_str, self.max_output_bytes);
                    let (stderr_t, stderr_trunc) = Self::truncate(&stderr_str, self.max_output_bytes);
                    let duration_ms = start.elapsed().as_millis() as u64;
                    // Clean up cancel token.
                    if let Some(ref registry) = self.cancel_registry {
                        registry.remove(&operation_id.to_string()).await;
                    }
                    let result = ExecOutput {
                        command: args.command,
                        process_id: child_pid,
                        stdout: stdout_t,
                        stderr: stderr_t,
                        exit_code: status.code(),
                        timed_out: false,
                        duration_ms,
                        stdout_truncated: stdout_trunc,
                        stderr_truncated: stderr_trunc,
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: args.env_delta.clone(),
                    };
                    return serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    // Cancelled.
                    let duration_ms = start.elapsed().as_millis() as u64;
                    if let Some(ref registry) = self.cancel_registry {
                        registry.remove(&operation_id.to_string()).await;
                    }
                    let reason = token.reason().await.unwrap_or_else(|| "cancelled".into());
                    let result = ExecOutput {
                        command: args.command,
                        process_id: child_pid,
                        stdout: stdout_str,
                        stderr: format!("command cancelled: {reason}"),
                        exit_code: None,
                        timed_out: false,
                        duration_ms,
                        stdout_truncated: false,
                        stderr_truncated: false,
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: args.env_delta.clone(),
                    };
                    return serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    // Timeout — kill the whole process group (the child was
                    // setsid'd; kill -pid reaches shell + grandchildren).
                    if let Some(pid) = child_pid {
                        Self::kill_process_group(pid);
                    }
                    if let Some(ref registry) = self.cancel_registry {
                        registry.remove(&operation_id.to_string()).await;
                    }
                    let result = ExecOutput {
                        command: args.command,
                        process_id: child_pid,
                        stdout: stdout_str,
                        stderr: format!("command timed out after {timeout:?}"),
                        exit_code: None,
                        timed_out: true,
                        duration_ms,
                        stdout_truncated: false,
                        stderr_truncated: false,
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: args.env_delta.clone(),
                    };
                    return serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
                }
                Err(e) => {
                    if let Some(ref registry) = self.cancel_registry {
                        registry.remove(&operation_id.to_string()).await;
                    }
                    return Err(GrodexError::ToolExecution(format!("exec failed: {e}")));
                }
                _ => unreachable!(),
            }
        } else {
            // No cancel token — manual wait + capped collectors (the old
            // wait_with_output read the child output unbounded into RAM).
            let stdout_child = child.stdout.take();
            let stderr_child = child.stderr.take();
            let cap = self
                .max_output_bytes
                .saturating_mul(2)
                .saturating_add(64 * 1024);
            let out_h = tokio::spawn(async move {
                if let Some(r) = stdout_child {
                    Self::read_capped(r, cap).await.0
                } else {
                    Vec::new()
                }
            });
            let err_h = tokio::spawn(async move {
                if let Some(r) = stderr_child {
                    Self::read_capped(r, cap).await.0
                } else {
                    Vec::new()
                }
            });
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(Ok(status)) => {
                    let proc_output = (
                        out_h.await.unwrap_or_default(),
                        err_h.await.unwrap_or_default(),
                    );
                    let stdout = String::from_utf8_lossy(&proc_output.0);
                    let stderr = String::from_utf8_lossy(&proc_output.1);
                    let (stdout_str, stdout_truncated) = Self::truncate(&stdout, self.max_output_bytes);
                    let (stderr_str, stderr_truncated) = Self::truncate(&stderr, self.max_output_bytes);
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let result = ExecOutput {
                        command: args.command,
                        process_id: child_pid,
                        stdout: stdout_str,
                        stderr: stderr_str,
                        exit_code: status.code(),
                        timed_out: false,
                        duration_ms,
                        stdout_truncated,
                        stderr_truncated,
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: args.env_delta.clone(),
                    };
                    return serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
                }
                Ok(Err(e)) => {
                    if let Some(ref registry) = self.cancel_registry {
                        registry.remove(&operation_id.to_string()).await;
                    }
                    return Err(GrodexError::ToolExecution(format!("exec failed: {e}")));
                }
                Err(_) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    if let Some(ref registry) = self.cancel_registry {
                        registry.remove(&operation_id.to_string()).await;
                    }
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
                        still_running: false,
                        retained_head: None,
                        retained_tail: None,
                        shell_mode_used: actual_shell.to_string(),
                        env_delta_applied: args.env_delta.clone(),
                    };
                    return serde_json::to_value(result)
                        .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
                }
            }
        };
    }
}

impl ExecTool {
    /// Head + tail truncation with a middle elision marker.
    ///
    /// Long command output (e.g. build logs) is shown as a head and a tail
    /// joined by an elision marker, so the user sees both the start and the
    /// final exit/error lines instead of only a head (the old behaviour).
    /// Cuts are char-aligned to avoid splitting UTF-8.
    /// Effective limits for one call: per-call args override the
    /// tool-level defaults when provided (and tighter).
    fn effective_limits(&self, args: &ExecArgs) -> ResourceLimits {
        let mut limits = self.resource_limits;
        if let Some(mb) = args.memory_limit_mb {
            limits.memory_limit_mb = mb;
        }
        if let Some(secs) = args.cpu_limit_secs {
            limits.cpu_limit_secs = secs;
        }
        limits
    }

    /// Apply rlimits + own process group to the child, pre-exec.
    ///
    /// Doc 13 §19 quotas + Codex-style process-group isolation:
    ///  - setsid() → the child becomes its own process-group leader, so a
    ///    timeout/cancel can `kill(-pid)` the WHOLE tree (shell + grandchildren),
    ///    not just the shell;
    ///  - setrlimit: AS (memory), CPU, FSIZE, NPROC, and CORE=0 (Codex
    ///    process-hardening: core dumps off);
    ///  - best-effort: a failing setrlimit does NOT abort the spawn
    ///    (RLIMIT_AS is advisory on macOS anyway).
    #[allow(unsafe_code)]
    fn apply_rlimits_and_group(cmd: &mut std::process::Command, limits: &ResourceLimits) {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let mem = limits.memory_limit_mb.saturating_mul(1024 * 1024);
            let cpu = limits.cpu_limit_secs;
            let fsize = limits.file_size_limit_mb.saturating_mul(1024 * 1024);
            let nproc = limits.max_processes;
            unsafe {
                cmd.pre_exec(move || {
                    // Own process group (pid == pgid).
                    if libc::setsid() < 0 {
                        // Best-effort: ignore EPERM (already a group leader).
                    }
                    let set = |res: i32, cur: u64| {
                        let rlim = libc::rlimit {
                            rlim_cur: cur,
                            rlim_max: cur,
                        };
                        // Ignore failures: advisory limits must not break spawn.
                        let _ = libc::setrlimit(res, &rlim);
                    };
                    set(libc::RLIMIT_AS, mem);
                    set(libc::RLIMIT_CPU, cpu);
                    set(libc::RLIMIT_FSIZE, fsize);
                    set(
                        libc::RLIMIT_NPROC,
                        nproc as u64,
                    );
                    // Codex process-hardening: core dumps off.
                    set(libc::RLIMIT_CORE, 0);
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (cmd, limits);
        }
    }

    /// Kill the child's WHOLE process group (child was setsid'd, so
    /// pid == pgid). Codex-style escalation: SIGKILL the tree, not just
    /// the shell.
    #[allow(unsafe_code)]
    fn kill_process_group(pid: u32) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
        }
    }

    /// Resolve the requested `shell_mode` into the ACTUAL shell binary
    /// used. `none` is rejected outright — commands are strings that need
    /// a shell to parse, and a silent whitespace-split would mangle quoted
    /// arguments; the model should use dedicated tools for file ops.
    fn resolve_shell(requested: &str) -> Result<&'static str, GrodexError> {
        match requested {
            "auto" => Ok(if cfg!(target_os = "windows") { "cmd" } else { "sh" }),
            "sh" => Ok("sh"),
            "bash" => Ok("bash"),
            "zsh" => Ok("zsh"),
            "none" => Err(GrodexError::ToolExecution(
                "shell_mode='none' is not supported — commands run through a shell;                  use read_file/write_file/edit_file instead of shell builtins"
                    .into(),
            )),
            other => Err(GrodexError::ToolExecution(format!(
                "unknown shell_mode '{other}' (expected auto|bash|sh|zsh)"
            ))),
        }
    }

    /// Drain a child stream with a HARD memory cap. The old collectors
    /// used `read_to_end` — a command emitting gigabytes was fully read
    /// into RAM before the string-level truncation ran. Now: keep the
    /// first `cap` bytes plus a rolling window of the last `cap` bytes;
    /// everything in between is discarded as it arrives. The child is
    /// still fully drained (no SIGPIPE behavior change), only buffered.
    async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> (Vec<u8>, u64) {
        use tokio::io::AsyncReadExt;
        let mut head: Vec<u8> = Vec::with_capacity(cap.min(1024 * 1024));
        let mut tail: Vec<u8> = Vec::new();
        let mut total: u64 = 0;
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    total += n as u64;
                    let data = &chunk[..n];
                    if head.len() < cap {
                        let take = (cap - head.len()).min(n);
                        head.extend_from_slice(&data[..take]);
                        tail.extend_from_slice(&data[take..]);
                    } else {
                        tail.extend_from_slice(data);
                    }
                    if tail.len() > cap {
                        let overflow = tail.len() - cap;
                        tail.drain(..overflow);
                    }
                }
                Err(_) => break,
            }
        }
        if head.len() < cap && !tail.is_empty() {
            head.extend_from_slice(&tail);
            (head, total)
        } else {
            (head, total)
        }
    }

    /// Env-var names that look like credentials are stripped from the
    /// inherited environment before spawning a model-authored command —
    /// the parent env routinely carries API keys / OAuth tokens, and every
    /// exec'd command can read them. Explicit `env_delta` entries bypass
    /// the filter (the user asked for them by name).
    fn is_secret_env_name(name: &str) -> bool {
        let upper = name.to_ascii_uppercase();
        upper.contains("API_KEY")
            || upper.contains("APIKEY")
            || upper.contains("TOKEN")
            || upper.contains("SECRET")
            || upper.contains("PASSWORD")
            || upper.contains("PASSWD")
            || upper.contains("CREDENTIAL")
            || upper.ends_with("_KEY")
    }

    /// Apply the env hygiene policy + explicit env_delta to a command.
    ///
    /// CRITICAL: iterate `vars_os`, never `vars` — `vars()` unwraps
    /// `into_string()` and PANICS the whole runtime when the environment
    /// contains a single non-UTF-8 variable name/value (observed in the
    /// wild with paths containing non-UTF-8 bytes).
    fn apply_env_hygiene(cmd: &mut std::process::Command, env_delta: &[String]) {
        for (name, _) in std::env::vars_os() {
            match name.into_string() {
                Ok(name) => {
                    if Self::is_secret_env_name(&name) {
                        cmd.env_remove(&name);
                    }
                }
                Err(bad) => {
                    // Non-UTF-8 names can neither match the secret list nor
                    // be re-added via env_delta — drop them from the child.
                    cmd.env_remove(&bad);
                }
            }
        }
        for entry in env_delta {
            if let Some((k, v)) = entry.split_once('=') {
                cmd.env(k, v);
            }
        }
    }

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
                    still_running: false,
                    retained_head: None,
                    retained_tail: None,
                    shell_mode_used: prepared.args.shell_mode.clone(),
                    env_delta_applied: prepared.args.env_delta.clone(),
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
                    still_running: false,
                    retained_head: None,
                    retained_tail: None,
                    shell_mode_used: prepared.args.shell_mode.clone(),
                    env_delta_applied: prepared.args.env_delta.clone(),
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

    /// rlimits actually reach the child (unix): CPU cap = 1s, NPROC = 123.
    #[cfg(unix)]
    #[test]
    fn rlimits_reach_child() {
        let limits = ResourceLimits {
            memory_limit_mb: 2048,
            cpu_limit_secs: 1,
            file_size_limit_mb: 16,
            max_processes: 123,
        };
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "ulimit -t; ulimit -u"]);
        ExecTool::apply_rlimits_and_group(&mut cmd, &limits);
        let out = cmd.output().expect("child");
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines = text.lines();
        let cpu: u64 = lines.next().unwrap().trim().parse().unwrap();
        let nproc: u64 = lines.next().unwrap().trim().parse().unwrap();
        assert_eq!(cpu, 1, "RLIMIT_CPU");
        assert_eq!(nproc, 123, "RLIMIT_NPROC");
    }

    /// The child gets its own process group (setsid) — pid == pgid.
    #[cfg(unix)]
    #[test]
    fn child_is_own_process_group_leader() {
        let limits = ResourceLimits::default();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "ps -o pgid= -p $$"]);
        ExecTool::apply_rlimits_and_group(&mut cmd, &limits);
        let out = cmd.output().expect("child");
        let pgid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("pgid output");
        // setsid 生效断言: pgid 不等于测试进程自身的 pgid（子进程自成一组）。
        assert_ne!(
            pgid,
            {
                let out = std::process::Command::new("ps")
                    .args(["-o", "pgid=", "-p", &std::process::id().to_string()])
                    .output()
                    .expect("ps");
                String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().unwrap()
            },
            "child must lead its own process group (setsid)"
        );
    }
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

    #[tokio::test]
    async fn exec_background_returns_immediately_with_pid() {
        let tool = ExecTool::new();
        // `sleep 10` is a long-running command; background mode should return
        // immediately with still_running=true and a valid PID.
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"command": "sleep 10", "background": true}),
            OperationId::new(),
        )
        .await
        .unwrap();

        let output: ExecOutput = serde_json::from_value(result).unwrap();
        assert!(output.still_running, "background command must be still_running");
        assert!(output.process_id.is_some(), "must return a process_id");
        assert!(output.exit_code.is_none(), "no exit code yet");
        assert!(!output.timed_out);

        // Clean up: kill the background sleep.
        if let Some(pid) = output.process_id {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .output();
        }
    }

    #[tokio::test]
    async fn exec_yield_time_completes_within_window() {
        let tool = ExecTool::new();
        // `echo hi` finishes well within 5 seconds.
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"command": "echo hi", "yield_time_ms": 5000}),
            OperationId::new(),
        )
        .await
        .unwrap();

        let output: ExecOutput = serde_json::from_value(result).unwrap();
        assert!(!output.still_running, "fast command should not be still_running");
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("hi"));
    }

    #[tokio::test]
    async fn exec_yield_time_expires_returns_still_running() {
        let tool = ExecTool::new();
        // `sleep 30` won't finish within 50ms yield window.
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"command": "sleep 30", "yield_time_ms": 50}),
            OperationId::new(),
        )
        .await
        .unwrap();

        let output: ExecOutput = serde_json::from_value(result).unwrap();
        assert!(output.still_running, "yield expired → still_running");
        assert!(output.process_id.is_some(), "must return pid for later poll/kill");
        assert!(output.exit_code.is_none());

        // Clean up.
        if let Some(pid) = output.process_id {
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .output();
        }
    }
}
