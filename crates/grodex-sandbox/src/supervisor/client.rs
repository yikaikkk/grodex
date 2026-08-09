use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::protocol::{SupervisorRequest, SupervisorResponse};

#[derive(Debug)]
pub enum SandboxError {
    BackendMissing(String),
    IoError(String),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendMissing(m) => write!(f, "sandbox backend missing: {m}"),
            Self::IoError(m) => write!(f, "sandbox io error: {m}"),
        }
    }
}

impl std::error::Error for SandboxError {}

pub struct ExternalSupervisorClient {
    child: Child,
    #[allow(dead_code)]
    timeout_ms: u64,
}

impl ExternalSupervisorClient {
    pub fn spawn(binary: &Path, timeout_ms: u64) -> Result<Self, SandboxError> {
        let child = Command::new(binary)
            .args(["sandbox-supervisor"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| SandboxError::BackendMissing(format!("spawn supervisor: {e}")))?;
        Ok(Self { child, timeout_ms })
    }

    pub fn call(
        &mut self,
        req: SupervisorRequest,
        timeout_ms: u64,
    ) -> Result<SupervisorResponse, SandboxError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        if let Some(ref mut stdin) = self.child.stdin {
            let line =
                serde_json::to_string(&req).map_err(|e| SandboxError::IoError(e.to_string()))?;
            stdin
                .write_all(line.as_bytes())
                .and_then(|_| stdin.write_all(b"\n"))
                .and_then(|_| stdin.flush())
                .map_err(|e| SandboxError::IoError(e.to_string()))?;
        } else {
            return Err(SandboxError::BackendMissing(
                "supervisor stdin closed".into(),
            ));
        }

        let mut stdout_opt = self.child.stdout.take();
        if let Some(mut stdout) = stdout_opt.take() {
            let mut reader = BufReader::new(&mut stdout);
            let mut buf = String::new();
            let result = loop {
                if Instant::now() > deadline {
                    break Err(SandboxError::IoError(
                        "supervisor response timeout".into(),
                    ));
                }
                if let Ok(Some(_)) = self.child.try_wait() {
                    break Err(SandboxError::BackendMissing(
                        "supervisor process exited".into(),
                    ));
                }
                match reader.read_line(&mut buf) {
                    Ok(0) => {
                        break Err(SandboxError::BackendMissing(
                            "supervisor stdout EOF".into(),
                        ));
                    }
                    Ok(_) => {
                        if buf.ends_with('\n') {
                            let parsed: Result<SupervisorResponse, _> =
                                serde_json::from_str(&buf);
                            break parsed.map_err(|e| {
                                SandboxError::IoError(format!("parse response: {e}"))
                            });
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => {
                        break Err(SandboxError::IoError(e.to_string()));
                    }
                }
            };
            self.child.stdout = Some(stdout);
            result
        } else {
            Err(SandboxError::BackendMissing(
                "supervisor stdout closed".into(),
            ))
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.call(SupervisorRequest::Shutdown, 1000);
        let _ = self.child.wait();
    }
}

impl Drop for ExternalSupervisorClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
