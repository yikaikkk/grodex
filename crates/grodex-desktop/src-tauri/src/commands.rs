//! `#[tauri::command]` surface exposed to the React frontend.

use std::collections::HashMap;
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

/// Persist the granular tool permission rules to `~/.grodex/config.toml`.
///
/// Performs a text-surgery replacement of the `[rules]` section so the rest
/// of the file (model, provider, comments, …) is preserved untouched. The
/// running `grodex serve` agent has a config-file watcher that detects this
/// write and hot-adopts the new `PermissionPolicy` via
/// `SessionCommand::AdoptPermissionPolicy` — no manual push needed.
///
/// `permissions` maps tool name → `"allow"` | `"ask"` | `"deny"`.
#[tauri::command]
pub fn update_tool_permissions(
    permissions: HashMap<String, String>,
) -> Result<(), String> {
    let home = dirs::home_dir().ok_or_else(|| "无法定位 HOME 目录".to_string())?;
    let grodex_dir = home.join(".grodex");
    let config_path = grodex_dir.join("config.toml");

    // Build the new [rules] section text.
    let mut new_section = String::from("[rules]\n");
    // Sort keys for deterministic output.
    let mut keys: Vec<&String> = permissions.keys().collect();
    keys.sort();
    for key in keys {
        let val = permissions.get(key).map(|v| v.as_str()).unwrap_or("ask");
        // Quote the value and escape any embedded quotes.
        let escaped = val.replace('"', "\\\"");
        new_section.push_str(&format!("{key} = \"{escaped}\"\n"));
    }

    // Read current content (may not exist yet).
    let current = std::fs::read_to_string(&config_path).unwrap_or_default();

    // Text surgery: find and replace the [rules] section.
    let updated = replace_rules_section(&current, &new_section);

    // Ensure ~/.grodex exists.
    std::fs::create_dir_all(&grodex_dir)
        .map_err(|e| format!("无法创建 {}: {e}", grodex_dir.display()))?;

    // Atomic write: temp file in the same dir + fsync + rename.
    let tmp_path = config_path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, &updated)
        .map_err(|e| format!("写入临时文件失败: {e}"))?;
    // fsync for durability.
    if let Ok(f) = std::fs::File::open(&tmp_path) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp_path, &config_path)
        .map_err(|e| format!("重命名临时文件失败: {e}"))?;

    Ok(())
}

/// Replace the `[rules]` section in `content` with `new_section`.
///
/// - If a `[rules]` section exists, replace from the `[rules]` header line to
///   the next section header (a line starting with `[` at column 0, excluding
///   `[rules]` itself) or EOF.
/// - If no `[rules]` section exists, append `new_section` at the end (with a
///   blank separator line if needed).
fn replace_rules_section(content: &str, new_section: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();

    // Find the [rules] section start.
    let rules_start = lines.iter().position(|line| line.trim() == "[rules]");

    match rules_start {
        Some(start_idx) => {
            // Find the next section header after [rules]: a line whose trimmed
            // form starts with '[' but is not a key=value continuation.
            let mut end_idx = lines.len();
            for (i, line) in lines.iter().enumerate().skip(start_idx + 1) {
                let trimmed = line.trim_start();
                // A new section header starts with '[' at the beginning of the
                // (trimmed) line. Blank lines and indented lines belong to [rules].
                if trimmed.starts_with('[') && trimmed != "[rules]" {
                    end_idx = i;
                    break;
                }
            }
            // Rebuild: lines before [rules] + new section + lines after end.
            let mut result = String::new();
            for line in &lines[..start_idx] {
                result.push_str(line);
                result.push('\n');
            }
            result.push_str(new_section);
            for line in &lines[end_idx..] {
                result.push_str(line);
                result.push('\n');
            }
            result
        }
        None => {
            // No [rules] section — append.
            if content.is_empty() {
                new_section.to_string()
            } else {
                let mut result = content.to_string();
                if !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push('\n');
                result.push_str(new_section);
                result
            }
        }
    }
}
