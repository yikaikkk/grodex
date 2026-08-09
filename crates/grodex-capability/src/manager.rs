//! The CapabilityManager: turn-level base capture, per-Step overlay
//! application, live-revocation fencing, and invariant validation.
//!
//! Design Doc 10 §12–14:
//!
//! ```text
//! ┌ Turn begin ────────────────────────────────────────────────┐
//! │  1. CapabilityManager::capture_turn_base()                 │
//! │     -> TurnCapabilityBase (promotable_ids, epoch, snaps)   │
//! │                                                            │
//! │  ┌ Per Step ────────────────────────────────────────────┐  │
//! │  │ 2. LiveRevocationFence::epoch() → current_epoch      │  │
//! │  │ 3. (build StepCapabilitySnapshot)                    │  │
//! │  │ 4. TurnCapabilityOverlay::validate_against(base)     │  │
//! │  │ 5. CapabilityManager::apply_overlay(step, overlay)   │  │
//! │  └──────────────────────────────────────────────────────┘  │
//! │                                                            │
//! │  6. At end: ::validate_turn_consistency(base, overlays)    │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! The manager also enforces deferred promotion (fail-closed unless
//! explicitly configured per-Turn) and tracks the promotable-id closure
//! so Steps can never "accidentally" introduce a capability id that was
//! not visible at Turn start (invariant #10: capability set ceiling).

use crate::authority::Authority;
use crate::descriptor::{
    LiveRevocationFence, StepCapabilitySnapshot, TurnCapabilityBase, TurnCapabilityOverlay,
};
use crate::id::{CapabilityId, CapabilityKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Outcome of validate_* calls below — either Ok or a structured error
/// with enough context to surface to the model / diagnostics UI.
pub type VResult<T> = Result<T, CapabilityViolation>;

/// A single capability-policy violation reported by the manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityViolation {
    /// Short invariant identifier (e.g. "inv10-ceiling", "overlay-fence").
    pub code: String,
    /// Human-readable description of what went wrong.
    pub message: String,
    /// Capability id(s) implicated, if any.
    pub capability_ids: Vec<CapabilityId>,
    /// Turn id + optional step index for log correlation.
    pub turn_id: String,
    pub step_index: Option<usize>,
}

/// The top-level capability runtime manager.
///
/// Construct one per session: the manager holds the live-revocation fence
/// (epoch monotonic across Turns, shared with CapabilityResolver) and
/// exposes the turn-level capture / per-Step overlay / end-of-turn
/// validation primitives that make Design Doc 10 §12–13 enforceable in
/// a single canonical place, not scattered across every tool call site.
#[derive(Debug, Clone, Default)]
pub struct CapabilityManager {
    /// Monotonic live revocation fence shared by all Turns in this session.
    revocation_fence: LiveRevocationFence,
    /// Per-step-authority ceiling configuration: every Turn's authority
    /// ceiling is clamped to <= this value at capture time (enterprise
    /// override knob that can never be raised by a local config).
    session_authority_ceiling: Option<u8>,
    /// Whether the session allows deferred-to-Available promotion on a
    /// per-Step basis. When false (fail-closed enterprise default), the
    /// TurnCapabilityBase always inherits `deferred_promotion_allowed=false`
    /// regardless of per-Turn input.
    session_allow_deferred_promotion: bool,
}

/// Inputs captured once at Turn start and passed into
/// `capture_turn_base`. Represents the full promotable-id set before any
/// Step in the Turn executes.
#[derive(Debug, Clone)]
pub struct TurnBaseInputs<'a> {
    pub turn_id: &'a str,
    pub capability_generation: u64,
    /// Maximum authority ceiling *requested* for this Turn. Will be further
    /// clamped to `session_authority_ceiling` if set.
    pub requested_authority_ceiling: u8,
    /// All capability ids that are currently promotable (Available OR
    /// Deferred-but-visible) at Turn start. This is the closure that
    /// every Step's overlay `promoted_ids` must be a subset of.
    pub promotable_ids: Vec<CapabilityId>,
    /// One StepCapabilitySnapshot per Step expected in the Turn, in order.
    /// This models the scenario where the turn planner pre-declares Steps;
    /// if Steps are added on the fly, append to `step_snapshots` before
    /// running that Step's overlay validation.
    pub step_snapshots: Vec<StepCapabilitySnapshot>,
    /// Caller may request per-Turn deferred promotion permission. Subject
    /// to session_allow_deferred_promotion AND session-level overrides.
    pub request_deferred_promotion_allowed: bool,
}

/// The result of `apply_overlay` — mostly a marker type for future
/// expansion (it will also feed rollout audit events).
#[derive(Debug, Clone)]
pub struct AppliedOverlay<'a> {
    /// Step index this overlay was applied to.
    pub step_index: usize,
    /// Reference to the now-validated overlay.
    pub overlay: &'a TurnCapabilityOverlay,
    /// Effective (post-fence, post-closure) promoted ids after this step.
    pub effective_promoted_ids: BTreeSet<CapabilityId>,
}

impl CapabilityManager {
    // ── Construction / session-level overrides ──────────────────────

    pub fn new() -> Self { Self::default() }

    /// Enterprise dial: impose a global ceiling for every Turn in this
    /// session. A Turn's requested authority ceiling that is HIGHER than
    /// this value is silently clamped down (fail-closed — can never
    /// exceed the enterprise cap).
    pub fn with_session_authority_ceiling(mut self, ceiling: u8) -> Self {
        self.session_authority_ceiling = Some(ceiling);
        self
    }
    /// Session-wide gate for deferred promotion. When false (default,
    /// enterprise), no Turn in this session may promote Deferred items,
    /// regardless of per-Turn input. When true, per-Turn requests are
    /// honored.
    pub fn with_allow_deferred_promotion(mut self, allow: bool) -> Self {
        self.session_allow_deferred_promotion = allow;
        self
    }

    // ── Accessors to shared state ───────────────────────────────────

    /// Shared live-revocation fence reference. Used by CapabilityResolver
    /// to issue `revoke_all` / `revoke_ids`, and by Steps to stamp their
    /// current `revocation_fence_epoch`.
    pub fn revocation_fence(&self) -> &LiveRevocationFence {
        &self.revocation_fence
    }
    /// Mutable access to the revocation fence (for `revoke_all` /
    /// `revoke_ids` from the resolver or admin controls).
    pub fn revocation_fence_mut(&mut self) -> &mut LiveRevocationFence {
        &mut self.revocation_fence
    }

    // ── Step 1: capture turn base ───────────────────────────────────

    /// Capture the immutable TurnCapabilityBase at Turn-start (§12.1).
    ///
    /// This is the ONLY place a new Turn's authority ceiling and
    /// promotable-id closure are decided. Everything downstream — Step
    /// overlays, policy revalidation, crash recovery — re-validates
    /// against this base.
    pub fn capture_turn_base(&self, inputs: TurnBaseInputs<'_>) -> TurnCapabilityBase {
        // 1. Clamp authority ceiling to session-wide cap if present.
        let authority_ceiling = match self.session_authority_ceiling {
            Some(session_cap) => inputs.requested_authority_ceiling.min(session_cap),
            None => inputs.requested_authority_ceiling,
        };
        // 2. Session-wide deferred-promotion gate (fail-closed default).
        let deferred_promotion_allowed =
            self.session_allow_deferred_promotion && inputs.request_deferred_promotion_allowed;
        // 3. Deduplicate promotable ids to keep base_hash stable across
        //    accidental duplicate submissions. Sorted for determinism.
        let mut promotable_ids = inputs.promotable_ids;
        promotable_ids.sort();
        promotable_ids.dedup();
        // 4. Captured policy epoch is the current revocation-fence epoch —
        //    any revocation happening mid-Turn will have a higher epoch
        //    and Steps must reflect that in their overlay.
        let policy_epoch_at_start = self.revocation_fence.epoch();

        TurnCapabilityBase::new(
            inputs.turn_id,
            inputs.capability_generation,
            authority_ceiling,
            promotable_ids,
            inputs.step_snapshots,
            deferred_promotion_allowed,
            policy_epoch_at_start,
        )
    }

    // ── Pre-validate: build a suggested Step overlay automatically ──

    /// Given a Step's candidate promotion set + any deferred ids the
    /// Step wishes to promote, build a TurnCapabilityOverlay that passes
    /// validate_against the base (or return the violations that would
    /// occur) plus the fence's current epoch.
    ///
    /// This is the helper that Step builders should use by default;
    /// hand-constructing overlays is possible but error-prone.
    pub fn suggest_overlay(
        &self,
        base: &TurnCapabilityBase,
        step_index: usize,
        candidate_promoted_ids: impl IntoIterator<Item = CapabilityId>,
        candidate_deferred_ids: impl IntoIterator<Item = CapabilityId>,
        reason: Option<String>,
    ) -> VResult<TurnCapabilityOverlay> {
        let snap = base
            .step_snapshot(step_index)
            .ok_or_else(|| CapabilityViolation {
                code: "step-out-of-range".into(),
                message: format!(
                    "step_index {step_index} out of range for turn {} with {} steps",
                    base.turn_id,
                    base.step_snapshots.len()
                ),
                capability_ids: Vec::new(),
                turn_id: base.turn_id.clone(),
                step_index: Some(step_index),
            })?;

        let current_fence_epoch = self.revocation_fence.epoch();

        // Drop any candidate that has been revoked at-or-before current_fence_epoch.
        let promoted_before_fence: BTreeSet<CapabilityId> =
            candidate_promoted_ids.into_iter().collect();
        let deferred_before_fence: BTreeSet<CapabilityId> =
            candidate_deferred_ids.into_iter().collect();
        let promoted: Vec<CapabilityId> = self
            .revocation_fence
            .filter_revoked(&promoted_before_fence.into_iter().collect::<Vec<_>>(), current_fence_epoch);
        let deferred: Vec<CapabilityId> = self
            .revocation_fence
            .filter_revoked(&deferred_before_fence.into_iter().collect::<Vec<_>>(), current_fence_epoch);

        let overlay = TurnCapabilityOverlay {
            turn_id: base.turn_id.clone(),
            step_index,
            snapshot_id: snap.snapshot_id.to_string(),
            promoted_ids: promoted,
            deferred_promoted_ids: deferred,
            revocation_fence_epoch: current_fence_epoch,
            reason,
        };

        // Always validate the suggested overlay before returning it — this
        // catches "promoted id not in promotable_ids" and deferred gating.
        overlay.validate_against(base).map_err(|msg| CapabilityViolation {
            code: "overlay-invalid".into(),
            message: msg,
            capability_ids: {
                let mut all: Vec<CapabilityId> = overlay.promoted_ids.clone();
                all.extend(overlay.deferred_promoted_ids.clone());
                all
            },
            turn_id: base.turn_id.clone(),
            step_index: Some(step_index),
        })?;

        Ok(overlay)
    }

    // ── Apply an overlay ────────────────────────────────────────────

    /// Apply a per-Step overlay after validating it against the turn
    /// base and the current revocation fence. Returns effective promoted
    /// ids (useful for rollout stamping / capability snapshotting).
    pub fn apply_overlay<'a>(
        &'a self,
        base: &'a TurnCapabilityBase,
        step_index: usize,
        overlay: &'a TurnCapabilityOverlay,
    ) -> VResult<AppliedOverlay<'a>> {
        if overlay.step_index != step_index {
            return Err(CapabilityViolation {
                code: "overlay-step-mismatch".into(),
                message: format!(
                    "apply_overlay called with step_index={} but overlay reports step_index={}",
                    step_index, overlay.step_index
                ),
                capability_ids: Vec::new(),
                turn_id: base.turn_id.clone(),
                step_index: Some(step_index),
            });
        }

        // 1. Validate overlay invariants against base.
        overlay
            .validate_against(base)
            .map_err(|msg| CapabilityViolation {
                code: "overlay-invariant".into(),
                message: msg,
                capability_ids: {
                    let mut all: Vec<CapabilityId> = overlay.promoted_ids.clone();
                    all.extend(overlay.deferred_promoted_ids.clone());
                    all
                },
                turn_id: base.turn_id.clone(),
                step_index: Some(step_index),
            })?;

        // 2. Also assert the overlay's fence epoch is consistent with the
        //    *current* fence — if a revoke_all happened since the overlay
        //    was built, the overlay must reference the newer epoch (it
        //    cannot "pretend" to still be at an older epoch).
        if overlay.revocation_fence_epoch < self.revocation_fence.epoch() {
            return Err(CapabilityViolation {
                code: "overlay-fence-stale".into(),
                message: format!(
                    "overlay revocation_fence_epoch={} predates manager's live epoch={}; rebuild suggest_overlay() first",
                    overlay.revocation_fence_epoch,
                    self.revocation_fence.epoch()
                ),
                capability_ids: Vec::new(),
                turn_id: base.turn_id.clone(),
                step_index: Some(step_index),
            });
        }

        // 3. Compute effective promoted ids: promoted_ids UNION
        //    deferred_promoted_ids, deduplicated, still alive at the
        //    overlay's epoch.
        let mut eff: BTreeSet<CapabilityId> = BTreeSet::new();
        for id in overlay.promoted_ids.iter().chain(overlay.deferred_promoted_ids.iter()) {
            if !self
                .revocation_fence
                .was_revoked_before(id, overlay.revocation_fence_epoch)
            {
                eff.insert(id.clone());
            }
        }

        Ok(AppliedOverlay { step_index, overlay, effective_promoted_ids: eff })
    }

    // ── End-of-turn consistency validation ──────────────────────────

    /// Validate the whole turn end-to-end: every Step has an overlay,
    /// all overlays pass validate_against, all promote strictly within
    /// base.promotable_ids, no fence epochs go backwards across Steps
    /// (monotonic), and there are no "orphan" Step snapshots without
    /// matching overlays (or vice versa).
    ///
    /// This is the canonical "Turn complete" check run just before a
    /// Turn is committed to rollout; if it fails, the Turn is aborted
    /// or retried rather than committed with an inconsistent capability
    /// history.
    pub fn validate_turn_consistency(
        &self,
        base: &TurnCapabilityBase,
        overlays: &[TurnCapabilityOverlay],
    ) -> VResult<()> {
        // 1. Same number of overlays as Step snapshots.
        if overlays.len() != base.step_snapshots.len() {
            return Err(CapabilityViolation {
                code: "turn-step-arity".into(),
                message: format!(
                    "turn {} has {} step snapshots but {} overlays",
                    base.turn_id,
                    base.step_snapshots.len(),
                    overlays.len()
                ),
                capability_ids: Vec::new(),
                turn_id: base.turn_id.clone(),
                step_index: None,
            });
        }
        // 2. Fence epoch must be monotonic across Steps (time only moves
        //    forward; a later Step cannot reference a revocation epoch
        //    older than an earlier Step).
        let mut max_fence_seen: u64 = base.policy_epoch_at_start;
        for (step_index, overlay) in overlays.iter().enumerate() {
            if overlay.revocation_fence_epoch < max_fence_seen {
                return Err(CapabilityViolation {
                    code: "turn-fence-not-monotonic".into(),
                    message: format!(
                        "step {step_index} fence_epoch={} is older than previous max={}",
                        overlay.revocation_fence_epoch, max_fence_seen
                    ),
                    capability_ids: Vec::new(),
                    turn_id: base.turn_id.clone(),
                    step_index: Some(step_index),
                });
            }
            max_fence_seen = overlay.revocation_fence_epoch;
            // 3. Per-overlay invariant validation.
            overlay
                .validate_against(base)
                .map_err(|msg| CapabilityViolation {
                    code: "turn-overlay-invariant".into(),
                    message: format!("step {step_index}: {msg}"),
                    capability_ids: {
                        let mut all: Vec<CapabilityId> = overlay.promoted_ids.clone();
                        all.extend(overlay.deferred_promoted_ids.clone());
                        all
                    },
                    turn_id: base.turn_id.clone(),
                    step_index: Some(step_index),
                })?;
        }
        Ok(())
    }

    // ── Cross-tool helper: build promotable_ids from a capability set ─

    /// Convenience: given a bag of (id, kind, authority, availability)
    /// tuples, compute the promotable-id closure for a Turn: any
    /// capability with authority <= the turn's authority_ceiling AND
    /// (availability == Available OR availability == Deferred).
    ///
    /// `Revoked` ids are never included here (they must also be marked
    /// in LiveRevocationFence for cross-checking; this is a belt-and-
    /// suspenders approach where both layers agree).
    pub fn compute_promotable_ids(
        turn_authority_ceiling: u8,
        caps: impl IntoIterator<Item = (CapabilityId, CapabilityKind, Authority, Availability)>,
    ) -> Vec<CapabilityId> {
        caps.into_iter()
            .filter_map(|(id, _kind, auth, avail)| {
                if auth.level() > turn_authority_ceiling {
                    return None;
                }
                match avail {
                    Availability::Available | Availability::Deferred => Some(id),
                    Availability::Revoked | Availability::Disabled | Availability::Hidden => None,
                }
            })
            .collect()
    }

    // ── Report helpers for diagnostics UI ────────────────────────────

    /// Build a short summary (promotable count, deferred allowed, policy
    /// epoch, step count) useful for `prompt explain` / `turn status`
    /// style commands.
    pub fn summarize_turn(base: &TurnCapabilityBase) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        out.insert("turn_id".into(), base.turn_id.clone());
        out.insert("turn_generation".into(), base.turn_generation.to_string());
        out.insert("authority_ceiling".into(), base.authority_ceiling.to_string());
        out.insert("promotable_id_count".into(), base.promotable_ids.len().to_string());
        out.insert(
            "deferred_promotion_allowed".into(),
            base.deferred_promotion_allowed.to_string(),
        );
        out.insert("policy_epoch_at_start".into(), base.policy_epoch_at_start.to_string());
        out.insert("step_count".into(), base.step_snapshots.len().to_string());
        out.insert("base_hash".into(), base.base_hash.clone());
        out
    }
}

/// Simple availability enumeration used by `compute_promotable_ids`.
///
/// This mirrors the "availability" concept carried elsewhere in the
/// capability layer; it is redefined here (rather than pulling in a
/// heavier resolver type) so manager.rs remains lightweight and unit-
/// testable without a full resolver stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Promotable directly.
    Available,
    /// Promotable only via deferred_promotion_allowed gate.
    Deferred,
    /// Never promotable.
    Revoked,
    /// Never promotable (config disabled).
    Disabled,
    /// Registered but not exposed to the model at all.
    Hidden,
}
