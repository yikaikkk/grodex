use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use base64::Engine;

use super::protocol::{SupervisorRequest, SupervisorResponse};

fn write_response<W: Write>(mut w: W, resp: &SupervisorResponse) {
    if let Ok(line) = serde_json::to_string(resp) {
        let _ = w.write_all(line.as_bytes());
        let _ = w.write_all(b"\n");
        let _ = w.flush();
    }
}

fn handle_request(req: SupervisorRequest) -> SupervisorResponse {
    match req {
        SupervisorRequest::PrepareOperation { operation } => SupervisorResponse::Accepted {
            operation_id: operation.operation_id,
        },
        SupervisorRequest::EnforceSeatbelt { profile, executable } => {
            let sb_path = std::env::temp_dir().join(format!("sb-{}.sb", std::process::id()));
            match std::fs::write(&sb_path, &profile) {
                Ok(_) => {}
                Err(e) => {
                    return SupervisorResponse::Error {
                        code: "SB_WRITE".to_string(),
                        message: e.to_string(),
                    };
                }
            }
            let sb_path_str = sb_path.to_string_lossy().to_string();
            let output = Command::new("sandbox-exec")
                .arg("-f")
                .arg(&sb_path_str)
                .arg(&executable)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output();
            let _ = std::fs::remove_file(&sb_path);
            match output {
                Ok(out) => SupervisorResponse::ExecuteResult {
                    exit_code: out.status.code().unwrap_or(-1),
                    stdout_b64: base64::engine::general_purpose::STANDARD.encode(&out.stdout),
                    stderr_b64: base64::engine::general_purpose::STANDARD.encode(&out.stderr),
                },
                Err(e) => SupervisorResponse::Error {
                    code: "SANDBOX_EXEC".to_string(),
                    message: e.to_string(),
                },
            }
        }
        SupervisorRequest::ExecuteChild {
            program,
            args,
            cwd,
            env,
        } => {
            let mut cmd = Command::new(&program);
            cmd.args(&args);
            if let Some(ref d) = cwd {
                cmd.current_dir(d);
            }
            for (k, v) in &env {
                cmd.env(k, v);
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            match cmd.output() {
                Ok(out) => SupervisorResponse::ExecuteResult {
                    exit_code: out.status.code().unwrap_or(-1),
                    stdout_b64: base64::engine::general_purpose::STANDARD.encode(&out.stdout),
                    stderr_b64: base64::engine::general_purpose::STANDARD.encode(&out.stderr),
                },
                Err(e) => SupervisorResponse::Error {
                    code: "EXEC".to_string(),
                    message: e.to_string(),
                },
            }
        }
        SupervisorRequest::HealthCheck => SupervisorResponse::Pong,
        SupervisorRequest::Shutdown => {
            std::process::exit(0);
        }
    }
}

pub fn child_main() -> ! {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let reader = BufReader::new(stdin.lock());

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => std::process::exit(1),
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<SupervisorRequest>(&line) {
            Ok(req) => handle_request(req),
            Err(e) => SupervisorResponse::Error {
                code: "PARSE".to_string(),
                message: e.to_string(),
            },
        };
        write_response(stdout.lock(), &resp);
    }
    std::process::exit(0);
}
