//! Workspace snapshot — a controlled manifest of file states, captured at
//! turn (or task) boundaries so changes made OUTSIDE tracked tools
//! (exec / formatters / the user) can be detected by diffing two
//! manifests.
//!
//! Design (三层 diff 方案 §四):
//! - NOT a filesystem watcher: two point-in-time manifest scans, diffed.
//! - Scope = the session sandbox's allowed paths ∪ cwd, bounded by a file
//!   count / total-bytes budget so the snapshot can never become a
//!   performance problem on huge repos.
//! - Ignores: `.git`, `node_modules`, `target`, `dist` and dot-locked
//!   caches by default (still configurable).
//! - Deletions are discovered by manifest comparison (absent → present).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Hard caps so a manifest scan stays bounded (huge monorepos must not
/// turn the snapshot into a multi-second stall on the hot turn path).
pub const MAX_SNAPSHOT_FILES: usize = 20_000;
pub const MAX_SNAPSHOT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
/// Files larger than this are hashed streaming but content is never kept
/// in memory; snapshot records size+mtime+hash only.
const HASH_CHUNK: usize = 64 * 1024;
/// Default per-scan time budget. When exceeded, the manifest is returned
/// with `truncated: true` (the reconciler downgrades confidence).
const SCAN_TIME_BUDGET: Duration = Duration::from_secs(10);

const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", ".next", ".cache",
    "__pycache__", ".venv", "venv",
];

/// One file's observed state in a manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    pub size: u64,
    /// System-time millis since UNIX epoch (`None` when unavailable).
    pub mtime_ms: Option<u64>,
    /// SHA-256 of the content (streamed, bounded memory).
    pub content_hash: String,
}

/// A point-in-time manifest of the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub snapshot_id: String,
    pub root: PathBuf,
    pub files: HashMap<String, FileState>,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    /// True when the scan hit the file/time/byte budget — the reconciler
    /// downgrades confidence for this snapshot.
    pub truncated: bool,
}

/// Snapshot error (best-effort semantics: callers log and continue).
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("root path does not exist: {0}")]
    RootMissing(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// What a turn/task did to one path, derived from two manifests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotChangeKind {
    Created,
    Deleted,
    Updated,
    /// Same content hash but mtime/size changed (touch / chmod / re-save).
    Metadata,
}

/// One path-level difference between two snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotChange {
    pub path: String,
    pub kind: SnapshotChangeKind,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

impl WorkspaceSnapshot {
    /// Capture a manifest of `root` (bounded). `extra_paths` are followed
    /// in addition to the root tree (e.g. sandbox-allowed absolute paths
    /// outside cwd).
    pub fn capture(root: &Path, extra_paths: &[PathBuf]) -> Result<Self, SnapshotError> {
        let started = Instant::now();
        if !root.exists() {
            return Err(SnapshotError::RootMissing(root.to_path_buf()));
        }
        let mut files = HashMap::new();
        let mut truncated = false;
        let mut total_bytes: u64 = 0;

        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if files.len() >= MAX_SNAPSHOT_FILES
                || total_bytes >= MAX_SNAPSHOT_TOTAL_BYTES
                || started.elapsed() > SCAN_TIME_BUDGET
            {
                truncated = true;
                break;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let path = entry.path();
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if ft.is_dir() {
                    if DEFAULT_IGNORED_DIRS.contains(&name_str.as_ref()) {
                        continue;
                    }
                    stack.push(path);
                } else if ft.is_file() {
                    if files.len() >= MAX_SNAPSHOT_FILES {
                        truncated = true;
                        break;
                    }
                    match std::fs::metadata(&path) {
                        Ok(meta) => {
                            let size = meta.len();
                            total_bytes = total_bytes.saturating_add(size);
                            let rel = path
                                .strip_prefix(root)
                                .unwrap_or(&path)
                                .to_string_lossy()
                                .to_string();
                            files.insert(
                                rel.clone(),
                                FileState {
                                    path: rel,
                                    size,
                                    mtime_ms: meta
                                        .modified()
                                        .ok()
                                        .and_then(|t| {
                                            t.duration_since(SystemTime::UNIX_EPOCH).ok()
                                        })
                                        .map(|d| d.as_millis() as u64),
                                    content_hash: hash_file(&path),
                                },
                            );
                        }
                        Err(_) => continue,
                    }
                }
            }
        }

        // Extra absolute paths outside the root (sandbox-allowed dirs).
        for extra in extra_paths {
            if extra.is_file() {
                let rel = extra.to_string_lossy().to_string();
                if let Ok(meta) = std::fs::metadata(extra) {
                    files.entry(rel.clone()).or_insert_with(|| FileState {
                        path: rel.clone(),
                        size: meta.len(),
                        mtime_ms: meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64),
                        content_hash: hash_file(extra),
                    });
                }
            }
        }

        let snapshot_id = {
            let mut hasher = Sha256::new();
            hasher.update(root.to_string_lossy().as_bytes());
            hasher.update(chrono::Utc::now().to_rfc3339().as_bytes());
            format!("snap_{:x}", hasher.finalize())[..16].to_string()
        };
        Ok(Self {
            snapshot_id,
            root: root.to_path_buf(),
            files,
            captured_at: chrono::Utc::now(),
            truncated,
        })
    }

    /// Diff this snapshot (newer) against `before` (older).
    /// Returns path-level changes in deterministic (sorted) order.
    pub fn diff_against(&self, before: &WorkspaceSnapshot) -> Vec<SnapshotChange> {
        let mut changes = Vec::new();
        let mut paths: std::collections::BTreeSet<&String> =
            before.files.keys().collect();
        for p in self.files.keys() {
            paths.insert(p);
        }
        for path in paths {
            let b = before.files.get(path);
            let a = self.files.get(path);
            match (b, a) {
                (None, Some(after)) => changes.push(SnapshotChange {
                    path: path.clone(),
                    kind: SnapshotChangeKind::Created,
                    before_hash: None,
                    after_hash: Some(after.content_hash.clone()),
                }),
                (Some(_), None) => changes.push(SnapshotChange {
                    path: path.clone(),
                    kind: SnapshotChangeKind::Deleted,
                    before_hash: b.map(|f| f.content_hash.clone()),
                    after_hash: None,
                }),
                (Some(before_f), Some(after_f)) => {
                    if before_f.content_hash != after_f.content_hash {
                        changes.push(SnapshotChange {
                            path: path.clone(),
                            kind: SnapshotChangeKind::Updated,
                            before_hash: Some(before_f.content_hash.clone()),
                            after_hash: Some(after_f.content_hash.clone()),
                        });
                    } else if before_f.mtime_ms != after_f.mtime_ms
                        || before_f.size != after_f.size
                    {
                        changes.push(SnapshotChange {
                            path: path.clone(),
                            kind: SnapshotChangeKind::Metadata,
                            before_hash: Some(before_f.content_hash.clone()),
                            after_hash: Some(after_f.content_hash.clone()),
                        });
                    }
                }
                (None, None) => {}
            }
        }
        changes
    }
}

/// Streamed SHA-256 (bounded memory for large files); unreadable → "".
fn hash_file(path: &Path) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        match f.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => hasher.update(&buf[..n]),
        }
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn detects_created_updated_deleted() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "aaa");
        write(&dir.path().join("b.txt"), "bbb");
        let before = WorkspaceSnapshot::capture(dir.path(), &[]).unwrap();

        write(&dir.path().join("a.txt"), "aaa2"); // updated
        std::fs::remove_file(dir.path().join("b.txt")).unwrap(); // deleted
        write(&dir.path().join("c.txt"), "ccc"); // created
        let after = WorkspaceSnapshot::capture(dir.path(), &[]).unwrap();

        let changes = after.diff_against(&before);
        let find = |p: &str| changes.iter().find(|c| c.path == p).unwrap();
        assert_eq!(find("a.txt").kind, SnapshotChangeKind::Updated);
        assert_eq!(find("b.txt").kind, SnapshotChangeKind::Deleted);
        assert_eq!(find("c.txt").kind, SnapshotChangeKind::Created);
    }

    #[test]
    fn ignores_default_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("target/debug/x.o"), "binary");
        write(&dir.path().join(".git/config"), "[core]");
        write(&dir.path().join("src/main.rs"), "fn main() {}");
        let snap = WorkspaceSnapshot::capture(dir.path(), &[]).unwrap();
        assert!(snap.files.contains_key("src/main.rs"));
        assert!(!snap.files.contains_key("target/debug/x.o"));
        assert!(!snap.files.contains_key(".git/config"));
        assert!(!snap.truncated);
    }

    #[test]
    fn no_change_yields_no_diff() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("a.txt"), "same");
        let a = WorkspaceSnapshot::capture(dir.path(), &[]).unwrap();
        let b = WorkspaceSnapshot::capture(dir.path(), &[]).unwrap();
        assert!(b.diff_against(&a).is_empty());
    }

    #[test]
    fn extra_paths_are_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("out.txt"), "outside");
        let snap =
            WorkspaceSnapshot::capture(dir.path(), &[outside.path().join("out.txt")]).unwrap();
        assert!(snap.files.contains_key(&outside.path().join("out.txt").to_string_lossy().to_string()));
    }
}
