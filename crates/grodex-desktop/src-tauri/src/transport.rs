//! ACP transport worker for the desktop backend.
//!
//! One long-lived thread owns the `grodex serve` child process (via the
//! shared [`grodex_acp_client::StdioClient`]) and fans every inbound ACP
//! event out to the webview as a Tauri event. Frontend commands arrive on
//! a control channel; each carries a reply channel so the command handler
//! can report write/spawn errors synchronously.
//!
//! Events emitted to the frontend:
//!   - `acp_event`    → [`grodex_protocol::acp::EventEnvelope`] JSON
//!   - `acp_snapshot` → [`grodex_protocol::acp::SessionSnapshotPayload`] JSON
//!   - `acp_log`      → plain log/protocol-error strings

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use grodex_acp_client::StdioClient;
use grodex_protocol::acp::Command as AcpCommand;
use tauri::{AppHandle, Emitter};

/// Control messages the frontend pushes to the agent worker.
pub enum ControlMsg {
    /// Send one ACP command to the agent (Prompt / Steer / Cancel /
    /// ResolveApproval / ResumeSession / …).
    Command {
        cmd: AcpCommand,
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Drop the current agent process and spawn a fresh `grodex serve` in
    /// `cwd` (i.e. "new session / switch workspace").
    Respawn {
        cwd: PathBuf,
        reply: mpsc::Sender<Result<(), String>>,
    },
    /// Ensure an agent process is running, spawning one in `cwd` only if
    /// there is none. Replies Ok(true) if a spawn happened, Ok(false) if a
    /// process was already running. Used before ResumeSession so opening a
    /// session never fails with "agent 尚未启动".
    EnsureRunning {
        cwd: PathBuf,
        reply: mpsc::Sender<Result<bool, String>>,
    },
    /// Stop the worker thread (app teardown). `ack` is signalled once the
    /// agent child has been gracefully shut down (stdin EOF → drain → reap),
    /// so the caller can wait for convergence instead of orphaning `serve`.
    Shutdown {
        ack: Option<mpsc::Sender<()>>,
    },
}

/// Resolve the `grodex` binary path. Precedence:
///   1. `GRODEX_BIN` env var
///   2. sibling of the running executable (`target/debug/grodex` in dev)
///   3. workspace-relative `target/debug/grodex`
///   4. `grodex` on `PATH`
pub fn resolve_grodex_bin() -> PathBuf {
    if let Ok(p) = std::env::var("GRODEX_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("grodex");
            if cand.exists() {
                return cand;
            }
        }
    }
    let from_manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/grodex");
    if from_manifest.exists() {
        return from_manifest;
    }
    PathBuf::from("grodex")
}

fn emit_log(app: &AppHandle, line: String) {
    let _ = app.emit("acp_log", line);
}

/// Spawn a fresh `grodex serve` child bound to `cwd`.
fn spawn_serve(bin_str: &str, cwd: &PathBuf) -> Result<StdioClient, String> {
    if !cwd.exists() {
        return Err(format!("工作目录不存在: {}", cwd.display()));
    }
    StdioClient::spawn_agent_subprocess_in(bin_str, &["serve"], cwd).map_err(|e| {
        format!(
            "无法启动 agent 进程（{bin_str}）。请先 cargo build -p grodex-cli，或用 GRODEX_BIN 指定 grodex 路径：{e}"
        )
    })
}

/// Run the agent worker until the control channel disconnects or Shutdown.
pub fn run_agent_worker(
    rx: mpsc::Receiver<ControlMsg>,
    app: AppHandle,
    bin: PathBuf,
) {
    let bin_str = bin.to_string_lossy().to_string();
    let mut client: Option<StdioClient> = None;
    // The canonical workspace the live agent is bound to. Guards against
    // cross-workspace reuse: a session whose journal lives in project B must
    // never run tools/sandbox rooted in project A.
    let mut active_cwd: Option<PathBuf> = None;

    loop {
        match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(ControlMsg::Shutdown { ack }) => {
                // Drop the client so StdioClient's graceful shutdown runs
                // (stdin EOF → serve drains → SIGKILL fallback), then ack.
                drop(client.take());
                active_cwd = None;
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
                break;
            }
            Ok(ControlMsg::Respawn { cwd, reply }) => {
                // Dropping the old client triggers StdioClient's graceful
                // shutdown (stdin EOF → serve drains → SIGKILL fallback).
                drop(client.take());
                active_cwd = None;
                match spawn_serve(&bin_str, &cwd) {
                    Ok(c) => {
                        client = Some(c);
                        active_cwd = Some(cwd.clone());
                        let _ = reply.send(Ok(()));
                        emit_log(
                            &app,
                            format!(
                                "[desktop] 已启动 grodex serve · workspace={}",
                                cwd.display()
                            ),
                        );
                    }
                    Err(msg) => {
                        let _ = reply.send(Err(msg.clone()));
                        emit_log(&app, msg);
                    }
                }
            }
            Ok(ControlMsg::EnsureRunning { cwd, reply }) => {
                // Respawn unless a live agent is already bound to this exact
                // canonical workspace. A dead child or a different cwd both
                // force a respawn (cross-workspace safety + crash self-heal).
                let alive_and_bound = match client.as_mut() {
                    Some(c) => c.is_alive() && active_cwd.as_ref() == Some(&cwd),
                    None => false,
                };
                if alive_and_bound {
                    let _ = reply.send(Ok(false));
                } else {
                    drop(client.take());
                    active_cwd = None;
                    match spawn_serve(&bin_str, &cwd) {
                        Ok(c) => {
                            client = Some(c);
                            active_cwd = Some(cwd.clone());
                            let _ = reply.send(Ok(true));
                            emit_log(
                                &app,
                                format!(
                                    "[desktop] 已启动 grodex serve · workspace={}",
                                    cwd.display()
                                ),
                            );
                        }
                        Err(msg) => {
                            let _ = reply.send(Err(msg));
                        }
                    }
                }
            }
            Ok(ControlMsg::Command { cmd, reply }) => {
                eprintln!("[desktop] received command: type={:?}", std::mem::discriminant(&cmd));
                // If the child died, drop it (so the next EnsureRunning
                // respawns) and surface a clear error instead of writing to a
                // broken stdin pipe.
                let dead = match client.as_mut() {
                    Some(c) => !c.is_alive(),
                    None => false,
                };
                if dead {
                    eprintln!("[desktop] agent dead, dropping client");
                    drop(client.take());
                    active_cwd = None;
                    let _ = reply
                        .send(Err("agent 进程已退出，请重新打开会话以重启".to_string()));
                } else {
                    let res = match client.as_mut() {
                        Some(c) => {
                            eprintln!("[desktop] sending acp command to agent");
                            let r = c.send_acp_command(&cmd).map_err(|e| e.to_string());
                            eprintln!("[desktop] send_acp_command result: {:?}", r.as_ref().map(|_| "ok").map_err(|e| e.as_str()));
                            r
                        }
                        None => Err(
                            "agent 进程尚未启动：请先新建任务并选择工作目录".to_string(),
                        ),
                    };
                    let _ = reply.send(res);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Drain whatever the agent produced since the last tick.
        if let Some(c) = client.as_mut() {
            let mut evt_count = 0;
            loop {
                match c.poll_event(Duration::ZERO) {
                    Some(env) => {
                        evt_count += 1;
                        if evt_count <= 3 {
                            let dbg = format!("{:?}", env.content);
                            let type_name = dbg.split('(').next().unwrap_or("?");
                            eprintln!("[desktop] emitting acp_event seq={} type={}", env.seq, type_name);
                        }
                        let _ = app.emit("acp_event", &env);
                    }
                    None => break,
                }
            }
            if evt_count > 3 {
                eprintln!("[desktop] ... and {} more events this tick", evt_count - 3);
            }
            for snap in c.take_snapshots() {
                eprintln!("[desktop] emitting acp_snapshot items={}", snap.items.len());
                let _ = app.emit("acp_snapshot", &snap);
            }
            for line in c.take_pending_logs() {
                emit_log(&app, line);
            }
            for line in c.take_protocol_errors() {
                emit_log(&app, line);
            }
        }
    }

    // Best-effort: tell the UI the worker is gone.
    let _ = app.emit("acp_log", "[desktop] transport worker 已停止");
}

/// Validate/normalize a workspace path supplied by the frontend picker.
pub fn normalize_workspace(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("工作目录不能为空".to_string());
    }
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| "无法解析 HOME".to_string())?
            .join(rest)
    } else {
        PathBuf::from(raw)
    };
    if !expanded.is_dir() {
        return Err(format!("不是有效目录: {}", expanded.display()));
    }
    // Canonicalize so a workspace has one stable identity regardless of
    // relative path / `..` / symlink spelling. This is what makes the
    // cross-workspace comparison in the worker reliable.
    expanded
        .canonicalize()
        .map_err(|e| format!("无法解析工作目录 {}: {e}", expanded.display()))
}

/// Send a control message and wait (blocking) for the worker's reply.
pub fn send_and_wait(
    tx: &mpsc::Sender<ControlMsg>,
    make: impl FnOnce(mpsc::Sender<Result<(), String>>) -> ControlMsg,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = mpsc::channel::<Result<(), String>>();
    let msg = make(reply_tx);
    tx.send(msg).map_err(|_| "transport worker 已停止".to_string())?;
    match reply_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(res) => res,
        Err(_) => Err("等待 transport worker 响应超时".to_string()),
    }
}

/// Send an EnsureRunning control message and wait for its spawn/no-op reply.
pub fn ensure_running(
    tx: &mpsc::Sender<ControlMsg>,
    cwd: &PathBuf,
) -> Result<bool, String> {
    let (reply_tx, reply_rx) = mpsc::channel::<Result<bool, String>>();
    tx.send(ControlMsg::EnsureRunning {
        cwd: cwd.clone(),
        reply: reply_tx,
    })
    .map_err(|_| "transport worker 已停止".to_string())?;
    match reply_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(res) => res,
        Err(_) => Err("等待 transport worker 响应超时".to_string()),
    }
}

/// Locate the `~/.grodex/sessions` rollout root (mirrors
/// `grodex_rollout::store::FileRolloutStore::default_dir`).
pub fn sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grodex")
        .join("sessions")
}
