//! grodex-supervisor — the external sandbox runtime supervisor binary.
//!
//! Design Doc 13: the Trusted Sandbox Supervisor is an **independent process**
//! the Agent talks to over a JSON pipe. This binary is that process.
//!
//! Protocol (single-request, synchronous):
//!   1. Read one `SandboxRuntimeRequest` JSON object from stdin (until EOF).
//!   2. Deserialize and validate the `PreparedOperation`.
//!   3. Execute the command under sandbox enforcement (`enforce_seatbelt`).
//!   4. Write one `SandboxRuntimeResponse` JSON object to stdout.
//!   5. Exit (one request per process — the client spawns a fresh supervisor
//!      per operation; a long-running mode can be added later).
//!
//! Fail-closed: any deserialization, spawn, or enforcement error produces a
//! `Refused` response — the operation is **never** silently run unsandboxed.
//!
//! The binary is intentionally minimal: it delegates to the same
//! `enforce_seatbelt` platform enforcer the in-process client uses, but runs
//! in a separate process so the Agent's address space is isolated from the
//! spawned command. Swapping in Landlock/seccomp/Windows Job later only
//! changes `enforce_seatbelt`, not this binary or the wire protocol.

use std::io::{self, Read, Write};
use std::process::Command;

use grodex_sandbox::platform::{enforce_seatbelt, SandboxEnforceError};
use grodex_sandbox::runtime::{
    ExitStatusStatus, PreparedOperation, SandboxRuntimeRequest, SandboxRuntimeResponse,
};

fn main() {
    // ── Read the request from stdin ───────────────────────────────
    let mut stdin = io::stdin();
    let mut input = String::new();
    if let Err(e) = stdin.read_to_string(&mut input) {
        eprintln!("supervisor: failed to read stdin: {e}");
        // Cannot construct a response with an operation_id we don't know.
        std::process::exit(1);
    }

    let req: SandboxRuntimeRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("supervisor: failed to parse request: {e}");
            // No operation_id available — write a bare Refused and exit.
            let resp = SandboxRuntimeResponse::Refused {
                operation_id: "<unknown>".into(),
                reason: format!("invalid request JSON: {e}"),
            };
            let _ = write_response(&resp);
            std::process::exit(1);
        }
    };

    let op = req.operation;
    let operation_id = op.operation_id.clone();

    // ── Validate the operation ────────────────────────────────────
    if let Err(reason) = validate_operation(&op) {
        let resp = SandboxRuntimeResponse::Refused { operation_id, reason };
        let _ = write_response(&resp);
        std::process::exit(1);
    }

    // ── Build the command ─────────────────────────────────────────
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

    // ── Enforce the sandbox and run ───────────────────────────────
    let response = match enforce_seatbelt(&op.profile, &mut cmd) {
        Ok(status) => SandboxRuntimeResponse::Completed {
            operation_id,
            exit_status: ExitStatusStatus::from(status),
        },
        Err(SandboxEnforceError::Unsupported) => SandboxRuntimeResponse::Refused {
            operation_id,
            reason: "sandbox enforcement unsupported on this platform".into(),
        },
        Err(SandboxEnforceError::BackendMissing) => SandboxRuntimeResponse::Refused {
            operation_id,
            reason: "sandbox backend (sandbox-exec) not found".into(),
        },
        Err(e) => SandboxRuntimeResponse::Refused {
            operation_id,
            reason: format!("{e}"),
        },
    };

    // ── Write the response to stdout ──────────────────────────────
    if let Err(e) = write_response(&response) {
        eprintln!("supervisor: failed to write response: {e}");
        std::process::exit(1);
    }

    // Exit code mirrors the sandboxed command's success/failure.
    match &response {
        SandboxRuntimeResponse::Completed { exit_status, .. } => {
            if exit_status.success() {
                std::process::exit(0);
            } else {
                std::process::exit(1);
            }
        }
        SandboxRuntimeResponse::Refused { .. } => std::process::exit(2),
    }
}

/// Validate a prepared operation before execution.
///
/// Checks that the program is non-empty and the authority ceiling is
/// non-zero (fail-closed for sub-agents that forgot to inherit one).
fn validate_operation(op: &PreparedOperation) -> Result<(), String> {
    if op.program.is_empty() {
        return Err("operation program is empty".into());
    }
    if op.agent_authority_ceiling == 0 {
        return Err("agent authority ceiling is 0 (not inherited)".into());
    }
    Ok(())
}

/// Serialize and write a response to stdout.
fn write_response(resp: &SandboxRuntimeResponse) -> io::Result<()> {
    let json = serde_json::to_string(resp).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(json.as_bytes())?;
    stdout.flush()?;
    Ok(())
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
    fn validate_rejects_empty_program() {
        let op = PreparedOperation::new("op-1", "", permissive_profile())
            .with_authority_ceiling(10);
        assert!(validate_operation(&op).is_err());
    }

    #[test]
    fn validate_rejects_zero_authority_ceiling() {
        let op = PreparedOperation::new("op-2", "echo", permissive_profile())
            .with_authority_ceiling(0);
        assert!(validate_operation(&op).is_err());
    }

    #[test]
    fn validate_accepts_valid_operation() {
        let op = PreparedOperation::new("op-3", "echo", permissive_profile())
            .with_authority_ceiling(10);
        assert!(validate_operation(&op).is_ok());
    }

    #[test]
    fn request_response_roundtrip() {
        let op = PreparedOperation::new("op-rt", "echo", permissive_profile())
            .with_arg("hi")
            .with_authority_ceiling(10);
        let req = SandboxRuntimeRequest { operation: op };

        let json = serde_json::to_string(&req).unwrap();
        let back: SandboxRuntimeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.operation.operation_id, "op-rt");
        assert_eq!(back.operation.program, "echo");
    }

    #[test]
    fn write_response_produces_valid_json() {
        let resp = SandboxRuntimeResponse::Completed {
            operation_id: "op-w".into(),
            exit_status: ExitStatusStatus::Code(0),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: SandboxRuntimeResponse = serde_json::from_str(&json).unwrap();
        match back {
            SandboxRuntimeResponse::Completed { operation_id, exit_status } => {
                assert_eq!(operation_id, "op-w");
                assert!(exit_status.success());
            }
            _ => panic!("expected Completed"),
        }
    }
}
