//! `TurnDiffTracker` — aggregates per-turn file changes into a single net diff.
//!
//! A turn may touch the same file several times (write, then edit, then revert).
//! The model-facing tool results show each step, but the user-facing "what did
//! this turn change" view wants the **net** baseline → current delta. This
//! tracker keeps only two versions per path — the first-observed baseline and
//! the latest current — so multiple edits collapse to one net change and a file
//! restored to its original content produces no diff at all.
//!
//! It is pure in-memory state, built from per-tool [`AppliedChangeDelta`]s and
//! released at turn end; persistence keeps only the final summary + blob ref.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::common::{AppliedChangeDelta, ChangedResource, ChangeType};

pub struct TurnDiffTracker {
    /// First-observed change per `resource_id` (the pre-turn baseline).
    baseline_by_path: HashMap<String, ChangedResource>,
    /// Latest change per `resource_id` (the post-turn current state).
    current_by_path: HashMap<String, ChangedResource>,
    /// False once an inexact delta (e.g. arbitrary `exec`) invalidates us.
    valid: bool,
}

impl Default for TurnDiffTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnDiffTracker {
    pub fn new() -> Self {
        Self {
            baseline_by_path: HashMap::new(),
            current_by_path: HashMap::new(),
            valid: true,
        }
    }

    /// Whether the tracker can still produce a trustworthy net diff. An
    /// arbitrary `exec` (which may mutate files outside the tracked tools)
    /// flips this to false via [`apply`] with an inexact delta.
    pub fn is_valid(&self) -> bool {
        self.valid
    }

    /// Merge an applied delta. An inexact delta invalidates the whole tracker.
    pub fn apply(&mut self, delta: &AppliedChangeDelta) {
        if !delta.exact {
            self.valid = false;
            return;
        }
        for change in &delta.changes {
            self.baseline_by_path
                .entry(change.resource_id.clone())
                .or_insert_with(|| change.clone());
            self.current_by_path
                .insert(change.resource_id.clone(), change.clone());
        }
    }

    /// Compute the net baseline → current diff. Returns `None` when the
    /// tracker was invalidated. Files whose content returned to the baseline
    /// are dropped (no net change). Moves carry through as path-level changes
    /// (their content is unchanged by definition).
    pub fn net_changes(&self) -> Option<Vec<ChangedResource>> {
        if !self.valid {
            return None;
        }
        let mut out = Vec::new();
        for (rid, current) in &self.current_by_path {
            let baseline = match self.baseline_by_path.get(rid) {
                Some(b) => b,
                None => continue,
            };

            // A rename is a pure path change — carry it through unchanged.
            if current.change_type == ChangeType::Moved {
                out.push(current.clone());
                continue;
            }

            let before = baseline.before_content.clone();
            let after = current.after_content.clone();

            // Restored to the original → no net change.
            if before == after {
                continue;
            }

            let change_type = match (before.as_ref(), after.as_ref()) {
                (None, Some(_)) => ChangeType::Created,
                (Some(_), None) => ChangeType::Deleted,
                _ => ChangeType::Updated,
            };

            out.push(ChangedResource {
                resource_id: rid.clone(),
                display_path: current.display_path.clone(),
                change_type,
                before_hash: baseline.before_hash.clone(),
                after_hash: current.after_hash.clone(),
                before_content: before,
                after_content: after,
            });
        }
        Some(out)
    }
}

/// The serialized, blob-stored form of a turn's net diff. Stored as a
/// content-addressed blob (`application/vnd.grodex.diff+json`); the rollout
/// journal only keeps the summary + `diff_id`, never the full contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffDocument {
    pub format: String,
    pub files: Vec<DiffFile>,
    pub changed_files: usize,
    pub added_lines: usize,
    pub removed_lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub change_type: String,
    pub before_content: Option<String>,
    pub after_content: Option<String>,
}

pub const DIFF_MIME: &str = "application/vnd.grodex.diff+json";

fn change_type_str(t: ChangeType) -> &'static str {
    match t {
        ChangeType::Created => "created",
        ChangeType::Updated => "updated",
        ChangeType::Deleted => "deleted",
        ChangeType::Moved => "moved",
        ChangeType::Metadata => "metadata",
    }
}

impl DiffDocument {
    /// Build the net-diff document from a set of net [`ChangedResource`]s.
    /// `added_lines`/`removed_lines` are an approximate line-count delta; the
    /// frontend renders the precise unified diff from before/after content.
    pub fn from_net_changes(changes: &[ChangedResource]) -> Self {
        let mut files = Vec::with_capacity(changes.len());
        let mut added_lines = 0usize;
        let mut removed_lines = 0usize;
        for c in changes {
            let before_lines = c.before_content.as_ref().map(|s| s.lines().count()).unwrap_or(0);
            let after_lines = c.after_content.as_ref().map(|s| s.lines().count()).unwrap_or(0);
            if after_lines >= before_lines {
                added_lines += after_lines - before_lines;
            } else {
                removed_lines += before_lines - after_lines;
            }
            files.push(DiffFile {
                path: c.display_path.to_string_lossy().to_string(),
                change_type: change_type_str(c.change_type).to_string(),
                before_content: c.before_content.clone(),
                after_content: c.after_content.clone(),
            });
        }
        Self {
            format: DIFF_MIME.to_string(),
            changed_files: files.len(),
            added_lines,
            removed_lines,
            files,
        }
    }

    /// Serialize to JSON bytes (the blob payload).
    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ch(
        rid: &str,
        change_type: ChangeType,
        before: Option<&str>,
        after: Option<&str>,
    ) -> ChangedResource {
        ChangedResource {
            resource_id: rid.to_string(),
            display_path: PathBuf::from(rid.trim_start_matches("fs://")),
            change_type,
            before_hash: None,
            after_hash: None,
            before_content: before.map(str::to_string),
            after_content: after.map(str::to_string),
        }
    }

    #[test]
    fn multiple_updates_collapse_to_net_change() {
        let mut t = TurnDiffTracker::new();
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://a.rs", ChangeType::Updated, Some("one"), Some("two"))],
            exact: true,
        });
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://a.rs", ChangeType::Updated, Some("two"), Some("three"))],
            exact: true,
        });

        let net = t.net_changes().unwrap();
        assert_eq!(net.len(), 1);
        assert_eq!(net[0].before_content.as_deref(), Some("one")); // baseline
        assert_eq!(net[0].after_content.as_deref(), Some("three")); // final
    }

    #[test]
    fn restored_to_original_produces_no_diff() {
        let mut t = TurnDiffTracker::new();
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://a.rs", ChangeType::Updated, Some("one"), Some("two"))],
            exact: true,
        });
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://a.rs", ChangeType::Updated, Some("two"), Some("one"))],
            exact: true,
        });

        assert!(t.net_changes().unwrap().is_empty());
    }

    #[test]
    fn create_then_delete_cancels_out() {
        let mut t = TurnDiffTracker::new();
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://new.rs", ChangeType::Created, None, Some("hello"))],
            exact: true,
        });
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://new.rs", ChangeType::Deleted, Some("hello"), None)],
            exact: true,
        });

        assert!(t.net_changes().unwrap().is_empty());
    }

    #[test]
    fn inexact_delta_invalidates() {
        let mut t = TurnDiffTracker::new();
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://a.rs", ChangeType::Updated, Some("one"), Some("two"))],
            exact: true,
        });
        t.apply(&AppliedChangeDelta {
            changes: vec![],
            exact: false,
        });

        assert!(!t.is_valid());
        assert!(t.net_changes().is_none());
    }

    #[test]
    fn distinct_files_tracked_independently() {
        let mut t = TurnDiffTracker::new();
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://a.rs", ChangeType::Updated, Some("1"), Some("2"))],
            exact: true,
        });
        t.apply(&AppliedChangeDelta {
            changes: vec![ch("fs://b.rs", ChangeType::Created, None, Some("new"))],
            exact: true,
        });

        let net = t.net_changes().unwrap();
        assert_eq!(net.len(), 2);
    }
}
