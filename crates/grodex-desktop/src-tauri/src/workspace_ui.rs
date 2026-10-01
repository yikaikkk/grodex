//! Workspace file-tree and search commands backing `WorkspacePanel`.
//!
//! Design constraints:
//! - The tree is LAZY: the frontend asks for one directory's entries at a
//!   time, so opening the panel never walks the whole project.
//! - Both search modes are bounded (visited-file budget, per-file size cap,
//!   result cap) so a huge workspace cannot freeze the UI thread.
//! - All paths are validated to stay inside the workspace: `..`, absolute
//!   components and symlinked directories that canonicalize outside the
//!   root are rejected or skipped.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::transport::normalize_workspace;

/// Per-directory listing cap (tree mode).
const MAX_TREE_ENTRIES: usize = 500;
/// Total matches returned by either search mode.
const MAX_SEARCH_RESULTS: usize = 200;
/// Files larger than this are skipped by content search.
const MAX_SEARCH_FILE_BYTES: u64 = 512 * 1024;
/// Total files visited per search — hard walk budget.
const MAX_WALK_FILES: usize = 20_000;
/// Per-file match cap in content search.
const MAX_MATCHES_PER_FILE: usize = 10;
/// Directory names never descended into during walks.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "__pycache__",
    ".next",
    ".venv",
    ".grodex",
];

#[derive(Serialize)]
pub struct WorkspaceEntryJson {
    /// File name (last component).
    pub name: String,
    /// Path relative to the workspace root, '/'-separated. The frontend
    /// sends this back when expanding a directory.
    pub path: String,
    pub is_dir: bool,
    /// Lowercased extension without the dot, absent for directories.
    pub ext: Option<String>,
}

#[derive(Serialize)]
pub struct WorkspaceMatchJson {
    /// Path relative to the workspace root ('/'-separated).
    pub path: String,
    /// 1-based line number (content search only).
    pub line: Option<u32>,
    /// 1-based column of the match start (content search only).
    pub column: Option<u32>,
    /// The matched line, trimmed to a displayable width.
    pub text: Option<String>,
    pub is_dir: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Filename,
    Content,
}

/// Validate a workspace-relative path from the frontend: reject `..`,
/// absolute and prefix components. Returns the relative PathBuf.
fn rel_path(rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim().trim_start_matches('/');
    if rel.is_empty() {
        return Ok(PathBuf::new());
    }
    let p = Path::new(rel);
    let suspicious = p
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir));
    if suspicious {
        return Err("非法路径".to_string());
    }
    Ok(p.to_path_buf())
}

/// Resolve `root + rel` canonically and verify it stays inside the
/// canonicalized root (defeats symlink escape for the final component).
fn resolve_inside(root_canon: &Path, rel: &Path) -> Result<PathBuf, String> {
    let joined = if rel.as_os_str().is_empty() {
        root_canon.to_path_buf()
    } else {
        root_canon.join(rel)
    };
    let canon = joined
        .canonicalize()
        .map_err(|e| format!("路径不存在或无法访问: {e}"))?;
    if !canon.starts_with(root_canon) {
        return Err("路径位于工作区之外，拒绝访问".to_string());
    }
    Ok(canon)
}

/// List one directory's entries (lazy tree). `path` is the workspace-
/// relative directory, empty for the root.
#[tauri::command]
pub fn list_workspace_entries(
    workspace: String,
    path: Option<String>,
) -> Result<Vec<WorkspaceEntryJson>, String> {
    let root = normalize_workspace(&workspace)?;
    let root_canon = root
        .canonicalize()
        .map_err(|e| format!("无法解析工作区目录: {e}"))?;
    let rel = rel_path(path.as_deref().unwrap_or(""))?;
    let dir = resolve_inside(&root_canon, &rel)?;

    let mut dirs: Vec<WorkspaceEntryJson> = Vec::new();
    let mut files: Vec<WorkspaceEntryJson> = Vec::new();
    let read = std::fs::read_dir(&dir).map_err(|e| format!("无法读取目录: {e}"))?;
    for entry in read.flatten().take(MAX_TREE_ENTRIES) {
        let name = entry.file_name().to_string_lossy().to_string();
        // Hidden files/dirs are skipped in the tree UI.
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let is_dir = ft.is_dir();
        let child_rel = if rel.as_os_str().is_empty() {
            name.clone()
        } else {
            format!("{}/{}", rel.to_string_lossy(), name)
        };
        let ext = if is_dir {
            None
        } else {
            Path::new(&name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
        };
        let item = WorkspaceEntryJson {
            name,
            path: child_rel,
            is_dir,
            ext,
        };
        if is_dir {
            dirs.push(item);
        } else {
            files.push(item);
        }
    }
    dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    dirs.append(&mut files);
    Ok(dirs)
}

/// Search the workspace. `mode` selects filename (name contains query) or
/// content (line contains query, case-insensitive). Bounded as described
/// in the module docs.
#[tauri::command]
pub fn search_workspace(
    workspace: String,
    query: String,
    mode: SearchMode,
) -> Result<Vec<WorkspaceMatchJson>, String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let root = normalize_workspace(&workspace)?;
    let root_canon = root
        .canonicalize()
        .map_err(|e| format!("无法解析工作区目录: {e}"))?;

    let mut results = Vec::new();
    let mut visited = 0usize;
    // Iterative DFS stack of canonical dirs + their rel paths.
    let mut stack: Vec<(PathBuf, String)> = vec![(root_canon.clone(), String::new())];

    'walk: while let Some((dir, rel)) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        for entry in read.flatten() {
            visited += 1;
            if visited > MAX_WALK_FILES || results.len() >= MAX_SEARCH_RESULTS {
                break 'walk;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel, name)
            };
            let is_dir = ft.is_dir();
            if is_dir {
                stack.push((entry.path(), child_rel));
                continue;
            }
            match mode {
                SearchMode::Filename => {
                    if name.to_lowercase().contains(&query) {
                        results.push(WorkspaceMatchJson {
                            path: child_rel,
                            line: None,
                            column: None,
                            text: None,
                            is_dir: false,
                        });
                    }
                }
                SearchMode::Content => {
                    search_file_content(
                        &entry.path(),
                        &child_rel,
                        &query,
                        &mut results,
                    );
                }
            }
            if results.len() >= MAX_SEARCH_RESULTS {
                break 'walk;
            }
        }
    }
    Ok(results)
}

/// Append content matches from one file, honoring per-file and total caps.
/// Files that fail to read (binary, permissions, too large) are skipped.
fn search_file_content(
    path: &Path,
    rel: &str,
    query: &str,
    results: &mut Vec<WorkspaceMatchJson>,
) {
    let Ok(meta) = std::fs::metadata(path) else { return };
    if !meta.is_file() || meta.len() > MAX_SEARCH_FILE_BYTES {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else { return };
    let mut in_file = 0usize;
    for (idx, line) in content.lines().enumerate() {
        if in_file >= MAX_MATCHES_PER_FILE || results.len() >= MAX_SEARCH_RESULTS {
            return;
        }
        let lower = line.to_lowercase();
        if let Some(byte_col) = lower.find(query) {
            in_file += 1;
            // Byte offset → char column (best effort; CJK-safe enough for
            // navigation since the frontend only scrolls to the line).
            let column = line[..byte_col].chars().count() + 1;
            let text = line.trim();
            let text = if text.len() > 240 {
                format!("{}…", &text[..240])
            } else {
                text.to_string()
            };
            results.push(WorkspaceMatchJson {
                path: rel.to_string(),
                line: Some((idx + 1) as u32),
                column: Some(column as u32),
                text: Some(text),
                is_dir: false,
            });
        }
    }
}
