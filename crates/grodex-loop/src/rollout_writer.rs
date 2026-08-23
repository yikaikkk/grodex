//! RolloutWriter — thin writer façade over the [`RolloutStore`] trait.
//!
//! **P0-1 reliability rewrite** — seq sovereignty has moved into the
//! single-writer [`JournalHandle`] (see [`grodex_rollout::journal_actor`]
//! for the contract). This struct no longer holds a `AtomicU64` counter.
//! All it does is:
//!
//! 1. Build the typed [`RolloutEvent`] for each state transition.
//! 2. Ship it to the store's `append_event` / `append_event_durable`
//!    method (which dispatches to the actor).
//! 3. On [`rebind`](Self::rebind), orchestrate the swap of the shared
//!    `RolloutWriterInner` *after* the actor has quiesced, so old-clone
//!    writes cannot leak into a resumed session's journal.
//!
//! ### Durability tiers
//!
//! Writes fall into two tiers. The store decides what "durable" actually
//! means (for the FileRolloutStore it means force `sync_data`; for an
//! in-memory store in a test it is a no-op). Callers choose the tier:
//!
//! | Method | Tier | Rationale |
//! |--------|------|-----------|
//! | `write_state`, `write_user_input`, `write_step_started`, `write_model_output` | normal | Losing the last ~8 of these on power loss is acceptable; they will be re-sampled on resume. |
//! | `write_tool_execution_started`, `write_tool_finished`, `write_tool_execution_finished`, `write_turn_completed`, `write_context_restored`, `write_compaction` | **durable** (`force_fsync=true`) | These gate side-effects or turn boundaries. A tool process *must not* be spawned before its ToolExecutionStarted event is on platters (otherwise recovery cannot find it). |

use grodex_core::error::GrodexError;
use grodex_core::id::{SessionId, StepGeneration, StepId, TurnId};
use grodex_rollout::event::{RolloutEvent, RolloutEventType, SensitivityLevel};
use grodex_rollout::store::RolloutStore;
use std::sync::{Arc, RwLock};

/// Inner state shared across all [`RolloutWriter`] clones. When
/// `/resume <old_session_id>` rebinds the writer, every outstanding
/// clone (supervisor, coordinator, durable sub-agent) sees the new
/// store/session_id immediately — no stale writes to the ephemeral
/// "new-session" empty journal.
///
/// The store is behind an `Arc<dyn RolloutStore>` so that swapping it
/// here does **not** break the existing single-writer actor inside the
/// old store: the actor's in-flight queue was drained by
/// [`JournalHandle::rebind`] BEFORE we overwrite this pointer (see
/// [`Self::rebind`]). Any clone that was in the middle of
/// `.append_event().await` holds an `Arc<old_store>` in its call
/// stack — but since that old store's actor was rebind-quiesced *and*
/// session-id-mismatch defence rejects events bound to a now-stale
/// session id, the only possible outcome is a clean error (not silent
/// corruption).
#[derive(Clone)]
struct RolloutWriterInner {
    store: Arc<dyn RolloutStore>,
    session_id: SessionId,
}

/// Manages journal writes through the shared store. Cloneable — the
/// inner state is behind `Arc` and all seq assignment is delegated to
/// the store's single-writer actor.
#[derive(Clone)]
pub struct RolloutWriter {
    inner: Arc<RwLock<RolloutWriterInner>>,
}

impl RolloutWriter {
    pub fn new(store: Arc<dyn RolloutStore>, session_id: SessionId) -> Self {
        Self {
            inner: Arc::new(RwLock::new(RolloutWriterInner { store, session_id })),
        }
    }

    /// Pre-seed the expected seq counter from an existing journal (crash
    /// recovery). After a replay the writer must continue from the next
    /// seq so newly appended events don't collide with replayed ones.
    ///
    /// **P0-1 note**: this used to write to an in-process `AtomicU64`;
    /// it now resolves to a no-op on the `RolloutWriter` level and only
    /// has an effect if the underlying store exposes a
    /// `FileRolloutStore` with a [`JournalHandle`] (which is reseeded at
    /// [`FileRolloutStore::new`] construction time with the correct
    /// `next_seq`). Callers that construct stores *without* passing
    /// `next_seq` to the ctor will double-count — please keep using the
    /// pattern `FileRolloutStore::new(base, sid, next_seq, policy).await`.
    ///
    /// We keep this method for source compatibility with existing tests
    /// that call `writer2.resume_from(replayed.len() as u64)`; it is
    /// intentionally a documentation-level no-op on structs that don't
    /// own the seq counter anymore.
    #[allow(clippy::unused_self)]
    pub fn resume_from(&self, _next_seq: u64) {
        // See doc comment. The seq counter lives inside JournalHandle
        // now and was already seeded at FileRolloutStore construction.
    }

    /// Obsolescent helper. Returns `u64::MAX` in all builds; the real
    /// seq comes from the store's single-writer actor on the success
    /// path of every append.
    #[deprecated(note = "seq is now allocated inside the single-writer journal actor; use the Ok(seq) returned from each write_* call")]
    pub fn next_seq(&self) -> u64 {
        u64::MAX
    }

    /// Store access (recovery code reads via the same handle).
    ///
    /// Returns a cloned `Arc` because the inner may be swapped by a
    /// concurrent [`rebind`](Self::rebind) call; returning a borrowed
    /// reference would outlive the read guard.
    pub fn store(&self) -> Arc<dyn RolloutStore> {
        self.inner
            .read()
            .expect("RolloutWriter inner poisoned")
            .store
            .clone()
    }

    pub fn session_id(&self) -> SessionId {
        self.inner
            .read()
            .expect("RolloutWriter inner poisoned")
            .session_id
    }

    /// Swap the attached rollout store + session id. Every [`Clone`]d
    /// handle sees the same swap (single shared `Arc<RwLock<Inner>>`).
    ///
    /// **Quiesce ordering (the hard part)**: the caller must guarantee
    /// that `new_store` has *already* been bound through a
    /// [`FileRolloutStore::rebind`] or constructed with a freshly
    /// quiesced actor. We do NOT reach into the store from here because
    /// `dyn RolloutStore` is trait-object-erased — rebind is the
    /// caller's responsibility (see the `/resume` handler in
    /// `supervisor.rs` which does the steps in the correct order).
    ///
    /// The `next_seq` argument is kept for signature compat but no
    /// longer has an effect at this layer.
    pub fn rebind(
        &self,
        new_store: Arc<dyn RolloutStore>,
        new_session_id: SessionId,
        _next_seq: u64,
    ) {
        let mut w = self.inner.write().expect("RolloutWriter inner poisoned");
        w.store = new_store;
        w.session_id = new_session_id;
        drop(w);
    }

    // ── Append helpers (private) ───────────────────────────────────

    /// Build the event + dispatch to the store.
    async fn write(
        &self,
        event_type: RolloutEventType,
        turn_id: Option<TurnId>,
        step_id: Option<StepId>,
        generation: Option<StepGeneration>,
        payload: serde_json::Value,
        durable: bool,
    ) -> Result<u64, GrodexError> {
        let store = self.store();
        let session_id = self.session_id();
        let event = RolloutEvent {
            schema_version: 2,
            seq: 0, // filled in by the journal actor — sovereignty principle
            session_id,
            turn_id,
            step_id,
            generation,
            timestamp: chrono::Utc::now(),
            event_type,
            payload,
            sensitivity: SensitivityLevel::Normal,
        };
        if durable {
            store.append_event_durable(event).await
        } else {
            store.append_event(event).await
        }
    }

    // ── Public API ─────────────────────────────────────────────────

    pub async fn write_state(&self, state: &str) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::RuntimeStateChanged,
            None,
            None,
            None,
            serde_json::json!({"state": state}),
            false,
        )
        .await
    }

    /// Persist context items restored from a prior session journal during
    /// `resume`. Written at the start of a new session so a second crash
    /// does not lose the recovered history. **Durable** — this event is
    /// the only surviving record of a restore; we fsync before proceeding.
    pub async fn write_context_restored(
        &self,
        items: &[grodex_core::context::ContextItem],
    ) -> Result<u64, GrodexError> {
        let payload = serde_json::json!({
            "items": items,
        });
        self.write(
            RolloutEventType::ContextRestored,
            None,
            None,
            None,
            payload,
            true, // durable: second crash must not lose the restore
        )
        .await
    }

    pub async fn write_user_input(&self, turn_id: TurnId, text: &str) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::UserInputAccepted,
            Some(turn_id),
            None,
            None,
            serde_json::json!({"text": text}),
            false,
        )
        .await
    }

    pub async fn write_model_output(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        assistant_text: &str,
        tool_calls: &[serde_json::Value],
        reasoning: Option<&str>,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "assistant_text": assistant_text,
            "tool_calls": tool_calls,
        });
        if let Some(r) = reasoning {
            payload["reasoning"] = serde_json::json!(r);
        }
        self.write(
            RolloutEventType::ModelItemProduced,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            payload,
            false,
        )
        .await
    }

    pub async fn write_step_boundary(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
    ) -> Result<u64, GrodexError> {
        // NOTE: prior builds reused `ToolCallPrepared` for this, which
        // conflated step boundaries with real prepared tool calls. The
        // recovery reducer's lifecycle state machine strictly expects
        // ToolCallPrepared to mean the beginning of a specific call_id
        // lifecycle, so step boundaries use RuntimeStateChanged with a
        // phase tag (purely observational — NOT durable).
        self.write(
            RolloutEventType::RuntimeStateChanged,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            serde_json::json!({"phase": "step_started"}),
            false,
        )
        .await
    }

    /// Legacy alias — retained for call sites that still reference
    /// `write_step_started`. Immediately after this merge we should
    /// grep the whole tree and replace them, but keeping the alias
    /// avoids breaking compilation mid-refactor.
    #[deprecated(note = "Use write_step_boundary to avoid conflating step boundaries with ToolCallPrepared")]
    pub async fn write_step_started(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
    ) -> Result<u64, GrodexError> {
        self.write_step_boundary(turn_id, step_id, generation).await
    }

    // ── Tool-call lifecycle (schema v2) ─────────────────────────────
    //
    // These methods encode the authoritative prepared → approved →
    // started → finished → committed state machine. Durability choices:
    //   Prepared, Approved     — durable (recovery reads them to answer "did we intend to run?")
    //   Started                — durable (pre-side-effect fence)
    //   Finished               — durable (proves tool ran to completion)
    //   ResultCommitted        — durable (proves model saw the result)
    //   Indeterminate/Resolved — durable (human-resolution audit trail)

    /// Persist a parsed-and-validated tool call. Always durable.
    ///
    /// `operation_id` MUST be populated for side-effecting tools. It
    /// is the idempotency key checked by
    /// `RecoveryCheckpoint::is_safe_to_replay` across restarts.
    ///
    /// `args_hash`, `capability_revision` and `policy_generation` are
    /// optional audit fields. If the same Prepared event were replayed
    /// with different values, the system would refuse to resume under
    /// semantic drift (future work).
    pub async fn write_tool_call_prepared(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        operation_id: Option<&str>,
        tool_name: &str,
        args: &serde_json::Value,
        args_hash: Option<&str>,
        capability_revision: Option<&str>,
        policy_generation: Option<u64>,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "call_id": call_id,
            "name": tool_name,
            "args": args,
        });
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        if let Some(h) = args_hash { payload["args_hash"] = serde_json::json!(h); }
        if let Some(cr) = capability_revision { payload["capability_revision"] = serde_json::json!(cr); }
        if let Some(pg) = policy_generation { payload["policy_generation"] = serde_json::json!(pg); }
        self.write(
            RolloutEventType::ToolCallPrepared,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            payload,
            true,
        ).await
    }

    /// Persist the durable "go-ahead" for a prepared call. Written
    /// just before spawning the side-effecting process (or immediately
    /// after ToolCallPrepared if auto-approve). Always durable.
    pub async fn write_tool_call_approved(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        operation_id: Option<&str>,
        tool_name: &str,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "call_id": call_id,
            "name": tool_name,
        });
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        self.write(
            RolloutEventType::ToolCallApproved,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            payload,
            true,
        ).await
    }

    /// ToolExecutionStarted. **Durable** because the next step is
    /// actually spawning the side-effecting process; on post-crash
    /// recovery, seeing *this event without a matching
    /// ToolExecutionFinished / ToolResultCommitted* is exactly what
    /// produces the `Indeterminate` classification.
    pub async fn write_tool_started(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        operation_id: Option<&str>,
        name: &str,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({"call_id": call_id, "name": name});
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        self.write(
            RolloutEventType::ToolExecutionStarted,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            payload,
            true, // durable: pre-side-effect commit point
        )
        .await
    }

    /// ToolExecutionFinished — records the instant the tool returned
    /// (before the result is committed to the transcript). Durable
    /// because it is the pair to ToolExecutionStarted: if both are on
    /// platters we know the tool did run to completion, even if the
    /// later `ToolResultCommitted` is missing from the journal.
    ///
    /// Contrary to the prior build we now record `content`,
    /// `exit_code`, and `duration_ms` here too, so a resume that finds
    /// *only* this event (and no ToolResultCommitted) has enough info
    /// to reconstruct the result without re-running the tool.
    pub async fn write_tool_execution_finished(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        operation_id: Option<&str>,
        is_error: bool,
        content: Option<&str>,
        exit_code: Option<i32>,
        duration_ms: Option<u64>,
        output_truncated: Option<bool>,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({"call_id": call_id, "is_error": is_error});
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        if let Some(c) = content { payload["content"] = serde_json::json!(c); }
        if let Some(ec) = exit_code { payload["exit_code"] = serde_json::json!(ec); }
        if let Some(d) = duration_ms { payload["duration_ms"] = serde_json::json!(d); }
        if let Some(ot) = output_truncated { payload["output_truncated"] = serde_json::json!(ot); }
        self.write(
            RolloutEventType::ToolExecutionFinished,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            payload,
            true,
        )
        .await
    }

    /// ToolResultCommitted — the result has been folded into the
    /// session transcript. Durable because the next sampling step reads
    /// the transcript; if we crash after sampling we need to be able to
    /// prove what result the model saw.
    pub async fn write_tool_finished(
        &self,
        turn_id: TurnId,
        step_id: StepId,
        generation: StepGeneration,
        call_id: &str,
        operation_id: Option<&str>,
        content: &str,
        is_error: bool,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({"call_id": call_id, "content": content, "is_error": is_error});
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        self.write(
            RolloutEventType::ToolResultCommitted,
            Some(turn_id),
            Some(step_id),
            Some(generation),
            payload,
            true,
        )
        .await
    }

    /// Post-crash marker: `call_id` has `ToolExecutionStarted` on
    /// disk without a matching `ToolExecutionFinished`, so the
    /// side-effect is in an unknown state. Once this event is written,
    /// the supervisor/tool layer MUST refuse to re-execute the call
    /// until a matching `ToolOutcomeResolved` is observed. Always
    /// durable.
    pub async fn write_tool_outcome_indeterminate(
        &self,
        call_id: &str,
        operation_id: Option<&str>,
        tool_name: &str,
        reason: &str,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "call_id": call_id,
            "name": tool_name,
            "reason": reason,
        });
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        self.write(
            RolloutEventType::ToolOutcomeIndeterminate,
            None,
            None,
            None,
            payload,
            true,
        ).await
    }

    /// Human resolution of an `Indeterminate` call. Always durable.
    ///
    /// `resolution` must be one of:
    ///   * `confirmed_executed`  — the side-effect DID take place out-of-band; `resolved_content` is its result
    ///   * `confirmed_not_executed` — the side-effect definitely did NOT take place
    ///   * `terminated`          — operator chose to abort the call without deciding
    pub async fn write_tool_outcome_resolved(
        &self,
        call_id: &str,
        operation_id: Option<&str>,
        resolution: &str,
        resolved_content: Option<&str>,
        resolver_id: Option<&str>,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "call_id": call_id,
            "resolution": resolution,
            "resolved_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        if let Some(rc) = resolved_content { payload["resolved_content"] = serde_json::json!(rc); }
        if let Some(r) = resolver_id { payload["resolver_id"] = serde_json::json!(r); }
        self.write(
            RolloutEventType::ToolOutcomeResolved,
            None,
            None,
            None,
            payload,
            true,
        ).await
    }

    // ── Approval / Lease lifecycle (P0-4 persistence surface) ───────
    //
    // These methods are the durable arm of the approval state machine.
    // The broker (memory or persistent) decides *what* to write; the
    // journal is the source of truth for replay / auditing / resume.

    pub async fn write_approval_requested(
        &self,
        call_id: Option<&str>,
        operation_id: Option<&str>,
        ticket_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        requested_by: &str,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "ticket_id": ticket_id,
            "tool_name": tool_name,
            "args": args,
            "requested_by": requested_by,
            "requested_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(c) = call_id { payload["call_id"] = serde_json::json!(c); }
        if let Some(op) = operation_id { payload["operation_id"] = serde_json::json!(op); }
        self.write(RolloutEventType::ApprovalRequested, None, None, None, payload, true).await
    }

    pub async fn write_approval_resolved(
        &self,
        ticket_id: &str,
        call_id: Option<&str>,
        resolution: &str, // "approved" | "rejected" | "expired" | "narrowed"
        resolved_by: Option<&str>,
        narrowed_args: Option<&serde_json::Value>,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "ticket_id": ticket_id,
            "resolution": resolution,
        });
        if let Some(c) = call_id { payload["call_id"] = serde_json::json!(c); }
        if let Some(r) = resolved_by { payload["resolved_by"] = serde_json::json!(r); }
        if let Some(n) = narrowed_args { payload["narrowed_args"] = n.clone(); }
        self.write(RolloutEventType::ApprovalResolved, None, None, None, payload, true).await
    }

    pub async fn write_lease_issued(
        &self,
        lease_id: &str,
        ticket_id: &str,
        call_id: &str,
        ttl_secs: Option<u64>,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "lease_id": lease_id,
            "ticket_id": ticket_id,
            "call_id": call_id,
            "issued_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(t) = ttl_secs { payload["ttl_secs"] = serde_json::json!(t); }
        self.write(RolloutEventType::LeaseIssued, None, None, None, payload, true).await
    }

    pub async fn write_lease_consumed(
        &self,
        lease_id: &str,
        call_id: &str,
    ) -> Result<u64, GrodexError> {
        let payload = serde_json::json!({
            "lease_id": lease_id,
            "call_id": call_id,
            "consumed_at": chrono::Utc::now().to_rfc3339(),
        });
        self.write(RolloutEventType::LeaseConsumed, None, None, None, payload, true).await
    }

    pub async fn write_lease_expired(
        &self,
        lease_id: &str,
        reason: &str,
    ) -> Result<u64, GrodexError> {
        let payload = serde_json::json!({"lease_id": lease_id, "reason": reason});
        self.write(RolloutEventType::LeaseExpired, None, None, None, payload, true).await
    }

    /// Record a Turn-level skill snapshot (Design Doc 08 §6).
    /// Writes the skill name, source, path, and content_hash for each
    /// skill active during the Turn, plus the skill_generation counter.
    /// This enables version auditing: replay can determine exactly
    /// which version of each skill the model saw.
    pub async fn write_skill_snapshot(
        &self,
        turn_id: TurnId,
        skills: &[serde_json::Value],
        skill_generation: u64,
    ) -> Result<u64, GrodexError> {
        let payload = serde_json::json!({
            "skills": skills,
            "skill_generation": skill_generation,
        });
        self.write(
            RolloutEventType::SkillSnapshotRecorded,
            Some(turn_id),
            None,
            None,
            payload,
            true,
        ).await
    }

    /// Durable record of a tool-call parameter revision created by a
    /// user Narrow operation. Subsequent lookups (tool executor on
    /// resume, rollout replay) should prefer this revision's args over
    /// the original `prepared.args`. Always durable.
    pub async fn write_effective_tool_call_revision(
        &self,
        call_id: &str,
        tool_name: Option<&str>,
        revision: u64,
        narrowed_args: &serde_json::Value,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "tool_call_id": call_id,
            "revision": revision,
            "narrowed_args": narrowed_args,
        });
        if let Some(tn) = tool_name { payload["tool_name"] = serde_json::json!(tn); }
        self.write(
            RolloutEventType::EffectiveToolCallRevisionCreated,
            None,
            None,
            None,
            payload,
            true,
        ).await
    }

    /// Durable record that a tool call was refused because its bound
    /// capability generation was evicted from the ring buffer. Always
    /// durable — this is an audit event that explains *why* a call
    /// didn't execute during recovery / replay.
    pub async fn write_capability_call_rejected_stale(
        &self,
        turn_id: Option<TurnId>,
        step_id: Option<StepId>,
        generation: Option<StepGeneration>,
        capability_id: &str,
        bound_generation: u64,
        reason: &str,
    ) -> Result<u64, GrodexError> {
        let payload = serde_json::json!({
            "capability_id": capability_id,
            "bound_generation": bound_generation,
            "reason": reason,
        });
        self.write(
            RolloutEventType::CapabilityCallRejectedStale,
            turn_id,
            step_id,
            generation,
            payload,
            true,
        ).await
    }

    /// TurnCompleted — durable turn boundary. Resume replays will not
    /// re-execute a closed turn.
    pub async fn write_turn_completed(&self, turn_id: TurnId) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::TurnCompleted,
            Some(turn_id),
            None,
            None,
            serde_json::json!({}),
            true,
        )
        .await
    }

    /// ToolResultCommitted (synthetic) — emitted by the supervisor when
    /// a turn is aborted mid-tool-execution (`cancel_turn`). The real
    /// result never arrives, so we commit an error placeholder to keep
    /// every ToolCall paired; otherwise resume replay fails validation
    /// with OrphanedToolResult. step_id/generation are None because the
    /// step never completed — the reducer only validates generation
    /// when present, so this cannot regress monotonicity.
    pub async fn write_tool_result_interrupted(
        &self,
        turn_id: TurnId,
        call_id: &str,
        content: &str,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::ToolResultCommitted,
            Some(turn_id),
            None,
            None,
            serde_json::json!({
                "call_id": call_id,
                "content": content,
                "is_error": true,
                "interrupted": true
            }),
            true,
        )
        .await
    }

    /// CompactionCommitted — must be durable; a post-crash replay that
    /// skips compaction would re-read a stale oversized transcript and
    /// produce a different model output (semantic drift).
    pub async fn write_compaction(
        &self,
        turn_id: Option<TurnId>,
        items: &[grodex_core::context::ContextItem],
    ) -> Result<u64, GrodexError> {
        let items_json: Vec<serde_json::Value> = items
            .iter()
            .filter_map(|i| serde_json::to_value(i).ok())
            .collect();
        self.write(
            RolloutEventType::CompactionCommitted,
            turn_id,
            None,
            None,
            serde_json::json!({"items": items_json}),
            true,
        )
        .await
    }

    /// CompactionStarted — durable marker that a compaction cycle has
    /// begun (plan built, model request about to be sent). On crash
    /// replay, a Started without a matching Committed/Failed means
    /// the compaction was in-flight and must NOT be installed.
    pub async fn write_compaction_started(
        &self,
        turn_id: Option<TurnId>,
        trigger: &str,
        pre_compaction_item_count: usize,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::CompactionStarted,
            turn_id,
            None,
            None,
            serde_json::json!({
                "trigger": trigger,
                "pre_compaction_item_count": pre_compaction_item_count,
            }),
            true,
        ).await
    }

    /// CompactionCandidateBuilt — durable record that the model
    /// returned a summary and the candidate context has been assembled
    /// but NOT yet verified / installed. On crash replay, a
    /// CandidateBuilt without a Committed means the candidate exists
    /// but verification was interrupted.
    pub async fn write_compaction_candidate_built(
        &self,
        turn_id: Option<TurnId>,
        candidate_item_count: usize,
        summary_preview: &str,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::CompactionCandidateBuilt,
            turn_id,
            None,
            None,
            serde_json::json!({
                "candidate_item_count": candidate_item_count,
                "summary_preview": summary_preview.chars().take(200).collect::<String>(),
            }),
            true,
        ).await
    }

    /// CompactionFailed — durable record that compaction was attempted
    /// but failed (model error, journal write failure, verification
    /// failure, etc.). The old context is preserved. Always durable
    /// so resume knows the compaction was explicitly aborted, not
    /// interrupted mid-flight.
    pub async fn write_compaction_failed(
        &self,
        turn_id: Option<TurnId>,
        reason: &str,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::CompactionFailed,
            turn_id,
            None,
            None,
            serde_json::json!({"reason": reason}),
            true,
        ).await
    }

    /// Route events — observability-only; NOT durable (state transitions
    /// never depend on them). Batched with the every-8 fsync policy.
    pub async fn write_route_event(
        &self,
        turn_id: Option<TurnId>,
        event_kind: &str,
        candidate_id: &str,
        details: serde_json::Value,
    ) -> Result<u64, GrodexError> {
        self.write(
            RolloutEventType::ModelRouteEvent,
            turn_id,
            None,
            None,
            serde_json::json!({
                "event_kind": event_kind,
                "candidate_id": candidate_id,
                "details": details,
            }),
            false,
        )
        .await
    }

    /// Record an application-initiated tool call (invariant #17).
    ///
    /// AppOnly calls bypass model sampling — they are produced by
    /// application logic (auto-compaction, system maintenance, human-
    /// initiated side effects). They MUST still be journaled so that
    /// recovery/replay can account for their side effects.
    ///
    /// Always durable — these calls have no model-side record, so
    /// the journal entry is the only audit trail.
    pub async fn write_app_only_tool_call(
        &self,
        turn_id: Option<TurnId>,
        call_id: &str,
        operation_id: Option<&str>,
        tool_name: &str,
        args: &serde_json::Value,
        result: &serde_json::Value,
        reason: &str,
    ) -> Result<u64, GrodexError> {
        let mut payload = serde_json::json!({
            "call_id": call_id,
            "tool_name": tool_name,
            "args": args,
            "result": result,
            "reason": reason,
        });
        if let Some(op) = operation_id {
            payload["operation_id"] = serde_json::json!(op);
        }
        self.write(
            RolloutEventType::AppOnlyToolCall,
            turn_id,
            None,
            None,
            payload,
            true, // durable: only audit trail for app-initiated calls
        )
        .await
    }
}
