//! Change Reconciler — merges the three change sources into one trusted
//! `TurnChangeSet` (三层 diff 方案 §二/§四):
//!
//! 1. **Exact tool deltas** — from `edit_file` / `write_file` / `apply_patch`
//!    (`AppliedChangeDelta.exact = true`, with before/after content).
//! 2. **Snapshot-detected changes** — workspace manifest diff between turn
//!    boundaries (catches `exec` mutations, formatters, generated files,
//!    external/user edits).
//! 3. Attribution — every entry carries its source and confidence so the
//!    UI can answer "哪些是 Agent 造成的？哪些来自 exec 或外部？哪些只
//!    确认了 hash？"
//!
//! Merge rules (方案 §四):
//! - exact delta agrees with snapshot (or snapshot has no entry) → Exact.
//! - snapshot-only change → SnapshotDetected (content not captured — the
//!   snapshot only hashes).
//! - same path both sources but different after-hash → Conflict
//!   (in-flight mutation between tool end and snapshot capture).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::common::{AppliedChangeDelta, ChangeType, ChangedResource};
use crate::workspace_snapshot::{SnapshotChange, SnapshotChangeKind};

/// Where a change came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeSource {
    EditFile,
    WriteFile,
    ApplyPatch,
    ExecSnapshot,
    UserExternal,
    Unknown,
}

impl ChangeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeSource::EditFile => "edit_file",
            ChangeSource::WriteFile => "write_file",
            ChangeSource::ApplyPatch => "apply_patch",
            ChangeSource::ExecSnapshot => "exec_snapshot",
            ChangeSource::UserExternal => "user_external",
            ChangeSource::Unknown => "unknown",
        }
    }
}

/// How trustworthy one change record is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeConfidence {
    /// Exact before/after content captured by a tracked tool.
    Exact,
    /// Detected by manifest diff only — hashes verified, content unknown.
    SnapshotDetected,
    /// Exact tool delta and snapshot disagree (in-flight mutation).
    Conflict,
}

impl ChangeConfidence {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeConfidence::Exact => "exact",
            ChangeConfidence::SnapshotDetected => "snapshot_detected",
            ChangeConfidence::Conflict => "conflict",
        }
    }
}

/// One reconciled workspace change with provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceChange {
    pub resource_id: String,
    pub display_path: PathBuf,
    pub change_type: ChangeType,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    /// Present only when the source captured content (Exact).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_content: Option<String>,
    pub source: ChangeSource,
    pub confidence: ChangeConfidence,
}

/// The reconciled result for one turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnChangeSet {
    pub changes: Vec<WorkspaceChange>,
    pub complete: bool,
}

/// Merge exact tool deltas (already net-collapsed by the tracker) with
/// snapshot-detected changes. `exec_paths_seen` are the cwd(s) exec ran
/// in — changes under them are attributed to `ExecSnapshot`, everything
/// else to `UserExternal`.
pub fn reconcile(
    exact: &[ChangedResource],
    snapshot_changes: &[SnapshotChange],
    snapshot_root: &PathBuf,
    exec_paths_seen: &[PathBuf],
) -> TurnChangeSet {
    use std::collections::BTreeMap;
    // key: canonical resource id (fs://<abs-or-rel>)
    let mut merged: BTreeMap<String, WorkspaceChange> = BTreeMap::new();

    for c in exact {
        merged.insert(
            c.resource_id.clone(),
            WorkspaceChange {
                resource_id: c.resource_id.clone(),
                display_path: c.display_path.clone(),
                change_type: c.change_type,
                before_hash: c.before_hash.clone(),
                after_hash: c.after_hash.clone(),
                before_content: c.before_content.clone(),
                after_content: c.after_content.clone(),
                source: source_of_tool_path(&c.display_path),
                confidence: ChangeConfidence::Exact,
            },
        );
    }

    let mut complete = true;
    for sc in snapshot_changes {
        // Snapshot records paths relative to the snapshot root; exact
        // deltas key by resource_id `fs://<abs>`. Try both keys.
        let abs = snapshot_root.join(&sc.path);
        let rid_exact = format!("fs://{}", abs.display());
        let key_conflict = merged
            .values()
            .find(|c| c.display_path == abs || c.display_path.ends_with(&sc.path))
            .map(|c| c.resource_id.clone());

        match key_conflict {
            Some(rid) => {
                // Both sources saw this path — verify hash agreement.
                let entry = merged.get_mut(&rid).expect("just found");
                let snap_after = sc.after_hash.as_deref();
                match (&entry.after_hash, snap_after) {
                    (Some(a), Some(b)) if a == b => { /* agree — keep Exact */ }
                    _ => {
                        entry.confidence = ChangeConfidence::Conflict;
                        complete = false;
                    }
                }
            }
            None => {
                // Snapshot-only change (exec / formatter / external).
                let kind = match sc.kind {
                    SnapshotChangeKind::Created => ChangeType::Created,
                    SnapshotChangeKind::Deleted => ChangeType::Deleted,
                    SnapshotChangeKind::Updated | SnapshotChangeKind::Metadata => {
                        ChangeType::Updated
                    }
                };
                let source = source_of_exec_path(&abs, exec_paths_seen);
                if source == ChangeSource::UserExternal {
                    // 外部变化：保留但不算 in-flight agent 变更缺失。
                } else {
                    complete = false;
                }
                merged.insert(
                    format!("snapshot:{}/{}", snapshot_root.display(), sc.path),
                    WorkspaceChange {
                        resource_id: format!("snapshot:{}/{}", snapshot_root.display(), sc.path),
                        display_path: abs,
                        change_type: kind,
                        before_hash: sc.before_hash.clone(),
                        after_hash: sc.after_hash.clone(),
                        before_content: None,
                        after_content: None,
                        source,
                        confidence: ChangeConfidence::SnapshotDetected,
                    },
                );
            }
        }
    }

    TurnChangeSet {
        changes: merged.into_values().collect(),
        complete,
    }
}

fn source_of_tool_path(path: &PathBuf) -> ChangeSource {
    let _ = path;
    // Tracked tools are known by which delta fed us; the caller collapse
    // loses the per-tool tag — attribute generically and let the tracker
    // refine later.
    ChangeSource::Unknown
}

fn source_of_exec_path(path: &PathBuf, exec_paths_seen: &[PathBuf]) -> ChangeSource {
    if exec_paths_seen
        .iter()
        .any(|p| path.starts_with(p))
    {
        ChangeSource::ExecSnapshot
    } else {
        ChangeSource::UserExternal
    }
}

#[allow(dead_code)]
fn _assert_applied_delta_import_used(d: &AppliedChangeDelta) -> usize {
    d.changes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn res(path: &Path, before: Option<&str>, after: Option<&str>) -> ChangedResource {
        ChangedResource {
            resource_id: format!("fs://{}", path.display()),
            display_path: path.to_path_buf(),
            change_type: match (before, after) {
                (None, Some(_)) => ChangeType::Created,
                (Some(_), None) => ChangeType::Deleted,
                _ => ChangeType::Updated,
            },
            before_hash: before.map(|c| format!("h({c})")),
            after_hash: after.map(|c| format!("h({c})")),
            before_content: before.map(str::to_string),
            after_content: after.map(str::to_string),
        }
    }

    #[test]
    fn exact_only_is_complete() {
        let root = PathBuf::from("/w");
        let r = reconcile(
            &[res(Path::new("/w/a.rs"), Some("old"), Some("new"))],
            &[],
            &root,
            &[],
        );
        assert_eq!(r.changes.len(), 1);
        assert!(r.complete);
        assert_eq!(r.changes[0].confidence, ChangeConfidence::Exact);
    }

    #[test]
    fn snapshot_only_change_is_detected_with_source() {
        let root = PathBuf::from("/w");
        let snap = vec![SnapshotChange {
            path: "gen/output.json".into(),
            kind: SnapshotChangeKind::Created,
            before_hash: None,
            after_hash: Some("h2".into()),
        }];
        let r = reconcile(&[], &snap, &root, &[root.clone()]);
        assert_eq!(r.changes.len(), 1);
        assert!(!r.complete, "snapshot-only under exec → incomplete");
        assert_eq!(r.changes[0].source, ChangeSource::ExecSnapshot);
        assert_eq!(
            r.changes[0].confidence,
            ChangeConfidence::SnapshotDetected
        );
    }

    #[test]
    fn external_change_attributed_user_external() {
        let root = PathBuf::from("/w");
        let snap = vec![SnapshotChange {
            path: "notes.md".into(),
            kind: SnapshotChangeKind::Updated,
            before_hash: Some("h1".into()),
            after_hash: Some("h2".into()),
        }];
        let r = reconcile(&[], &snap, &root, &[]); // no exec cwd → external
        assert_eq!(r.changes[0].source, ChangeSource::UserExternal);
        assert!(r.complete, "外部变化不算 agent 变更缺失");
    }

    #[test]
    fn hash_disagreement_conflicts() {
        let root = PathBuf::from("/w");
        let exact = vec![res(Path::new("/w/a.txt"), Some("old"), Some("exact-new"))];
        let snap = vec![SnapshotChange {
            path: "a.txt".into(),
            kind: SnapshotChangeKind::Updated,
            before_hash: Some("h(old)".into()),
            after_hash: Some("h(snapshot-new)".into()),
        }];
        let r = reconcile(&exact, &snap, &root, &[]);
        assert_eq!(r.changes.len(), 1);
        assert_eq!(r.changes[0].confidence, ChangeConfidence::Conflict);
        assert!(!r.complete);
    }
}

/// Task 级累计变化集：任务首个 turn 前捕获基线，之后每轮 reconcile 的
/// 结果并入。`complete=false` 一旦出现即保持（累计可信度由最弱一环决定）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskChangeSet {
    /// 累计（按 resource_id 去重，后者覆盖前者）。
    pub changes: Vec<WorkspaceChange>,
    pub complete: bool,
    pub turn_count: u64,
}

impl TaskChangeSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// 并入一轮的 TurnChangeSet：同 resource_id 的 Exact 覆盖 Snapshot 条目；
    /// 任何一轮 Partial/Conflict 使整体 complete=false。
    pub fn absorb(&mut self, turn: &TurnChangeSet) {
        self.turn_count += 1;
        if !turn.complete {
            self.complete = false;
        }
        for c in &turn.changes {
            match self.changes.iter_mut().find(|e| e.resource_id == c.resource_id) {
                Some(existing) => {
                    // 新条目更可信（Exact > SnapshotDetected > Conflict）时覆盖。
                    if rank(c.confidence) >= rank(existing.confidence) {
                        *existing = c.clone();
                    }
                }
                None => self.changes.push(c.clone()),
            }
        }
    }
}

fn rank(c: ChangeConfidence) -> u8 {
    match c {
        ChangeConfidence::Exact => 3,
        ChangeConfidence::SnapshotDetected => 2,
        ChangeConfidence::Conflict => 1,
    }
}
