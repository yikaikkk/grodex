//! Integration test: spawn the `grodex-supervisor` binary and verify the
//! full pipe protocol (stdin JSON → stdout JSON) works end-to-end.
//!
//! This validates A1: the external supervisor process boundary is real,
//! not just a type-level assertion.

use grodex_sandbox::runtime::{
    ExitStatusStatus, PreparedOperation, SandboxRuntimeClient, SandboxRuntimeRequest,
    SandboxRuntimeResponse,
};
use grodex_sandbox_types::profile::{NetworkRule, SandboxProfile};
use std::io::Write;
use std::process::{Command, Stdio};

/// Path to the built supervisor binary (resolved by cargo at test time).
const SUPERVISOR_BIN: &str = env!("CARGO_BIN_EXE_grodex-supervisor");

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

/// Directly spawn the supervisor binary, send a request via stdin, and
/// read the response from stdout. This bypasses `SandboxRuntimeClient` to
/// test the binary itself in isolation.
#[test]
fn supervisor_binary_echo_completes_or_refuses() {
    let op = PreparedOperation::new("op-direct", "echo", permissive_profile())
        .with_arg("hello")
        .with_authority_ceiling(10);
    let req = SandboxRuntimeRequest { operation: op };
    let req_json = serde_json::to_string(&req).unwrap();

    let mut child = Command::new(SUPERVISOR_BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn supervisor");

    {
        let mut stdin = child.stdin.take().expect("stdin pipe");
        stdin
            .write_all(req_json.as_bytes())
            .expect("write request");
    }

    let output = child.wait_with_output().expect("wait for supervisor");

    let resp: SandboxRuntimeResponse =
        serde_json::from_slice(&output.stdout).expect("deserialize response");

    match resp {
        SandboxRuntimeResponse::Completed {
            operation_id,
            exit_status,
            ..
        } => {
            assert_eq!(operation_id, "op-direct");
            assert!(exit_status.success(), "echo should succeed");
        }
        SandboxRuntimeResponse::Refused { reason, .. } => {
            // On platforms without sandbox-exec, the supervisor refuses
            // (fail-closed) — this is acceptable.
            assert!(
                reason.contains("unsupported") || reason.contains("not found"),
                "unexpected refusal: {reason}"
            );
        }
    }
}

/// The supervisor must reject an operation with authority_ceiling=0.
#[test]
fn supervisor_binary_rejects_zero_authority() {
    let op = PreparedOperation::new("op-zero", "echo", permissive_profile())
        .with_authority_ceiling(0);
    let req = SandboxRuntimeRequest { operation: op };
    let req_json = serde_json::to_string(&req).unwrap();

    let mut child = Command::new(SUPERVISOR_BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn supervisor");

    {
        let mut stdin = child.stdin.take().expect("stdin pipe");
        stdin
            .write_all(req_json.as_bytes())
            .expect("write request");
    }

    let output = child.wait_with_output().expect("wait for supervisor");
    let exit_code = output.status.code().unwrap_or(-1);

    let resp: SandboxRuntimeResponse =
        serde_json::from_slice(&output.stdout).expect("deserialize response");

    match resp {
        SandboxRuntimeResponse::Refused {
            operation_id, reason, ..
        } => {
            assert_eq!(operation_id, "op-zero");
            assert!(
                reason.contains("authority ceiling"),
                "should mention authority ceiling: {reason}"
            );
            assert_eq!(exit_code, 1, "validation refusal exit code should be 1");
        }
        other => panic!("expected Refused for ceiling=0, got {other:?}"),
    }
}

/// The supervisor must reject malformed JSON input.
#[test]
fn supervisor_binary_rejects_bad_json() {
    let mut child = Command::new(SUPERVISOR_BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn supervisor");

    {
        let mut stdin = child.stdin.take().expect("stdin pipe");
        stdin
            .write_all(b"this is not json {{{")
            .expect("write bad json");
    }

    let output = child.wait_with_output().expect("wait for supervisor");

    // The supervisor should exit non-zero on bad JSON.
    assert!(
        !output.status.success(),
        "supervisor should exit non-zero on bad JSON"
    );

    // It may or may not write a response to stdout (depends on parse stage).
    // If it did write something, it should be a valid Refused response.
    if !output.stdout.is_empty() {
        if let Ok(resp) = serde_json::from_slice::<SandboxRuntimeResponse>(&output.stdout) {
            match resp {
                SandboxRuntimeResponse::Refused { reason, .. } => {
                    assert!(reason.contains("invalid request JSON"));
                }
                other => panic!("expected Refused for bad JSON, got {other:?}"),
            }
        }
    }
}

/// End-to-end through `SandboxRuntimeClient::run_external` — the full
/// A1 path: client spawns supervisor binary, sends request, gets response.
#[test]
fn run_external_through_supervisor_binary() {
    let client = SandboxRuntimeClient::new().with_supervisor_path(SUPERVISOR_BIN);
    let op = PreparedOperation::new("op-e2e", "echo", permissive_profile())
        .with_arg("world")
        .with_authority_ceiling(10);
    let resp = client.run_external(
        SandboxRuntimeRequest { operation: op },
        None, // use client's configured supervisor_path
    );
    match resp {
        SandboxRuntimeResponse::Completed {
            operation_id,
            exit_status,
            ..
        } => {
            assert_eq!(operation_id, "op-e2e");
            assert!(exit_status.success());
        }
        SandboxRuntimeResponse::Refused { reason, .. } => {
            // Fail-closed on platforms without sandbox-exec — acceptable.
            assert!(
                reason.contains("unsupported") || reason.contains("not found"),
                "unexpected refusal: {reason}"
            );
        }
    }
}

/// `run_external` with an explicit supervisor_path override arg.
#[test]
fn run_external_with_explicit_path_override() {
    let client = SandboxRuntimeClient::new(); // no configured path
    let op = PreparedOperation::new("op-override", "echo", permissive_profile())
        .with_authority_ceiling(10);
    let resp = client.run_external(
        SandboxRuntimeRequest { operation: op },
        Some(SUPERVISOR_BIN), // explicit override
    );
    match resp {
        SandboxRuntimeResponse::Completed { exit_status, .. } => {
            assert!(exit_status.success());
        }
        SandboxRuntimeResponse::Refused { reason, .. } => {
            assert!(
                reason.contains("unsupported") || reason.contains("not found"),
                "unexpected refusal: {reason}"
            );
        }
    }
}

/// A non-zero exit code from the sandboxed command is reported as
/// Completed with a non-success exit_status (not Refused).
#[test]
fn supervisor_binary_reports_nonzero_exit() {
    // `false` always exits with code 1 on Unix.
    let op = PreparedOperation::new("op-false", "false", permissive_profile())
        .with_authority_ceiling(10);
    let req = SandboxRuntimeRequest { operation: op };
    let req_json = serde_json::to_string(&req).unwrap();

    let mut child = Command::new(SUPERVISOR_BIN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn supervisor");

    {
        let mut stdin = child.stdin.take().expect("stdin pipe");
        stdin
            .write_all(req_json.as_bytes())
            .expect("write request");
    }

    let output = child.wait_with_output().expect("wait for supervisor");

    let resp: SandboxRuntimeResponse =
        serde_json::from_slice(&output.stdout).expect("deserialize response");

    match resp {
        SandboxRuntimeResponse::Completed {
            exit_status,
            ..
        } => {
            // Should be Code(1) on Unix, not success.
            assert!(!exit_status.success(), "false should fail");
            if let ExitStatusStatus::Code(code) = exit_status {
                assert_eq!(code, 1, "false exits with code 1");
            }
        }
        SandboxRuntimeResponse::Refused { reason, .. } => {
            // On platforms without sandbox-exec, this is acceptable.
            assert!(
                reason.contains("unsupported") || reason.contains("not found"),
                "unexpected refusal: {reason}"
            );
        }
    }
}
