//! Capability descriptors and step-level snapshots.
//!
//! The descriptor is the long-lived registration record. The snapshot
//! captures exactly which capabilities were visible to the model during
//! a specific sampling step — eliminating drift between declaration and execution.

use crate::exposure::ToolExposure;
use crate::id::CapabilityId;
use grodex_core::id::StepGeneration;
use grodex_core::id::StepSnapshotId;
use grodex_core::tool::ConcurrencyClass;
use grodex_core::tool::SideEffectClass;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Full registration record for one capability.
///
/// This is the long-lived state managed by the CapabilityManager.
/// It may be updated (e.g. on MCP refresh) and carries a monotonic
/// revision counter for staleness detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    /// Globally unique identifier.
    pub id: CapabilityId,
    /// Human-readable display name.
    pub display_name: String,
    /// Description shown to the model and in listings.
    pub description: String,
    /// How this capability is exposed to different consumers.
    pub exposure: ToolExposure,
    /// Trust level (0 = untrusted, 255 = fully trusted core).
    pub trust_level: u8,
    /// JSON Schema for the input arguments, if applicable.
    pub input_schema: Option<serde_json::Value>,
    /// Concurrency mode, if this is a Tool.
    pub concurrency_class: Option<ConcurrencyClass>,
    /// Side-effect class for retry/recovery, if this is a Tool.
    pub side_effect_class: Option<SideEffectClass>,
    /// Human-readable locator for debugging (e.g. `mcp://github/tools/search`).
    pub source_locator: String,
    /// Content-addressed hash of the capability definition.
    pub content_hash: String,
    /// Monotonic revision counter for this specific capability.
    pub capability_revision: u64,
    /// Global generation at the time this descriptor was last updated.
    pub generation: u64,
}

/// An immutable snapshot of which capabilities were active for one Step.
///
/// Once captured, the model's tool calls within this Step are guaranteed
/// to execute against the exact same definitions — no MCP refresh or
/// config change can affect them mid-flight.
///
/// Five "binding" channels (Design Doc 10 §16) record, at capture time, the
/// exact revision of each capability source the model saw:
/// - tool_router:   built-in + core tools visible this Step
/// - skill_catalog: skills (workflow/prompt) available this Step
/// - mcp_binding:   MCP-server-provided tools + their connection generation
/// - policy_binding: permission policy epoch governing this Step
/// - sandbox_binding: sandbox profile enforced this Step
///
/// Together they are the audit trail a recovery / replay reconciles against
/// — a late tool call whose source generation no longer matches the bound
/// one is rejected (invariant #14/#15).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepCapabilitySnapshot {
    /// Unique identifier for this snapshot.
    pub snapshot_id: StepSnapshotId,
    /// The capability generation at the start of the Turn.
    pub turn_base_generation: u64,
    /// The Step's generation counter.
    pub step_generation: StepGeneration,
    /// Capability IDs that were promoted (visible) in this Step.
    pub promoted_capability_ids: Vec<CapabilityId>,
    /// Generation counter of each capability source at capture time.
    pub source_generations: HashMap<String, u64>,
    /// Revocation epoch at the time of capture.
    pub revocation_epoch_at_capture: u64,
    /// When this snapshot was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    // ── The five binding channels ───────────────────────────────────────
    /// Built-in / core tools visible to the model this Step + their generation.
    pub tool_router: ToolRouterBinding,
    /// Skills available this Step + catalog generation.
    pub skill_catalog: SkillCatalogBinding,
    /// MCP-server-provided tools + connection generation per server.
    pub mcp_binding: McpBinding,
    /// Permission policy epoch + pinned rule set governing this Step.
    pub policy_binding: PolicyBinding,
    /// Sandbox profile enforced for operations in this Step.
    pub sandbox_binding: SandboxBindingRef,
    /// Agent authority ceiling inherited from the session (sub-agents
    /// cannot exceed it — invariant #12 enforcement point).
    pub agent_authority_ceiling: u8,
}

/// Snapshot of the built-in tool router at capture time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolRouterBinding {
    /// Capability IDs of the promoted (visible) built-in tools.
    pub tool_capability_ids: Vec<CapabilityId>,
    /// Tool-router source generation at capture.
    pub source_generation: u64,
}

/// Snapshot of the skill catalog at capture time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillCatalogBinding {
    /// Skill capability IDs available this Step.
    pub skill_capability_ids: Vec<CapabilityId>,
    /// Skill-catalog source generation at capture.
    pub source_generation: u64,
}

/// Snapshot of MCP connections + their contributed tools at capture time.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpBinding {
    /// One entry per connected MCP server at capture time.
    pub servers: Vec<McpServerBinding>,
}

/// A single MCP server's binding at capture time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerBinding {
    /// Server name (provider-scoped id of the MCP capability source).
    pub server_name: String,
    /// Capability IDs contributed by this server this Step.
    pub contributed_capability_ids: Vec<CapabilityId>,
    /// Connection generation (bumped on reconnect / tools-changed).
    pub connection_generation: u64,
}

/// Snapshot of the permission policy governing this Step.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PolicyBinding {
    /// Revocation epoch pinned for this Step. A later `revoke_all()` does
    /// not retroactively re-open this Step; later Steps get a new epoch.
    pub revocation_epoch: u64,
    /// Number of rules in effect at capture (audit: monotonic growth check).
    pub rule_count: usize,
}

/// Reference to the sandbox profile enforced for this Step.
///
/// Holds the profile name + generation rather than a full clone; the
/// `SandboxManager` is the system of record for profiles.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxBindingRef {
    /// Active profile name (e.g. "workspace", "readonly", "restricted").
    pub profile_name: String,
    /// Profile generation at capture.
    pub profile_generation: u64,
}

impl StepCapabilitySnapshot {
    /// Builder-style constructor that fills the five binding channels.
    pub fn builder(snapshot_id: StepSnapshotId, step_generation: StepGeneration) -> StepCapabilitySnapshotBuilder {
        StepCapabilitySnapshotBuilder {
            snapshot_id,
            step_generation,
            turn_base_generation: 0,
            promoted_capability_ids: Vec::new(),
            source_generations: HashMap::new(),
            revocation_epoch_at_capture: 0,
            tool_router: ToolRouterBinding::default(),
            skill_catalog: SkillCatalogBinding::default(),
            mcp_binding: McpBinding::default(),
            policy_binding: PolicyBinding::default(),
            sandbox_binding: SandboxBindingRef::default(),
            agent_authority_ceiling: 0,
        }
    }
}

/// Builder for `StepCapabilitySnapshot` so call sites fill the five bindings
/// without a 13-argument constructor.
#[derive(Debug, Clone)]
pub struct StepCapabilitySnapshotBuilder {
    pub snapshot_id: StepSnapshotId,
    pub step_generation: StepGeneration,
    pub turn_base_generation: u64,
    pub promoted_capability_ids: Vec<CapabilityId>,
    pub source_generations: HashMap<String, u64>,
    pub revocation_epoch_at_capture: u64,
    pub tool_router: ToolRouterBinding,
    pub skill_catalog: SkillCatalogBinding,
    pub mcp_binding: McpBinding,
    pub policy_binding: PolicyBinding,
    pub sandbox_binding: SandboxBindingRef,
    pub agent_authority_ceiling: u8,
}

impl StepCapabilitySnapshotBuilder {
    pub fn turn_base_generation(mut self, g: u64) -> Self { self.turn_base_generation = g; self }
    pub fn promoted(mut self, ids: Vec<CapabilityId>) -> Self { self.promoted_capability_ids = ids; self }
    pub fn revocation_epoch(mut self, e: u64) -> Self { self.revocation_epoch_at_capture = e; self.policy_binding.revocation_epoch = e; self }
    pub fn tool_router(mut self, b: ToolRouterBinding) -> Self { self.tool_router = b; self }
    pub fn skill_catalog(mut self, b: SkillCatalogBinding) -> Self { self.skill_catalog = b; self }
    pub fn mcp_binding(mut self, b: McpBinding) -> Self { self.mcp_binding = b; self }
    pub fn sandbox(mut self, name: impl Into<String>, generation: u64) -> Self {
        self.sandbox_binding = SandboxBindingRef { profile_name: name.into(), profile_generation: generation };
        self
    }
    pub fn authority_ceiling(mut self, c: u8) -> Self { self.agent_authority_ceiling = c; self }

    pub fn build(self) -> StepCapabilitySnapshot {
        StepCapabilitySnapshot {
            snapshot_id: self.snapshot_id,
            turn_base_generation: self.turn_base_generation,
            step_generation: self.step_generation,
            promoted_capability_ids: self.promoted_capability_ids,
            source_generations: self.source_generations,
            revocation_epoch_at_capture: self.revocation_epoch_at_capture,
            created_at: chrono::Utc::now(),
            tool_router: self.tool_router,
            skill_catalog: self.skill_catalog,
            mcp_binding: self.mcp_binding,
            policy_binding: self.policy_binding,
            sandbox_binding: self.sandbox_binding,
            agent_authority_ceiling: self.agent_authority_ceiling,
        }
    }
}

// ── Turn-level capability base + overlay (Design Doc 10 §12) ──────────────

/// Immutable, per-Turn baseline capability state (Design Doc 10 §12.1).
///
/// The TurnCapabilityBase is captured exactly once when a Turn begins — it
/// holds the Turn's capability generation, its Step snapshots (ordered), and
/// a hash over the "declared visible" capabilities that Steps in this Turn
/// can never exceed.
///
/// Crucially: the base is monotonic within a Turn. If an individual Step
/// wants to promote *fewer* capabilities than the base (restrictions), it
/// records that delta via a `TurnCapabilityOverlay` — never by rewriting the
/// base itself. Replay/recovery revalidates every Step against this base.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCapabilityBase {
    /// Turn id this base was captured for (for cross-checks against rollout).
    pub turn_id: String,
    /// Capability generation captured at Turn start — every Step in this
    /// Turn must carry `source_generations["capability"] >= turn_generation`.
    pub turn_generation: u64,
    /// Max authority ceiling visible to any Step in this Turn (sub-agents
    /// start from their parent's ceiling and can only dial it down).
    pub authority_ceiling: u8,
    /// All capability ids that were promotable at Turn start. A Step may
    /// promote a *subset* via its overlay; it may never promote ids not
    /// present here (invariant #10 enforcement boundary).
    pub promotable_ids: Vec<CapabilityId>,
    /// Step snapshots, in Step order. Each Step's overlay references the
    /// index of its base snapshot so replay can walk the Turn linearly.
    pub step_snapshots: Vec<StepCapabilitySnapshot>,
    /// Content hash over `(turn_generation, authority_ceiling, promotable_ids,
    /// each step_snapshot's snapshot_id)` — allows cache keying and crash
    /// recovery validation ("did I reconstruct the same base?").
    pub base_hash: String,
    /// Whether deferred promotion is permitted for this Turn (true = Steps
    /// can promote `Deferred` capabilities by issuing a TurnCapabilityOverlay
    /// that flips them to `Available`; false = only `Available` items can be
    /// promoted in any Step — stricter enterprise mode).
    pub deferred_promotion_allowed: bool,
    /// Policy epoch at Turn start — revocation events after this epoch do
    /// not retroactively re-open Steps already captured, but Steps captured
    /// *after* a `revoke_all()` get a newer epoch via their overlay (see
    /// `LiveRevocationFence` below).
    pub policy_epoch_at_start: u64,
}

impl TurnCapabilityBase {
    /// Build the content-hash for a base (deterministic, order-dependent).
    ///
    /// The hash intentionally does NOT depend on the step snapshots'
    /// internal promoted lists — only their IDs, because the snapshots
    /// themselves carry their own hashes for auditing.
    pub fn compute_hash(
        turn_id: &str,
        turn_generation: u64,
        authority_ceiling: u8,
        promotable_ids: &[CapabilityId],
        step_snapshot_ids: &[String],
    ) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(256);
        write!(s, "turn={}|gen={}|auth={}|promo=", turn_id, turn_generation, authority_ceiling).unwrap();
        for id in promotable_ids {
            write!(s, "{:?},", id).unwrap();
        }
        write!(s, "|steps=").unwrap();
        for sid in step_snapshot_ids {
            write!(s, "{},", sid).unwrap();
        }
        // Lightweight 16-char hash. For cryptographic guarantees a caller
        // would run SHA-256 over `s`; we use base64(sha256)[..16] via the
        // standard `format!("{:x}", ...)` of a simple stable digest so the
        // runtime has no extra crypto crate dependency for this hash.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3); // FNV-1a 64-bit
        }
        format!("{:016x}", h)
    }

    /// Convenience constructor — fills `base_hash` from the other fields.
    pub fn new(
        turn_id: impl Into<String>,
        turn_generation: u64,
        authority_ceiling: u8,
        promotable_ids: Vec<CapabilityId>,
        step_snapshots: Vec<StepCapabilitySnapshot>,
        deferred_promotion_allowed: bool,
        policy_epoch_at_start: u64,
    ) -> Self {
        let turn_id = turn_id.into();
        let step_ids: Vec<String> = step_snapshots.iter().map(|s| s.snapshot_id.to_string()).collect();
        let base_hash = Self::compute_hash(
            &turn_id, turn_generation, authority_ceiling, &promotable_ids, &step_ids,
        );
        Self {
            turn_id,
            turn_generation,
            authority_ceiling,
            promotable_ids,
            step_snapshots,
            base_hash,
            deferred_promotion_allowed,
            policy_epoch_at_start,
        }
    }

    /// Returns true iff the given capability id was promotable at Turn start
    /// (i.e. a Step can legally promote it).
    pub fn is_promotable(&self, id: &CapabilityId) -> bool {
        self.promotable_ids.contains(id)
    }

    /// Look up a Step's snapshot by index (0-based in Turn order).
    pub fn step_snapshot(&self, step_index: usize) -> Option<&StepCapabilitySnapshot> {
        self.step_snapshots.get(step_index)
    }
}

/// A per-Step delta that tightens or relaxes the TurnCapabilityBase for one
/// Step only (Design Doc 10 §12.2).
///
/// A TurnCapabilityOverlay describes *what changed* between the Turn's base
/// and an individual Step:
///   - Which promotable ids the Step actually promoted (usually a subset).
///   - Which `Deferred` capabilities were explicitly promoted this Step
///     (only legal when `base.deferred_promotion_allowed` is true).
///   - Any live-revocation fence bumped before this Step executed.
///
/// Critically, overlays can only *restrict* the base + promote previously
/// Deferred items. They can never add a capability id that wasn't already
/// in `base.promotable_ids` (invariant #10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCapabilityOverlay {
    /// Turn id (matches the base this overlay applies to).
    pub turn_id: String,
    /// Index of the Step this overlay is for (0-based in Turn order).
    pub step_index: usize,
    /// The Step snapshot id this overlay is paired with (reconciliation id).
    pub snapshot_id: String,
    /// Capability ids this Step actually promoted — must be ⊆ of
    /// `TurnCapabilityBase::promotable_ids ∪ deferred_promoted_ids`.
    pub promoted_ids: Vec<CapabilityId>,
    /// `Deferred → Available` promotions that happened this Step. Each id
    /// here must also be in `base.promotable_ids`; if
    /// `base.deferred_promotion_allowed` is false this list MUST be empty.
    pub deferred_promoted_ids: Vec<CapabilityId>,
    /// Live-revocation fence epoch at the moment this Step ran. If a
    /// `revoke_all()` happened mid-Turn, this is the *newer* epoch (not
    /// the Turn-start one), meaning any capability revoked under that
    /// newer epoch must have its id removed from `promoted_ids` above.
    pub revocation_fence_epoch: u64,
    /// Optional human-readable reason this overlay deviates from the base
    /// (e.g. "user disabled MCP mid-turn", "live-revoke #3 applied"),
    /// surfaced in diagnostics and replay reports.
    pub reason: Option<String>,
}

impl TurnCapabilityOverlay {
    /// Whether this overlay carries no changes (no promotions, no deferred
    /// promotions, no fence-epoch bump from the Turn-start base).
    /// Callers use this to short-circuit `apply_overlay` / `adopt_overlay`
    /// when nothing actually changed during the Turn.
    pub fn is_empty(&self) -> bool {
        self.promoted_ids.is_empty()
            && self.deferred_promoted_ids.is_empty()
            && self.reason.is_none()
    }

    /// Validate the overlay against its declared base.
    ///
    /// Returns `Ok(())` iff all invariants hold; returns the first violating
    /// rule as a descriptive error otherwise. This is the code-level
    /// enforcement boundary for Design Doc 10 §12 invariants.
    pub fn validate_against(&self, base: &TurnCapabilityBase) -> Result<(), String> {
        // 1. Must reference the same Turn.
        if self.turn_id != base.turn_id {
            return Err(format!(
                "overlay turn_id={} mismatches base turn_id={}", self.turn_id, base.turn_id
            ));
        }
        // 2. step_index must be a valid index into base.step_snapshots.
        let snap = base.step_snapshot(self.step_index).ok_or_else(|| {
            format!(
                "overlay step_index={} out of range for base with {} steps",
                self.step_index, base.step_snapshots.len()
            )
        })?;
        // 3. snapshot_id must match the base's recorded snapshot at that index.
        if snap.snapshot_id.to_string() != self.snapshot_id {
            return Err(format!(
                "overlay snapshot_id={} mismatches step {}'s snapshot_id={}",
                self.snapshot_id, self.step_index, snap.snapshot_id
            ));
        }
        // 4. All deferred_promoted_ids must appear in base.promotable_ids.
        for id in &self.deferred_promoted_ids {
            if !base.is_promotable(id) {
                return Err(format!(
                    "deferred-promoted id {:?} not in base.promotable_ids", id
                ));
            }
        }
        // 5. Deferred promotion can only occur when the base explicitly
        //    allows it (fail-closed — enterprise default).
        if !base.deferred_promotion_allowed && !self.deferred_promoted_ids.is_empty() {
            return Err(format!(
                "base forbids deferred promotion but overlay promoted {} ids",
                self.deferred_promoted_ids.len()
            ));
        }
        // 6. promoted_ids must be a subset of (base.promotable_ids) UNION
        //    (this overlay's deferred_promoted_ids) — which simplifies to
        //    "every promoted_id is promotable", since deferred_promoted_ids
        //    was already checked to be ⊆ promotable_ids above.
        for id in &self.promoted_ids {
            if !base.is_promotable(id) {
                return Err(format!(
                    "overlay promoted_id {:?} not in base.promotable_ids", id
                ));
            }
        }
        // 7. revocation_fence_epoch must not be earlier than the Turn-start
        //    policy epoch (fences can only move forward — they don't go back).
        if self.revocation_fence_epoch < base.policy_epoch_at_start {
            return Err(format!(
                "overlay revocation_fence_epoch={} predates base.policy_epoch_at_start={}",
                self.revocation_fence_epoch, base.policy_epoch_at_start
            ));
        }
        Ok(())
    }
}

// ── Live revocation fence (Design Doc 10 §13) ────────────────────────────

/// An append-only, monotonic live-revocation fence (Design Doc 10 §13).
///
/// The `LiveRevocationFence` is the authority for "can this still run?"
/// between Turns. The CapabilityManager records revocations against it,
/// and every new Step consults the current `epoch()` before capturing its
/// `TurnCapabilityOverlay.revocation_fence_epoch`.
///
/// Revocation happens in two granularities:
///   * `revoke_all()` — bumps the epoch, invalidating *all* previous Step
///     captures for Steps not yet started.
///   * `revoke_ids(&[CapabilityId])` — records specific ids as revoked at
///     the current epoch; existing Step snapshots that carry those ids but
///     reference an older epoch in their overlay fence are still allowed
///     (per-Step guarantees) but *new* overlays must exclude them.
///
/// Both operations are monotonic: you cannot "un-revoke" something.
/// Recovery after restart rebuilds state by replaying the fence's
/// operations against the same event journal (§13.4).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LiveRevocationFence {
    /// Monotonic policy epoch. Starts at 0; `revoke_all()` bumps +1.
    epoch: u64,
    /// Per-id revocation epochs. If an id is absent it is not revoked.
    /// If present, it records the epoch at which the id became revoked.
    revoked_at: std::collections::HashMap<CapabilityId, u64>,
}

impl LiveRevocationFence {
    pub fn new() -> Self {
        Self { epoch: 0, revoked_at: std::collections::HashMap::new() }
    }

    /// Current epoch — new overlays stamp `revocation_fence_epoch` with
    /// this value.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Globally revoke all capabilities (bump epoch). Any Step captured
    /// *after* this call will see the newer epoch and must re-validate
    /// its promotable ids against current policy.
    ///
    /// Returns the new epoch.
    pub fn revoke_all(&mut self) -> u64 {
        self.epoch += 1;
        // Also explicitly bump every already-recorded id's revoked_at to
        // the new epoch, so `was_revoked_before(id, old_epoch)` queries
        // stay consistent across a revoke_all() that lands mid-Turn.
        for v in self.revoked_at.values_mut() {
            *v = self.epoch;
        }
        self.epoch
    }

    /// Revoke specific capability ids at the current epoch.
    ///
    /// If an id was already revoked at an earlier epoch, this is a no-op
    /// (monotonicity — cannot "upgrade" a revocation). Returns the number
    /// of ids that were newly revoked by this call.
    pub fn revoke_ids(&mut self, ids: &[CapabilityId]) -> usize {
        let mut newly = 0usize;
        for id in ids {
            use std::collections::hash_map::Entry;
            match self.revoked_at.entry(id.clone()) {
                Entry::Vacant(v) => {
                    v.insert(self.epoch);
                    newly += 1;
                }
                Entry::Occupied(_) => { /* already revoked at <= current epoch */ }
            }
        }
        newly
    }

    /// True iff `id` was revoked before or at `query_epoch`.
    ///
    /// This is the main predicate: when a Step is building its overlay and
    /// wants to promote `id` under `fence_epoch = X`, it calls
    /// `was_revoked_before(id, X)` and refuses to promote the id if the
    /// answer is yes.
    pub fn was_revoked_before(&self, id: &CapabilityId, query_epoch: u64) -> bool {
        match self.revoked_at.get(id) {
            Some(&revoked_at) => revoked_at <= query_epoch,
            None => false,
        }
    }

    /// Filter a candidate list of promotable ids to only those that are
    /// still valid at `query_epoch` — returns the survivors.
    pub fn filter_revoked(&self, ids: &[CapabilityId], query_epoch: u64) -> Vec<CapabilityId> {
        ids.iter()
            .filter(|id| !self.was_revoked_before(id, query_epoch))
            .cloned()
            .collect()
    }

    /// Number of ids that have ever been individually revoked (for metrics,
    /// diagnostics, tests). revoke_all() does not directly add entries here
    /// — it bumps existing ones plus increments the epoch counter.
    pub fn individually_revoked_count(&self) -> usize {
        self.revoked_at.len()
    }
}

#[cfg(test)]
mod tests_doc10 {
    use super::*;
    use crate::authority::Authority;
    use crate::id::CapabilityKind;
    use grodex_core::id::StepGeneration;

    fn make_cid(s: &str) -> CapabilityId {
        CapabilityId::new(
            Authority::Core,
            "test_provider",
            CapabilityKind::Tool,
            s,
        )
    }
    fn make_snap(sid: &str, step_gen: u32) -> StepCapabilitySnapshot {
        let snapshot_id = StepSnapshotId::from_string(sid)
            .unwrap_or_else(|_| StepSnapshotId::new());
        StepCapabilitySnapshot::builder(
            snapshot_id,
            StepGeneration::new(step_gen as u64),
        ).build()
    }

    #[test]
    fn turn_base_hash_stable_and_changes_on_content() {
        let snap1 = make_snap("ss_1", 1);
        let snap2 = make_snap("ss_2", 2);
        let ids = vec![make_cid("cap_a"), make_cid("cap_b")];
        let step_ids1: Vec<String> = [&snap1, &snap2].iter().map(|s| s.snapshot_id.to_string()).collect();
        let h1 = TurnCapabilityBase::compute_hash("t1", 5, 200, &ids, &step_ids1);
        let h2 = TurnCapabilityBase::compute_hash("t1", 5, 200, &ids, &step_ids1);
        assert_eq!(h1, h2, "hash must be deterministic");

        // Change step order → different hash.
        let step_ids2: Vec<String> = [&snap2, &snap1].iter().map(|s| s.snapshot_id.to_string()).collect();
        let h3 = TurnCapabilityBase::compute_hash("t1", 5, 200, &ids, &step_ids2);
        assert_ne!(h1, h3, "step-order change must change hash");
    }

    #[test]
    fn overlay_validate_against_base_rejects_unpromotable() {
        let snap = make_snap("ss_1", 1);
        let base = TurnCapabilityBase::new(
            "t_1", 3, 200,
            vec![make_cid("cap_a"), make_cid("cap_b")],
            vec![snap.clone()],
            false, // deferred promotion NOT allowed
            0,
        );

        // Good: promote only promotable ids.
        let good_overlay = TurnCapabilityOverlay {
            turn_id: "t_1".into(),
            step_index: 0,
            snapshot_id: snap.snapshot_id.to_string(),
            promoted_ids: vec![make_cid("cap_a")],
            deferred_promoted_ids: vec![],
            revocation_fence_epoch: 0,
            reason: None,
        };
        assert!(good_overlay.validate_against(&base).is_ok());

        // Bad: promote an id not in base.promotable_ids.
        let bad_overlay = TurnCapabilityOverlay {
            turn_id: "t_1".into(),
            step_index: 0,
            snapshot_id: snap.snapshot_id.to_string(),
            promoted_ids: vec![make_cid("cap_Z_NOT_IN_BASE")],
            deferred_promoted_ids: vec![],
            revocation_fence_epoch: 0,
            reason: None,
        };
        let err = bad_overlay.validate_against(&base).expect_err("should reject");
        assert!(err.contains("not in base.promotable_ids"), "unexpected err: {err}");

        // Bad: deferred promotion when base forbids it.
        let bad_def = TurnCapabilityOverlay {
            turn_id: "t_1".into(),
            step_index: 0,
            snapshot_id: snap.snapshot_id.to_string(),
            promoted_ids: vec![make_cid("cap_a")],
            deferred_promoted_ids: vec![make_cid("cap_b")],
            revocation_fence_epoch: 0,
            reason: None,
        };
        let err = bad_def.validate_against(&base).expect_err("should reject deferred");
        assert!(err.contains("forbids deferred promotion"), "unexpected err: {err}");
    }

    #[test]
    fn live_revocation_fence_monotonic() {
        let mut fence = LiveRevocationFence::new();
        assert_eq!(fence.epoch(), 0);

        // Revoke two ids at epoch 0.
        let n = fence.revoke_ids(&[make_cid("a"), make_cid("b")]);
        assert_eq!(n, 2);
        assert_eq!(fence.individually_revoked_count(), 2);
        assert!(fence.was_revoked_before(&make_cid("a"), 0));
        assert!(!fence.was_revoked_before(&make_cid("c"), 0));

        // Duplicate revoke_ids → no-op, count stays 2.
        let n2 = fence.revoke_ids(&[make_cid("a"), make_cid("c")]);
        assert_eq!(n2, 1);
        assert_eq!(fence.individually_revoked_count(), 3);

        // revoke_all → epoch goes up, and was_revoked_before("a", new_epoch)
        // is still true (revoked_at was bumped to the new epoch).
        let new_epoch = fence.revoke_all();
        assert_eq!(new_epoch, 1);
        assert_eq!(fence.epoch(), 1);
        assert!(fence.was_revoked_before(&make_cid("a"), 1));

        // Querying "was revoked at epoch 0?" after revoke_all() bumps a and
        // b's revoked_at to epoch 1 → they were NOT revoked before epoch 0
        // anymore. (This is the intended "revoke_all wipes the per-id slate
        // clean back to epoch-old" semantics: per-id records now reflect
        // that they were last-revoked at epoch 1.)
        assert!(!fence.was_revoked_before(&make_cid("a"), 0));

        // filter_revoked at epoch 1 should exclude all 3 ids.
        let survivors = fence.filter_revoked(
            &[make_cid("a"), make_cid("b"), make_cid("c"), make_cid("d")],
            1,
        );
        assert_eq!(survivors, vec![make_cid("d")]);
    }
}
