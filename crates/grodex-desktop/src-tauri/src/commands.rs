//! `#[tauri::command]` surface exposed to the React frontend.

use std::sync::{mpsc, Mutex};

use tauri::State;

use crate::sessions::{self, ConfigSummary, SessionSummary};
use crate::transport::{self, ControlMsg};

/// Long-lived handle to the agent worker's control channel.
pub struct TransportState(pub Mutex<mpsc::Sender<ControlMsg>>);

/// Send a raw ACP `Command` (tagged JSON, see `grodex_protocol::acp::Command`)
/// to the agent process. The frontend builds the full command — including
/// `command_id` / `idempotency_key` — so prompt/steer/cancel/approve/resume
/// all share one path.
#[tauri::command]
pub fn send_command(
    state: State<'_, TransportState>,
    command: serde_json::Value,
) -> Result<(), String> {
    let cmd: grodex_protocol::acp::Command =
        serde_json::from_value(command).map_err(|e| format!("命令 JSON 不合法: {e}"))?;
    let tx = state.0.lock().map_err(|_| "transport state 被污染".to_string())?;
    transport::send_and_wait(&tx, |reply| ControlMsg::Command { cmd, reply })
}

/// Ensure a `grodex serve` process is running, spawning one in `cwd` only if
/// none exists. Returns true if a spawn happened. Used before opening a
/// session so ResumeSession always has a live process to talk to.
#[tauri::command]
pub fn ensure_agent(state: State<'_, TransportState>, cwd: String) -> Result<bool, String> {
    let workspace = transport::normalize_workspace(&cwd)?;
    let tx = state.0.lock().map_err(|_| "transport state 被污染".to_string())?;
    transport::ensure_running(&tx, &workspace)
}

/// Drop the current `grodex serve` and spawn a fresh one in `cwd`
/// ("new session"). Returns the normalized workspace path.
#[tauri::command]
pub fn new_session(
    state: State<'_, TransportState>,
    cwd: String,
) -> Result<String, String> {
    let workspace = transport::normalize_workspace(&cwd)?;
    let tx = state.0.lock().map_err(|_| "transport state 被污染".to_string())?;
    transport::send_and_wait(&tx, |reply| ControlMsg::Respawn {
        cwd: workspace.clone(),
        reply,
    })?;
    Ok(workspace.to_string_lossy().to_string())
}

/// List sessions from the rollout root, most-recently-updated first.
#[tauri::command]
pub fn list_sessions() -> Vec<SessionSummary> {
    sessions::list_sessions()
}

/// Read the effective provider/model/sandbox config for the settings modal.
#[tauri::command]
pub fn get_config() -> ConfigSummary {
    sessions::get_config()
}

/// Permanently delete a session's rollout directory on disk.
#[tauri::command]
pub fn delete_session(session_id: String) -> Result<(), String> {
    sessions::delete_session(&session_id)
}

/// Remove phantom (empty / SessionStarted-only) session dirs. Called at app
/// startup so each app open never shows a session that had no conversation.
#[tauri::command]
pub fn purge_empty_sessions() -> usize {
    sessions::purge_empty_sessions()
}
