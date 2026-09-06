//! Session enumeration + config reading for the desktop sidebar.
//!
//! grodex persists each session under `~/.grodex/sessions/<uuid>/` with a
//! `rollout.jsonl` journal. There is no "list sessions" server API yet, so
//! this module scans the rollout root and derives a lightweight summary per
//! session from the journal head (metadata + first user message).

use std::io::BufRead;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::transport::sessions_dir;

/// Maximum number of journal lines scanned to derive a title/preview.
/// Bounds IO on very long sessions (a summary only needs the head).
const SCAN_LIMIT: usize = 6000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    /// Workspace the session was opened in (from `SessionStarted` payload).
    pub workspace: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub title: String,
    pub preview: String,
    /// Always "completed" for now — crash/run state detection is a follow-up.
    pub status: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSummary {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub sandbox_profile: Option<String>,
    pub wire_protocol: &'static str,
    pub config_path: Option<String>,
}

struct RawSession {
    summary: SessionSummary,
    updated_ms: u64,
}

fn now_ms_from_system(st: std::time::SystemTime) -> u64 {
    st.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn rfc3339(st: std::time::SystemTime) -> Option<String> {
    Some(chrono::DateTime::<chrono::Utc>::from(st).to_rfc3339())
}

/// Summarize one session dir from its journal head.
fn summarize_journal(journal: &PathBuf, id: &str) -> Option<RawSession> {
    let file = std::fs::File::open(journal).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut meta: Option<(String, String, String)> = None; // (cwd, model, provider)
    let mut created_at: Option<String> = None;
    let mut first_user_text: Option<String> = None;

    for (i, line) in reader.lines().enumerate() {
        if i >= SCAN_LIMIT {
            break;
        }
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(et) = v.get("event_type").and_then(|x| x.as_str()) else {
            continue;
        };
        if created_at.is_none() {
            created_at = v
                .get("timestamp")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
        }
        if et == "SessionStarted" {
            let payload = v.get("payload");
            let cwd = payload
                .and_then(|p| p.get("cwd"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let model = payload
                .and_then(|p| p.get("model"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let provider = payload
                .and_then(|p| p.get("model_provider"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            meta = Some((cwd.unwrap_or_default(), model.unwrap_or_default(), provider.unwrap_or_default()));
        } else if et == "UserInputAccepted" && first_user_text.is_none() {
            first_user_text = v
                .get("payload")
                .and_then(|p| p.get("text"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
        }
        if meta.is_some() && first_user_text.is_some() {
            break; // head scan complete
        }
    }

    let (workspace, model, provider) = meta.unwrap_or_default();
    let user_text = first_user_text.unwrap_or_default();
    let title = first_line(&user_text)
        .map(|t| t.chars().take(80).collect::<String>())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| format!("会话 {}", &id[..id.len().min(8)]));
    let preview = user_text.chars().take(180).collect::<String>();

    let mtime = std::fs::metadata(journal).and_then(|m| m.modified()).ok();

    Some(RawSession {
        summary: SessionSummary {
            id: id.to_string(),
            workspace: if workspace.is_empty() { None } else { Some(workspace) },
            model: if model.is_empty() { None } else { Some(model) },
            provider: if provider.is_empty() { None } else { Some(provider) },
            title,
            preview,
            status: "completed".to_string(),
            created_at,
            updated_at: mtime.and_then(rfc3339),
        },
        updated_ms: mtime.map(now_ms_from_system).unwrap_or(0),
    })
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(|l| l.trim()).find(|l| !l.is_empty())
}

pub fn list_sessions() -> Vec<SessionSummary> {
    let root = sessions_dir();
    let mut out: Vec<RawSession> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    for entry in rd.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let id = match dir.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let journal = dir.join("rollout.jsonl");
        if !journal.exists() {
            continue;
        }
        if let Some(raw) = summarize_journal(&journal, &id) {
            out.push(raw);
        }
    }
    out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
    out.into_iter().map(|r| r.summary).collect()
}

/// Resolve `<sessions_root>/<id>` after validating the id is a bare UUID-like
/// dir name so a malicious value can't escape the rollout root.
pub fn session_dir(id: &str) -> Result<PathBuf, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("session id 为空".to_string());
    }
    if id.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '-')) {
        return Err(format!("非法的 session id: {id}"));
    }
    let dir = sessions_dir().join(id);
    if !dir.is_dir() {
        return Err(format!("会话目录不存在: {}", dir.display()));
    }
    Ok(dir)
}

/// Permanently delete a session's rollout directory (journal + blobs).
pub fn delete_session(id: &str) -> Result<(), String> {
    let dir = session_dir(id)?;
    std::fs::remove_dir_all(&dir)
        .map_err(|e| format!("删除会话目录失败 ({}): {e}", dir.display()))
}

fn home_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grodex")
        .join("config.toml")
}

/// Best-effort read of the effective provider/model from the home config.
/// Sandbox profile comes from the workspace or home `.grodex/config.toml`
/// when present; otherwise the UI falls back to sensible defaults.
pub fn get_config() -> ConfigSummary {
    let path = home_config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            return ConfigSummary {
                provider: None,
                model: None,
                sandbox_profile: None,
                wire_protocol: "acp_stdio",
                config_path: Some(path.to_string_lossy().to_string()),
            };
        }
    };
    let parsed = toml::from_str::<toml::Value>(&text).ok();
    let get_str = |keys: &[&str]| -> Option<String> {
        let v = parsed.as_ref()?;
        for k in keys {
            if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
                return Some(s.to_string());
            }
        }
        None
    };
    let sandbox = parsed
        .as_ref()
        .and_then(|v| v.get("sandbox"))
        .and_then(|s| s.get("profile"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    ConfigSummary {
        provider: get_str(&["provider"]),
        model: get_str(&["model_id", "model"]),
        sandbox_profile: sandbox,
        wire_protocol: "acp_stdio",
        config_path: Some(path.to_string_lossy().to_string()),
    }
}
