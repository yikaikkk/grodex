//! External sandbox runtime — the process boundary the audit flagged.
//!
//! Design Doc 13's Trusted Sandbox Supervisor is supposed to be an
//! **independent process** the Agent talks to over a broker protocol, not a
//! session-internal struct. The Phase-1 code only had `enforce_seatbelt`
//! running the command in-process through `sandbox-exec`, with no notion of
//! a *prepared operation* or a *runtime client* that could hand the work to
//! a separate supervisor.
//!
//! This module introduces the protocol boundary without yet spawning a real
//! supervisor process:
//!
//!   - `PreparedOperation`: an opaque, fully-validated operation (command +
//!     profile + operation id + authority ceiling) ready to be handed off.
//!   - `SandboxRuntimeClient`: the Agent's handle. `run` delegates to the
//!     platform enforcer today (`enforce_seatbelt` / fallback) but is shaped
//!     as a client→supervisor RPC, so swapping in an out-of-process
//!     supervisor later is a transport change, not a redesign.
//!   - `SandboxRuntimeRequest` / `SandboxRuntimeResponse`: the wire shape the
//!     future supervisor speaks (a 14-message subset is out of scope; this is
//!     the minimum to define the boundary).
//!
//! This makes the "Trusted Sandbox Supervisor is external" architecture
//! *real at the type level* rather than a comment, and lets a fork/supervisor
//! process be added incrementally.

use crate::platform::{enforce_seatbelt, enforce_seatbelt_capturing, SandboxEnforceError};
use crate::profile_layers::{AccessLevel, LayeredProfileInput};
use crate::profile::ProfileStore;
use grodex_sandbox_types::profile::SandboxProfile;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, ExitStatus, Stdio};

/// A fully-prepared sandbox operation. Built by the Agent (or a sub-agent),
/// validated, and handed to the runtime client to execute. After this point
/// the sandbox profile governing the operation cannot change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedOperation {
    /// Unique id (audit/idempotency).
    pub operation_id: String,
    /// Program to run.
    pub program: String,
    /// Argv (excluding program).
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Env (k,v) pairs to apply.
    pub env: Vec<(String, String)>,
    /// Sandbox profile enforced for this op (after intersection).
    pub profile: SandboxProfile,
    /// Agent authority ceiling inherited from the session — the operation
    /// must not exceed it (invariant #12 enforcement surface).
    pub agent_authority_ceiling: u8,
    /// Optional label for telemetry (e.g. "exec:bash", "subagent:worker-3").
    pub label: Option<String>,
    /// Serialized seatbelt snapshot of the effective profile (JSON).
    pub profile_seatbelt_sb: String,
    /// Diagnostics emitted during layered intersection (audit log).
    pub profile_diagnostics: Vec<String>,
}

impl PreparedOperation {
    pub fn new(operation_id: impl Into<String>, program: impl Into<String>, profile: SandboxProfile) -> Self {
        let profile_seatbelt_sb = serde_json::to_string(&profile).unwrap_or_default();
        Self {
            operation_id: operation_id.into(),
            program: program.into(),
            argv: Vec::new(),
            cwd: None,
            env: Vec::new(),
            profile,
            agent_authority_ceiling: 0,
            label: None,
            profile_seatbelt_sb,
            profile_diagnostics: Vec::new(),
        }
    }
    pub fn with_arg(mut self, a: impl Into<String>) -> Self {
        self.argv.push(a.into());
        self
    }
    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.argv.extend(args);
        self
    }
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }
    pub fn with_authority_ceiling(mut self, c: u8) -> Self {
        self.agent_authority_ceiling = c;
        self
    }
    pub fn with_label(mut self, l: impl Into<String>) -> Self {
        self.label = Some(l.into());
        self
    }
    pub fn with_profile_layered_input(
        mut self,
        store: &ProfileStore,
        input: &LayeredProfileInput,
        level: AccessLevel,
    ) -> Self {
        let result = store.resolve_layered(input, level);
        self.profile = result.effective.clone();
        self.profile_seatbelt_sb = serde_json::to_string(&result.effective).unwrap_or_default();
        self.profile_diagnostics = result.diagnostics;
        self
    }
}

/// Wire request the (future) external supervisor receives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRuntimeRequest {
    pub operation: PreparedOperation,
}

/// Wire response from the supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SandboxRuntimeResponse {
    /// Operation ran to completion under the sandbox.
    ///
    /// `stdout`/`stderr` carry the sandboxed command's captured output as
    /// lossy UTF-8 (`String::from_utf8_lossy`). They are `#[serde(default)]`
    /// so responses from older supervisors that omitted them still parse.
    /// The external supervisor binary leaves them `None` (its own stdout
    /// carries the JSON response, so it cannot also stream the child's raw
    /// output); the in-process `run` fills them so the `exec` tool can
    /// return output to the model.
    Completed {
        operation_id: String,
        exit_status: ExitStatusStatus,
        #[serde(default)]
        stdout: Option<String>,
        #[serde(default)]
        stderr: Option<String>,
    },
    /// The supervisor refused the operation (profile too strict, authority
    /// ceiling exceeded, unsupported platform, etc.).
    Refused { operation_id: String, reason: String },
}

/// A serializable mirror of `ExitStatus` (which isn't Serialize). Captures
/// the two things callers care about: did it succeed, and what was the code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExitStatusStatus {
    Code(i32),
    // POSIX signal termination on Unix.
    Signal(i32),
    // Unknown (platform-specific).
    Unknown,
}

impl ExitStatusStatus {
    pub fn from(status: ExitStatus) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(code) = status.code() {
                Self::Code(code)
            } else if let Some(sig) = status.signal() {
                Self::Signal(sig)
            } else {
                Self::Unknown
            }
        }
        #[cfg(not(unix))]
        {
            status.code().map(Self::Code).unwrap_or(Self::Unknown)
        }
    }

    pub fn success(&self) -> bool {
        matches!(self, Self::Code(0))
    }
}

/// The Agent's handle to the Sandbox Runtime supervisor.
///
/// Today this is an *in-process* client: `run` builds a `Command` and hands
/// it to the platform enforcer (`enforce_seatbelt`). The point of routing
/// through this client — rather than calling `enforce_seatbelt` directly — is
/// that the boundary is now explicit: swapping in an out-of-process
/// supervisor (fork+exec a trusted helper that speaks the
/// `SandboxRuntimeRequest`/`Response` protocol over a pipe) is a change
/// confined to `run`, not spread across the codebase.
#[derive(Debug, Clone)]
pub struct SandboxRuntimeClient {
    /// Whether to refuse operations whose authority ceiling is 0
    /// (uninitialized). Defaults to false (permissive) for Phase-1 back-compat.
    enforce_authority_ceiling: bool,
    /// Optional path to the external supervisor binary used by `run_external`.
    /// When unset, `run_external` looks for `grodex-supervisor` in `PATH`.
    supervisor_path: Option<String>,
    /// Backend kind selection: in-process (default) or external-supervisor.
    backend: BackendKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    InProcess,
    #[cfg(target_os = "macos")]
    External,
}

#[allow(clippy::derivable_impls)]
impl Default for SandboxRuntimeClient {
    fn default() -> Self {
        Self {
            enforce_authority_ceiling: false,
            supervisor_path: None,
            backend: BackendKind::InProcess,
        }
    }
}

impl SandboxRuntimeClient {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(target_os = "macos")]
    pub fn new_external(
        supervisor_binary: std::path::PathBuf,
        timeout_ms: u64,
    ) -> Result<Self, crate::supervisor::client::SandboxError> {
        let _ = timeout_ms;
        Ok(Self {
            enforce_authority_ceiling: false,
            supervisor_path: Some(supervisor_binary.to_string_lossy().into_owned()),
            backend: BackendKind::External,
        })
    }

    /// Require non-zero authority ceiling on every operation (fail-closed for
    /// sub-agents that forgot to inherit one).
    pub fn enforce_authority_ceiling(mut self) -> Self {
        self.enforce_authority_ceiling = true;
        self
    }

    /// Configure the external supervisor binary path used by `run_external`.
    /// When unset, `run_external` searches `PATH` for `grodex-supervisor`.
    pub fn with_supervisor_path(mut self, path: impl Into<String>) -> Self {
        self.supervisor_path = Some(path.into());
        self
    }

    /// Run a prepared operation under the sandbox. Returns the supervisor
    /// response (Completed or Refused).
    pub fn run(&self, req: SandboxRuntimeRequest) -> SandboxRuntimeResponse {
        let op = req.operation;

        // Authority-ceiling gate (invariant #12): a sub-agent that didn't
        // inherit a ceiling must not silently run with full authority.
        if self.enforce_authority_ceiling && op.agent_authority_ceiling == 0 {
            return SandboxRuntimeResponse::Refused {
                operation_id: op.operation_id,
                reason: "agent authority ceiling is 0 (not inherited); refusing to run".into(),
            };
        }

        // Build the command from the prepared operation.
        let mut cmd = Command::new(&op.program);
        for a in &op.argv {
            cmd.arg(a);
        }
        for (k, v) in &op.env {
            cmd.env(k, v);
        }
        if let Some(ref cwd) = op.cwd {
            cmd.current_dir(cwd);
        }

        // Delegate to the platform enforcer (sandbox-exec on macOS). We use
        // the *capturing* variant so the `exec` tool routing through this
        // client gets the command's stdout/stderr to return to the model, in
        // addition to the kernel-enforced deny rules. On platforms without
        // enforcement, `enforce_seatbelt_capturing` returns `Unsupported` —
        // surfaced as a Refused so the caller fails closed instead of
        // silently running unsandboxed.
        match enforce_seatbelt_capturing(&op.profile, &mut cmd) {
            Ok((status, out, err)) => SandboxRuntimeResponse::Completed {
                operation_id: op.operation_id,
                exit_status: ExitStatusStatus::from(status),
                stdout: Some(String::from_utf8_lossy(&out).into_owned()),
                stderr: Some(String::from_utf8_lossy(&err).into_owned()),
            },
            Err(SandboxEnforceError::Unsupported) => SandboxRuntimeResponse::Refused {
                operation_id: op.operation_id,
                reason: "sandbox enforcement unsupported on this platform".into(),
            },
            Err(SandboxEnforceError::BackendMissing) => SandboxRuntimeResponse::Refused {
                operation_id: op.operation_id,
                reason: "sandbox backend (sandbox-exec) not found".into(),
            },
            Err(e) => SandboxRuntimeResponse::Refused {
                operation_id: op.operation_id,
                reason: format!("{e}"),
            },
        }
    }

    /// Run a prepared operation, dispatching to the in-process enforcer or
    /// the external supervisor binary based on the client's configured
    /// backend.
    ///
    /// This is the single entry point tools (e.g. `exec`) should call: they
    /// don't need to know whether the session is running sandboxed in-process
    /// or via an out-of-process supervisor. Fail-closed in both paths — a
    /// `Refused` is returned for any spawn/IO/serialization error.
    pub fn run_dispatched(&self, req: SandboxRuntimeRequest) -> SandboxRuntimeResponse {
        match self.backend {
            BackendKind::InProcess => self.run(req),
            #[cfg(target_os = "macos")]
            BackendKind::External => self.run_external(req, None),
        }
    }

    /// Run a prepared operation by fork+execing an external supervisor binary
    /// and exchanging a `SandboxRuntimeRequest`/`Response` over stdin/stdout
    /// pipes (JSON framing).
    ///
    /// `supervisor_path` (if `Some`) overrides the client's configured path;
    /// otherwise the configured path is used; otherwise `grodex-supervisor` is
    /// searched for in `PATH`.
    ///
    /// Fail-closed: any spawn failure or I/O/serialization error is reported
    /// as a `Refused` response — the operation is *never* silently run
    /// unsandboxed.
    pub fn run_external(
        &self,
        req: SandboxRuntimeRequest,
        supervisor_path: Option<&str>,
    ) -> SandboxRuntimeResponse {
        let operation_id = req.operation.operation_id.clone();

        // Resolve the supervisor binary: explicit arg > client config > PATH.
        let binary = supervisor_path
            .map(|s| s.to_string())
            .or_else(|| self.supervisor_path.clone())
            .or_else(|| {
                which::which("grodex-supervisor")
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });

        let binary = match binary {
            Some(b) => b,
            None => {
                return SandboxRuntimeResponse::Refused {
                    operation_id,
                    reason: "supervisor binary not found (grodex-supervisor not in PATH)".into(),
                };
            }
        };

        // Spawn the supervisor with piped stdin/stdout for JSON framing.
        let mut child = match Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return SandboxRuntimeResponse::Refused {
                    operation_id,
                    reason: format!("failed to spawn supervisor {binary:?}: {e}"),
                };
            }
        };

        // Serialize the request and write it to the supervisor's stdin.
        let req_json = match serde_json::to_string(&req) {
            Ok(s) => s,
            Err(e) => {
                return SandboxRuntimeResponse::Refused {
                    operation_id,
                    reason: format!("failed to serialize request: {e}"),
                };
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(req_json.as_bytes()) {
                return SandboxRuntimeResponse::Refused {
                    operation_id,
                    reason: format!("failed to write request to supervisor: {e}"),
                };
            }
            // `stdin` dropped here → EOF on the supervisor side.
        }

        // Read the JSON response from stdout and wait for the supervisor exit.
        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => {
                return SandboxRuntimeResponse::Refused {
                    operation_id,
                    reason: format!("failed to read supervisor output: {e}"),
                };
            }
        };

        match serde_json::from_slice::<SandboxRuntimeResponse>(&output.stdout) {
            Ok(resp) => resp,
            Err(e) => SandboxRuntimeResponse::Refused {
                operation_id,
                reason: format!("failed to deserialize supervisor response: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_sandbox_types::profile::{NetworkRule, SandboxProfile};

    fn permissive_profile() -> SandboxProfile {
        SandboxProfile {
            name: "full".into(),
            read_only_paths: vec!["/".into()],
            read_write_paths: vec!["/".into()],
            deny_paths: vec![],
            network_rules: vec![NetworkRule::AllowLocal],
            allow_exec: true,
            allow_fork: true,
        }
    }

    #[test]
    fn prepared_op_builders() {
        let op = PreparedOperation::new("op-1", "echo", permissive_profile())
            .with_arg("hi")
            .with_cwd("/tmp")
            .with_env("FOO", "bar")
            .with_authority_ceiling(40)
            .with_label("exec:echo");
        assert_eq!(op.operation_id, "op-1");
        assert_eq!(op.program, "echo");
        assert_eq!(op.argv, vec!["hi"]);
        assert_eq!(op.cwd.as_deref(), Some("/tmp"));
        assert_eq!(op.env, vec![("FOO".to_string(), "bar".to_string())]);
        assert_eq!(op.agent_authority_ceiling, 40);
        assert_eq!(op.label.as_deref(), Some("exec:echo"));
    }

    /// On non-macOS the client refuses (Unsupported) rather than silently
    /// running unsandboxed. On macOS with sandbox-exec present + a permissive
    /// "full" profile, echo should complete successfully; if sandbox-exec is
    /// missing the client refuses with BackendMissing.
    #[test]
    fn run_completes_or_refuses_no_silent_unsandboxed() {
        let client = SandboxRuntimeClient::new();
        let op = PreparedOperation::new("op-2", "echo", permissive_profile())
            .with_arg("hello");
        let resp = client.run(SandboxRuntimeRequest { operation: op });
        match resp {
            SandboxRuntimeResponse::Completed { exit_status, .. } => {
                assert!(exit_status.success(), "echo under a permissive profile should succeed");
            }
            SandboxRuntimeResponse::Refused { reason, .. } => {
                // Acceptable refusal reasons: unsupported platform or missing
                // backend. NOT "ran unsandboxed".
                assert!(
                    reason.contains("unsupported") || reason.contains("not found") || reason.contains("sandbox"),
                    "unexpected refusal reason: {reason}"
                );
            }
        }
    }

    /// Authority-ceiling enforcement: a sub-agent op with ceiling 0 must be
    /// refused when the gate is on.
    #[test]
    fn authority_ceiling_gate_refuses_zero() {
        let client = SandboxRuntimeClient::new().enforce_authority_ceiling();
        let op = PreparedOperation::new("op-3", "echo", permissive_profile())
            .with_arg("x") // ceiling left at 0
            .with_authority_ceiling(0);
        let resp = client.run(SandboxRuntimeRequest { operation: op });
        match resp {
            SandboxRuntimeResponse::Refused { reason, .. } => {
                assert!(reason.contains("authority ceiling"), "reason: {reason}");
            }
            other => panic!("expected Refused for ceiling=0, got {other:?}"),
        }
    }

    /// Wire types survive a JSON serialize→deserialize round-trip.
    #[test]
    fn wire_types_json_roundtrip() {
        // PreparedOperation / SandboxRuntimeRequest.
        let op = PreparedOperation::new("op-rt", "echo", permissive_profile())
            .with_arg("hi")
            .with_cwd("/tmp")
            .with_env("FOO", "bar")
            .with_authority_ceiling(40)
            .with_label("exec:echo");
        let req = SandboxRuntimeRequest { operation: op };

        let json = serde_json::to_string(&req).expect("serialize request");
        let back: SandboxRuntimeRequest =
            serde_json::from_str(&json).expect("deserialize request");
        assert_eq!(back.operation.operation_id, "op-rt");
        assert_eq!(back.operation.program, "echo");
        assert_eq!(back.operation.argv, vec!["hi"]);
        assert_eq!(back.operation.cwd.as_deref(), Some("/tmp"));
        assert_eq!(
            back.operation.env,
            vec![("FOO".to_string(), "bar".to_string())]
        );
        assert_eq!(back.operation.agent_authority_ceiling, 40);
        assert_eq!(back.operation.label.as_deref(), Some("exec:echo"));

        // SandboxRuntimeResponse::Completed.
        let completed = SandboxRuntimeResponse::Completed {
            operation_id: "op-rt".into(),
            exit_status: ExitStatusStatus::Code(0),
            stdout: Some("hello\n".into()),
            stderr: None,
        };
        let json = serde_json::to_string(&completed).expect("serialize completed");
        let back: SandboxRuntimeResponse =
            serde_json::from_str(&json).expect("deserialize completed");
        match back {
            SandboxRuntimeResponse::Completed {
                operation_id,
                exit_status,
                stdout,
                stderr,
            } => {
                assert_eq!(operation_id, "op-rt");
                assert!(exit_status.success());
                assert_eq!(stdout.as_deref(), Some("hello\n"));
                assert_eq!(stderr, None);
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // Old supervisor responses without stdout/stderr must still parse
        // (#[serde(default)] back-compat).
        let legacy = r#"{"Completed":{"operation_id":"op-legacy","exit_status":{"Code":0}}}"#;
        let back: SandboxRuntimeResponse =
            serde_json::from_str(legacy).expect("deserialize legacy completed");
        match back {
            SandboxRuntimeResponse::Completed { operation_id, stdout, stderr, .. } => {
                assert_eq!(operation_id, "op-legacy");
                assert_eq!(stdout, None);
                assert_eq!(stderr, None);
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // SandboxRuntimeResponse::Refused.
        let refused = SandboxRuntimeResponse::Refused {
            operation_id: "op-rt".into(),
            reason: "nope".into(),
        };
        let json = serde_json::to_string(&refused).expect("serialize refused");
        let back: SandboxRuntimeResponse =
            serde_json::from_str(&json).expect("deserialize refused");
        match back {
            SandboxRuntimeResponse::Refused { operation_id, reason } => {
                assert_eq!(operation_id, "op-rt");
                assert_eq!(reason, "nope");
            }
            other => panic!("expected Refused, got {other:?}"),
        }

        // ExitStatusStatus variants round-trip.
        let code = ExitStatusStatus::Code(7);
        let back: ExitStatusStatus =
            serde_json::from_str(&serde_json::to_string(&code).unwrap()).unwrap();
        assert!(matches!(back, ExitStatusStatus::Code(7)));

        let sig = ExitStatusStatus::Signal(9);
        let back: ExitStatusStatus =
            serde_json::from_str(&serde_json::to_string(&sig).unwrap()).unwrap();
        assert!(matches!(back, ExitStatusStatus::Signal(9)));

        let unk = ExitStatusStatus::Unknown;
        let back: ExitStatusStatus =
            serde_json::from_str(&serde_json::to_string(&unk).unwrap()).unwrap();
        assert!(matches!(back, ExitStatusStatus::Unknown));
    }

    /// `run_external` is fail-closed: a missing supervisor binary must yield a
    /// `Refused` response, never a silent unsandboxed run.
    #[test]
    fn run_external_refused_when_supervisor_missing() {
        let client = SandboxRuntimeClient::new();
        let op = PreparedOperation::new("op-ext", "echo", permissive_profile());
        let resp = client.run_external(
            SandboxRuntimeRequest { operation: op },
            Some("/definitely/does/not/exist/grodex-supervisor"),
        );
        match resp {
            SandboxRuntimeResponse::Refused {
                operation_id,
                reason,
            } => {
                assert_eq!(operation_id, "op-ext");
                assert!(!reason.is_empty(), "refusal reason should be non-empty");
            }
            other => panic!("expected Refused for missing supervisor, got {other:?}"),
        }
    }
}
