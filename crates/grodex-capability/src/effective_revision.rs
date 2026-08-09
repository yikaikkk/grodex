//! A user-narrowed revision of a tool call (doc 09 §11.2).
//!
//! When a user narrows the execution scope of a tool call (e.g. restricting
//! a path set to a subset, or escalating an authorization scope), the Runtime
//! creates an `EffectiveToolCallRevision`. The original `ToolCallId` is
//! preserved — the revision is a sibling record that captures the
//! `effective_args` actually approved and executed, along with the original
//! `requested_args` for audit.
//!
//! Schema, permission, resource-lock and sandbox judgments are re-run against
//! the `effective_args` of the latest revision.

use grodex_core::id::ToolCallId;
use serde::{Deserialize, Serialize};

/// How the user narrowed the execution scope of a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransformKind {
    /// Authorization scope narrowed (e.g. session-level → once), args unchanged.
    ScopeNarrowed,
    /// Tool-declared constraint transform (e.g. path set narrowed to subset).
    ArgsConstrained,
}

/// A revision of a tool call created when the user narrows the execution
/// scope (doc 09 §11.2). The original `ToolCallId` is preserved but a new
/// revision records the effective args after narrowing.
///
/// After this point, schema validation, policy checks, resource locks and
/// sandbox judgments are re-run against `effective_args`. Tool Results must
/// explicitly record that execution used the narrowed args and the actual
/// `effective_args`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveToolCallRevision {
    /// The original tool call this revision belongs to.
    pub tool_call_id: ToolCallId,
    /// Monotonic revision number within the same `ToolCallId`.
    pub revision: u32,
    /// The args the model originally requested.
    pub requested_args: serde_json::Value,
    /// The args actually approved/executed after user narrowing.
    pub effective_args: serde_json::Value,
    /// What kind of narrowing transform was applied.
    pub transform_kind: TransformKind,
    /// Identity of the user who approved the narrowing.
    pub approved_by: String,
    /// Hash of the `effective_args` for dedup and audit.
    pub effective_args_hash: String,
    /// When this revision was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl EffectiveToolCallRevision {
    /// Create a new revision. The `effective_args_hash` is computed from
    /// `effective_args` via [`compute_hash`](Self::compute_hash).
    pub fn new(
        tool_call_id: ToolCallId,
        revision: u32,
        requested_args: serde_json::Value,
        effective_args: serde_json::Value,
        transform_kind: TransformKind,
        approved_by: impl Into<String>,
    ) -> Self {
        let effective_args_hash = Self::compute_hash(&effective_args);
        Self {
            tool_call_id,
            revision,
            requested_args,
            effective_args,
            transform_kind,
            approved_by: approved_by.into(),
            effective_args_hash,
            created_at: chrono::Utc::now(),
        }
    }

    /// Compute the SHA-256 hash of `effective_args`.
    ///
    /// Uses serde_json's canonical serialization: by default `serde_json::Map`
    /// is backed by a `BTreeMap`, so object keys are serialized in sorted
    /// order, yielding a stable hash independent of insertion order.
    pub fn compute_hash(args: &serde_json::Value) -> String {
        let canonical = serde_json::to_string(args).expect("effective_args is always serializable");
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Whether this revision narrowed only the authorization scope (args
    /// unchanged).
    pub fn is_scope_narrowed(&self) -> bool {
        matches!(self.transform_kind, TransformKind::ScopeNarrowed)
    }

    /// Whether this revision constrained the args themselves (e.g. path set
    /// narrowed to a subset).
    pub fn is_args_constrained(&self) -> bool {
        matches!(self.transform_kind, TransformKind::ArgsConstrained)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcid() -> ToolCallId {
        // Deterministic UUID so multiple calls in one test compare equal.
        ToolCallId::from_string("00000000-0000-4000-8000-000000000000").unwrap()
    }

    #[test]
    fn new_revision_populates_fields_and_hash() {
        let requested = serde_json::json!({"paths": ["/a", "/b", "/c"]});
        let effective = serde_json::json!({"paths": ["/a"]});
        let rev = EffectiveToolCallRevision::new(
            tcid(),
            1,
            requested.clone(),
            effective.clone(),
            TransformKind::ArgsConstrained,
            "user-42",
        );
        assert_eq!(rev.tool_call_id, tcid());
        assert_eq!(rev.revision, 1);
        assert_eq!(rev.requested_args, requested);
        assert_eq!(rev.effective_args, effective);
        assert_eq!(rev.transform_kind, TransformKind::ArgsConstrained);
        assert_eq!(rev.approved_by, "user-42");
        assert_eq!(rev.effective_args_hash, EffectiveToolCallRevision::compute_hash(&effective));
        assert!(!rev.effective_args_hash.is_empty());
    }

    #[test]
    fn hash_is_stable_for_same_args() {
        let a = serde_json::json!({"paths": ["/a", "/b"], "mode": "ro"});
        let rev1 = EffectiveToolCallRevision::new(
            tcid(),
            1,
            a.clone(),
            a.clone(),
            TransformKind::ScopeNarrowed,
            "u",
        );
        let rev2 = EffectiveToolCallRevision::new(
            tcid(),
            2,
            a.clone(),
            a,
            TransformKind::ScopeNarrowed,
            "u",
        );
        assert_eq!(rev1.effective_args_hash, rev2.effective_args_hash);
    }

    #[test]
    fn hash_is_canonical_regardless_of_key_order() {
        // Different insertion order, same logical object — must hash equal
        // because serde_json::Map defaults to a BTreeMap (sorted keys).
        let v1 = serde_json::json!({"a": 1, "b": 2});
        let v2 = serde_json::json!({"b": 2, "a": 1});
        assert_eq!(
            EffectiveToolCallRevision::compute_hash(&v1),
            EffectiveToolCallRevision::compute_hash(&v2),
        );
    }

    #[test]
    fn hash_differs_for_different_args() {
        let a = serde_json::json!({"paths": ["/a"]});
        let b = serde_json::json!({"paths": ["/a", "/b"]});
        assert_ne!(
            EffectiveToolCallRevision::compute_hash(&a),
            EffectiveToolCallRevision::compute_hash(&b),
        );
    }

    #[test]
    fn transform_kind_predicates_are_correct() {
        let scope_rev = EffectiveToolCallRevision::new(
            tcid(),
            1,
            serde_json::json!({"k": 1}),
            serde_json::json!({"k": 1}),
            TransformKind::ScopeNarrowed,
            "u",
        );
        let args_rev = EffectiveToolCallRevision::new(
            tcid(),
            1,
            serde_json::json!({"paths": ["/a", "/b"]}),
            serde_json::json!({"paths": ["/a"]}),
            TransformKind::ArgsConstrained,
            "u",
        );
        assert!(scope_rev.is_scope_narrowed());
        assert!(!scope_rev.is_args_constrained());
        assert!(args_rev.is_args_constrained());
        assert!(!args_rev.is_scope_narrowed());
    }
}
