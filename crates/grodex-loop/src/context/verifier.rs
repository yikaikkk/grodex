//! Compaction verifier (doc 11 §17).
//!
//! Before a compaction candidate is installed, the verifier runs four
//! checks against the proposed post-compaction context:
//!   §17.1 Protocol   — no orphaned Tool Results, no faked completion.
//!   §17.2 Coverage   — critical runtime state captured in the capsule.
//!   §17.3 Budget     — tokens fall back into the 45%–60% target band.
//!   §17.4 Source fence — the source inputs are unchanged.

use std::collections::HashSet;

use grodex_core::context::ContextItem;
use grodex_core::id::ToolCallId;

use crate::context::state_capsule::CapsuleAuthority;

/// Source fence binding (doc 11 §17.4). A compaction candidate is only
/// installable if all these values are unchanged from when it was built.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceFence {
    pub source_history_version: u64,
    pub source_seq_end: u64,
    pub state_capsule_hash: String,
    pub stable_prefix_hash: String,
    pub maintenance_policy_version: u32,
    pub tokenizer_version: String,
}

/// Result of verifying a compaction candidate.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub passed: bool,
    pub protocol_errors: Vec<String>,
    pub coverage_warnings: Vec<String>,
    pub budget_warnings: Vec<String>,
    pub fence_violations: Vec<String>,
    pub estimated_tokens_after: u64,
    pub target_min: u64,
    pub target_max: u64,
}

impl VerificationResult {
    pub fn is_ok(&self) -> bool {
        self.passed
    }

    /// All diagnostic messages, concatenated in priority order.
    pub fn all_diagnostics(&self) -> Vec<String> {
        let mut all = Vec::new();
        all.extend(self.protocol_errors.iter().cloned());
        all.extend(self.coverage_warnings.iter().cloned());
        all.extend(self.budget_warnings.iter().cloned());
        all.extend(self.fence_violations.iter().cloned());
        all
    }
}

/// Tool-name heuristic for detecting file-mutating operations.
fn looks_like_file_edit(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("edit")
        || lower.contains("write")
        || lower.contains("patch")
        || lower.contains("apply")
        || lower.contains("create")
}

pub struct CompactionVerifier {
    context_window: u64,
    target_ratio_min: f64, // 0.45
    target_ratio_max: f64, // 0.60
}

impl CompactionVerifier {
    pub fn new(context_window: u64) -> Self {
        Self {
            context_window,
            target_ratio_min: 0.45,
            target_ratio_max: 0.60,
        }
    }

    /// Override the target ratio band (mostly for testing).
    pub fn with_target_band(mut self, min: f64, max: f64) -> Self {
        self.target_ratio_min = min;
        self.target_ratio_max = max;
        self
    }

    /// §17.1 Protocol validation. Two blocking checks, mirroring the
    /// existing `CompactionAssembly::validate` convention:
    ///   1. Every Tool Result has a preceding matching Tool Call
    ///      (no orphaned results).
    ///   2. No incomplete Tool masquerades as complete — every ToolCall
    ///      must have a corresponding ToolResult (no dangling calls).
    ///
    /// Error results (`is_error`) are NOT protocol violations; they are a
    /// coverage concern handled by `verify_coverage` (§17.2).
    pub fn verify_protocol(&self, items: &[ContextItem]) -> Vec<String> {
        let mut errors = Vec::new();
        let mut seen_calls: HashSet<ToolCallId> = HashSet::new();
        let mut resolved: HashSet<ToolCallId> = HashSet::new();

        for (idx, item) in items.iter().enumerate() {
            match item {
                ContextItem::ToolCall { call_id, .. } => {
                    seen_calls.insert(*call_id);
                }
                ContextItem::ToolResult { call_id, .. } => {
                    if !seen_calls.contains(call_id) {
                        errors.push(format!(
                            "orphaned ToolResult at index {idx}: no preceding ToolCall for id {call_id}"
                        ));
                    }
                    resolved.insert(*call_id);
                }
                _ => {}
            }
        }

        // §17.1 "no incomplete tool masquerading as complete": a ToolCall
        // with no matching ToolResult is a dangling/incomplete invocation
        // and must not be left in the compacted context.
        for item in items.iter() {
            if let ContextItem::ToolCall { call_id, name, .. } = item {
                if !resolved.contains(call_id) {
                    errors.push(format!(
                        "dangling ToolCall '{name}' (id {call_id}) has no ToolResult"
                    ));
                }
            }
        }

        errors
    }

    /// §17.2 State coverage: check that critical runtime state hinted at by
    /// the context items is reflected in the capsule authority fields.
    ///
    /// This is necessarily best-effort — the verifier only sees context
    /// items plus the authority block, not the live runtime stores.
    /// Returns warnings (non-blocking).
    pub fn verify_coverage(
        &self,
        items: &[ContextItem],
        authority: &CapsuleAuthority,
    ) -> Vec<String> {
        let mut warnings = Vec::new();

        let mut file_edit_tools_seen = false;
        let mut error_results_near_end = 0;
        let total = items.len();

        for (idx, item) in items.iter().enumerate() {
            match item {
                ContextItem::ToolCall { name, .. } => {
                    if looks_like_file_edit(name) {
                        file_edit_tools_seen = true;
                    }
                }
                ContextItem::ToolResult { is_error, .. } => {
                    if *is_error && idx + 4 >= total {
                        error_results_near_end += 1;
                    }
                }
                _ => {}
            }
        }

        if file_edit_tools_seen && authority.edited_files.is_empty() {
            warnings.push(
                "file-editing tool calls present but CapsuleAuthority.edited_files is empty"
                    .to_string(),
            );
        }

        if error_results_near_end > 0 && authority.unresolved_errors.is_empty() {
            warnings.push(format!(
                "{error_results_near_end} recent error tool results not recorded in CapsuleAuthority.unresolved_errors"
            ));
        }

        warnings
    }

    /// §17.3 Budget validation.
    ///
    /// Returns `(warnings, tokens_after, target_min, target_max)`.
    pub fn verify_budget(&self, estimated_tokens: u64) -> (Vec<String>, u64, u64, u64) {
        let target_min = (self.context_window as f64 * self.target_ratio_min) as u64;
        let target_max = (self.context_window as f64 * self.target_ratio_max) as u64;

        let mut warnings = Vec::new();
        if estimated_tokens > target_max {
            warnings.push(format!(
                "estimated tokens after compaction ({estimated_tokens}) exceed target max ({target_max}); target is {min}–{max}",
                min = target_min,
                max = target_max
            ));
        } else if estimated_tokens < target_min && estimated_tokens > 0 {
            warnings.push(format!(
                "estimated tokens after compaction ({estimated_tokens}) below target min ({target_min}); possible over-compaction"
            ));
        }

        (warnings, estimated_tokens, target_min, target_max)
    }

    /// §17.4 Source fence validation. A candidate is only valid if its
    /// source fence matches the current one exactly.
    pub fn verify_fence(
        &self,
        candidate_fence: &SourceFence,
        current_fence: &SourceFence,
    ) -> Vec<String> {
        let mut violations = Vec::new();
        if candidate_fence.source_history_version != current_fence.source_history_version {
            violations.push(format!(
                "source_history_version mismatch: candidate={} current={}",
                candidate_fence.source_history_version, current_fence.source_history_version
            ));
        }
        if candidate_fence.source_seq_end != current_fence.source_seq_end {
            violations.push(format!(
                "source_seq_end mismatch: candidate={} current={}",
                candidate_fence.source_seq_end, current_fence.source_seq_end
            ));
        }
        if candidate_fence.state_capsule_hash != current_fence.state_capsule_hash {
            violations.push("state_capsule_hash mismatch".to_string());
        }
        if candidate_fence.stable_prefix_hash != current_fence.stable_prefix_hash {
            violations.push("stable_prefix_hash mismatch".to_string());
        }
        if candidate_fence.maintenance_policy_version != current_fence.maintenance_policy_version {
            violations.push(format!(
                "maintenance_policy_version mismatch: candidate={} current={}",
                candidate_fence.maintenance_policy_version, current_fence.maintenance_policy_version
            ));
        }
        if candidate_fence.tokenizer_version != current_fence.tokenizer_version {
            violations.push(format!(
                "tokenizer_version mismatch: candidate={} current={}",
                candidate_fence.tokenizer_version, current_fence.tokenizer_version
            ));
        }
        violations
    }

    /// Full verification combining all four checks.
    ///
    /// `passed` is true only when there are no protocol errors and no
    /// fence violations. Coverage and budget issues are warnings.
    pub fn verify(
        &self,
        items: &[ContextItem],
        authority: &CapsuleAuthority,
        estimated_tokens: u64,
        candidate_fence: &SourceFence,
        current_fence: &SourceFence,
    ) -> VerificationResult {
        let protocol_errors = self.verify_protocol(items);
        let coverage_warnings = self.verify_coverage(items, authority);
        let (budget_warnings, tokens_after, target_min, target_max) =
            self.verify_budget(estimated_tokens);
        let fence_violations = self.verify_fence(candidate_fence, current_fence);

        let passed = protocol_errors.is_empty() && fence_violations.is_empty();

        VerificationResult {
            passed,
            protocol_errors,
            coverage_warnings,
            budget_warnings,
            fence_violations,
            estimated_tokens_after: tokens_after,
            target_min,
            target_max,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::id::ToolCallId;

    fn tc(id: ToolCallId, name: &str) -> ContextItem {
        ContextItem::ToolCall {
            call_id: id,
            name: name.to_string(),
            arguments: serde_json::Value::Null,
        }
    }

    fn tr(id: ToolCallId, is_error: bool) -> ContextItem {
        ContextItem::ToolResult {
            call_id: id,
            content: "result".to_string(),
            is_error,
        }
    }

    #[test]
    fn protocol_ok_when_result_follows_call() {
        let id = ToolCallId::new();
        let items = vec![tc(id, "read_file"), tr(id, false)];
        let v = CompactionVerifier::new(100_000);
        assert!(v.verify_protocol(&items).is_empty());
    }

    #[test]
    fn protocol_flags_orphaned_result() {
        let id = ToolCallId::new();
        let other = ToolCallId::new();
        let items = vec![tc(other, "read_file"), tr(id, false)];
        let v = CompactionVerifier::new(100_000);
        let errs = v.verify_protocol(&items);
        assert!(errs.iter().any(|e| e.contains("orphaned ToolResult")));
    }

    #[test]
    fn protocol_allows_error_result_with_matching_call() {
        // An error result is not a protocol violation — only a coverage
        // concern (§17.2). With a matching preceding call, protocol is clean.
        let id = ToolCallId::new();
        let items = vec![tc(id, "run"), tr(id, true)];
        let v = CompactionVerifier::new(100_000);
        let errs = v.verify_protocol(&items);
        assert!(
            errs.is_empty(),
            "error result with matching call must not be a protocol error: {errs:?}"
        );
    }

    #[test]
    fn protocol_flags_dangling_call_without_result() {
        let id = ToolCallId::new();
        let items = vec![tc(id, "run")];
        let v = CompactionVerifier::new(100_000);
        let errs = v.verify_protocol(&items);
        assert!(errs.iter().any(|e| e.contains("dangling ToolCall")));
    }

    #[test]
    fn coverage_warns_on_missing_edited_files() {
        let id = ToolCallId::new();
        let items = vec![tc(id, "edit_file"), tr(id, false)];
        let v = CompactionVerifier::new(100_000);
        let auth = CapsuleAuthority::default();
        let warns = v.verify_coverage(&items, &auth);
        assert!(warns.iter().any(|w| w.contains("edited_files")));
    }

    #[test]
    fn coverage_quiet_when_edited_files_present() {
        let id = ToolCallId::new();
        let items = vec![tc(id, "edit_file"), tr(id, false)];
        let v = CompactionVerifier::new(100_000);
        let mut auth = CapsuleAuthority::default();
        auth.edited_files = vec!["src/lib.rs".into()];
        assert!(v.verify_coverage(&items, &auth).is_empty());
    }

    #[test]
    fn coverage_warns_on_recent_error_unresolved() {
        let id = ToolCallId::new();
        let items = vec![tc(id, "run"), tr(id, true)];
        let v = CompactionVerifier::new(100_000);
        let auth = CapsuleAuthority::default();
        let warns = v.verify_coverage(&items, &auth);
        assert!(warns.iter().any(|w| w.contains("unresolved_errors")));
    }

    #[test]
    fn budget_within_band_is_clean() {
        let v = CompactionVerifier::new(100_000);
        // 50% of 100_000 = 50_000, within 45_000–60_000
        let (warns, _tokens, min, max) = v.verify_budget(50_000);
        assert!(warns.is_empty());
        assert_eq!(min, 45_000);
        assert_eq!(max, 60_000);
    }

    #[test]
    fn budget_above_max_warns() {
        let v = CompactionVerifier::new(100_000);
        let (warns, _, _, _) = v.verify_budget(70_000);
        assert!(warns.iter().any(|w| w.contains("exceed target max")));
    }

    #[test]
    fn budget_below_min_warns() {
        let v = CompactionVerifier::new(100_000);
        let (warns, _, _, _) = v.verify_budget(10_000);
        assert!(warns.iter().any(|w| w.contains("below target min")));
    }

    fn fence(version: u64, seq: u64, capsule: &str, prefix: &str, policy: u32, tok: &str) -> SourceFence {
        SourceFence {
            source_history_version: version,
            source_seq_end: seq,
            state_capsule_hash: capsule.to_string(),
            stable_prefix_hash: prefix.to_string(),
            maintenance_policy_version: policy,
            tokenizer_version: tok.to_string(),
        }
    }

    #[test]
    fn fence_match_is_clean() {
        let v = CompactionVerifier::new(100_000);
        let f = fence(1, 100, "c", "p", 2, "t1");
        assert!(v.verify_fence(&f, &f).is_empty());
    }

    #[test]
    fn fence_flags_version_change() {
        let v = CompactionVerifier::new(100_000);
        let a = fence(1, 100, "c", "p", 2, "t1");
        let b = fence(2, 100, "c", "p", 2, "t1");
        let viols = v.verify_fence(&a, &b);
        assert!(viols.iter().any(|s| s.contains("source_history_version")));
    }

    #[test]
    fn fence_flags_seq_change() {
        let v = CompactionVerifier::new(100_000);
        let a = fence(1, 100, "c", "p", 2, "t1");
        let b = fence(1, 200, "c", "p", 2, "t1");
        assert!(v.verify_fence(&a, &b).iter().any(|s| s.contains("source_seq_end")));
    }

    #[test]
    fn fence_flags_hash_and_tokenizer_changes() {
        let v = CompactionVerifier::new(100_000);
        let a = fence(1, 100, "c1", "p1", 2, "t1");
        let b = fence(1, 100, "c2", "p2", 3, "t2");
        let viols = v.verify_fence(&a, &b);
        assert!(viols.iter().any(|s| s.contains("state_capsule_hash")));
        assert!(viols.iter().any(|s| s.contains("stable_prefix_hash")));
        assert!(viols.iter().any(|s| s.contains("maintenance_policy_version")));
        assert!(viols.iter().any(|s| s.contains("tokenizer_version")));
    }

    #[test]
    fn full_verify_passes_on_clean_input() {
        let id = ToolCallId::new();
        let items = vec![tc(id, "read_file"), tr(id, false)];
        let mut auth = CapsuleAuthority::default();
        auth.objective = Some("obj".into());
        let f = fence(1, 100, "c", "p", 2, "t1");
        let v = CompactionVerifier::new(100_000);
        let res = v.verify(&items, &auth, 50_000, &f, &f);
        assert!(res.is_ok());
        assert!(res.all_diagnostics().is_empty());
        assert_eq!(res.target_min, 45_000);
        assert_eq!(res.target_max, 60_000);
    }

    #[test]
    fn full_verify_fails_on_protocol_error() {
        let id = ToolCallId::new();
        let other = ToolCallId::new();
        // orphaned result (no matching call) + error result
        let items = vec![tc(other, "read_file"), tr(id, true)];
        let auth = CapsuleAuthority::default();
        let f = fence(1, 100, "c", "p", 2, "t1");
        let v = CompactionVerifier::new(100_000);
        let res = v.verify(&items, &auth, 50_000, &f, &f);
        assert!(!res.is_ok());
        assert!(!res.protocol_errors.is_empty());
    }

    #[test]
    fn full_verify_fails_on_fence_violation() {
        let id = ToolCallId::new();
        let items = vec![tc(id, "read_file"), tr(id, false)];
        let auth = CapsuleAuthority::default();
        let candidate = fence(1, 100, "c", "p", 2, "t1");
        let current = fence(2, 100, "c", "p", 2, "t1");
        let v = CompactionVerifier::new(100_000);
        let res = v.verify(&items, &auth, 50_000, &candidate, &current);
        assert!(!res.is_ok());
        assert!(!res.fence_violations.is_empty());
    }

    #[test]
    fn full_verify_passes_with_warnings() {
        // budget warning (above max) should NOT block.
        let id = ToolCallId::new();
        let items = vec![tc(id, "read_file"), tr(id, false)];
        let auth = CapsuleAuthority::default();
        let f = fence(1, 100, "c", "p", 2, "t1");
        let v = CompactionVerifier::new(100_000);
        let res = v.verify(&items, &auth, 80_000, &f, &f);
        assert!(res.is_ok(), "budget warnings must not block");
        assert!(!res.budget_warnings.is_empty());
    }
}
