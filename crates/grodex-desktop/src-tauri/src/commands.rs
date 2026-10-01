//! `#[tauri::command]` surface exposed to the React frontend.

use std::collections::HashMap;
use std::sync::{mpsc, Mutex};

use tauri::State;

use crate::sessions::{self, ConfigSummary, SessionSummary};
use crate::transport::{self, ControlMsg};
use std::path::{Component, Path};

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
    // Clone the Sender out of the lock and drop the guard BEFORE
    // send_and_wait — otherwise the global Mutex is held for up to 60s
    // (the reply timeout), serializing every command behind the one
    // currently waiting. A ResolveApproval stuck behind a streaming
    // burst would hang the UI.
    let tx = state.0.lock().map_err(|_| "transport state 被污染".to_string())?.clone();
    transport::send_and_wait(&tx, |reply| ControlMsg::Command { cmd, reply })
}

/// Ensure a `grodex serve` process is running, spawning one in `cwd` only if
/// none exists. Returns true if a spawn happened. Used before opening a
/// session so ResumeSession always has a live process to talk to.
#[tauri::command]
pub fn ensure_agent(state: State<'_, TransportState>, cwd: String) -> Result<bool, String> {
    let workspace = transport::normalize_workspace(&cwd)?;
    let tx = state.0.lock().map_err(|_| "transport state 被污染".to_string())?.clone();
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
    let tx = state.0.lock().map_err(|_| "transport state 被污染".to_string())?.clone();
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

/// Read the persisted `[rules]` section from `~/.grodex/config.toml` so the
/// frontend can derive the current approval mode at startup. Tools not
/// present in the file are absent from the map (the frontend falls back to
/// its defaults for those).
#[tauri::command]
pub fn get_tool_permissions() -> HashMap<String, String> {
    let Some(home) = dirs::home_dir() else {
        return HashMap::new();
    };
    let config_path = home.join(".grodex").join("config.toml");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return HashMap::new();
    };
    parse_rules_section(&content)
}

/// Cap for the file preview command — larger files are rejected rather than
/// being slurped into the webview.
const MAX_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;

/// Read a UTF-8 text file inside the workspace for the frontend preview.
///
/// Security model: the requested path is joined onto the canonicalized
/// workspace root and re-canonicalized, so `..` components AND symlink
/// escapes both resolve outside the root and are rejected. Only regular
/// files under the size cap are served, and only if the bytes are valid
/// UTF-8 (binary files error instead of rendering garbage).
#[tauri::command]
pub fn preview_file(workspace: String, path: String) -> Result<String, String> {
    let root = transport::normalize_workspace(&workspace)?;
    let root_canon = root
        .canonicalize()
        .map_err(|e| format!("无法解析工作区目录: {e}"))?;

    let rel = path.trim().trim_start_matches('/');
    if rel.is_empty() {
        return Err("文件路径不能为空".to_string());
    }
    let rel_path = Path::new(rel);
    let traverses_up = rel_path
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if traverses_up {
        return Err("非法文件路径".to_string());
    }

    let target = root.join(rel_path);
    let target_canon = target
        .canonicalize()
        .map_err(|_| format!("文件不存在: {}", rel))?;
    if !target_canon.starts_with(&root_canon) {
        return Err("文件位于工作区之外，拒绝读取".to_string());
    }

    let meta = std::fs::metadata(&target_canon)
        .map_err(|e| format!("无法读取文件信息: {e}"))?;
    if !meta.is_file() {
        return Err("目标不是普通文件".to_string());
    }
    if meta.len() > MAX_PREVIEW_BYTES {
        return Err(format!(
            "文件超过 {} MB 预览上限",
            MAX_PREVIEW_BYTES / 1024 / 1024
        ));
    }

    let bytes = std::fs::read(&target_canon).map_err(|e| format!("读取文件失败: {e}"))?;
    String::from_utf8(bytes).map_err(|_| "文件不是有效的 UTF-8 文本（可能为二进制文件）".to_string())
}

/// Parse `key = "value"` pairs between the `[rules]` header and the next
/// section header (or EOF). Malformed lines are skipped.
fn parse_rules_section(content: &str) -> HashMap<String, String> {
    let mut rules = HashMap::new();
    let mut in_rules = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_rules = trimmed == "[rules]";
            continue;
        }
        if !in_rules || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = trimmed.split_once('=') {
            let key = key.trim();
            let val = val.trim().trim_matches('"').trim();
            if !key.is_empty() && !val.is_empty() {
                rules.insert(key.to_string(), val.to_string());
            }
        }
    }
    rules
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
