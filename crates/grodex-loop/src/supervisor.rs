//! SessionSupervisor — tokio::select! event loop with spawned turns.
//!
//! Following Grok's `run_session` pattern: the actor loop multiplexes
//! multiple event sources (commands, turn completions, timers, rollout)
//! while turn execution runs in spawned tasks with AbortHandle.

use crate::chat_state::ChatStateHandle;
use crate::command::{SessionCommand, SessionEvent};
use crate::reducer::SessionReducer;
use crate::rollout_writer::RolloutWriter;
use grodex_rollout::event::RolloutEventType;
use crate::session::Session;
use crate::step::TurnOutcome;
use crate::turn::TurnContext;
use crate::turn_coordinator::TurnCoordinator;
use grodex_core::context::ContextItem;
use grodex_core::policy::PolicyDecision;
use grodex_core::state::SessionState;
use grodex_permission::PermissionManager;
use grodex_prompt::manifest::InstructionNode;
use grodex_provider::descriptor::WireProtocol;
use grodex_sampler::StreamFragment;
use grodex_skills::SkillCatalog;

/// Model configuration for Turn creation.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub wire_protocol: WireProtocol,
    /// Model's maximum context window in tokens. Used by the compaction
    /// manager to decide when to trigger. Default: 1_048_576 (1M) to
    /// match modern models. Override via config `context_window`.
    pub context_window: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            model: "gpt-5".into(),
            wire_protocol: WireProtocol::Responses,
            context_window: 1_048_576,
        }
    }
}

/// Infer a model's context window (in tokens) from its name.
///
/// Built-in table for common models; substring matching, most-specific
/// pattern first. Unknown models fall back to 1M — callers can always
/// override via the `context_window` config key.
pub fn infer_context_window(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    // (substring, window) — checked in order, first match wins.
    const TABLE: &[(&str, u64)] = &[
        // OpenAI
        ("gpt-3.5", 16_385),
        ("gpt-4-turbo", 128_000),
        ("gpt-4o", 128_000),
        ("gpt-4.1", 1_047_576),
        ("gpt-4", 8_192),
        ("gpt-5", 256_000),
        ("o1", 200_000),
        ("o3", 200_000),
        ("o4-mini", 200_000),
        // Anthropic (all current models expose 200K)
        ("claude", 200_000),
        // DeepSeek
        ("deepseek", 128_000),
        // Qwen
        ("qwen-turbo", 1_000_000),
        ("qwen", 128_000),
        // Gemini
        ("gemini-1.5-pro", 2_000_000),
        ("gemini-1.5-flash", 1_000_000),
        ("gemini", 1_048_576),
        // GLM / Kimi
        ("glm", 128_000),
        ("moonshot", 128_000),
        ("kimi", 128_000),
    ];
    TABLE
        .iter()
        .find(|(pat, _)| m.contains(pat))
        .map(|(_, w)| *w)
        .unwrap_or(1_048_576)
}
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

// ── BackgroundTaskBarrier (invariant #11) ─────────────────────────────
//
// Invariant #11: "后台任务完成 ≠ 主 Agent 已读取" — background task
// completion does not imply the main agent has consumed the result.
//
// The barrier tracks two monotonic counters:
//   - completed_epoch: bumped each time a background task finishes
//   - consumed_epoch: bumped when the main agent explicitly acknowledges
//
// The invariant is: consumed_epoch <= completed_epoch at all times.
// A debug_assert fires if the main agent tries to consume a result
// that hasn't been completed yet (consumed_epoch would exceed
// completed_epoch).

/// Fence enforcing invariant #11: background task completion is
/// distinct from main-agent consumption.
///
/// Usage:
///   1. Background task finishes → `notify_completed()`
///   2. Main agent reads the result → `consume()` (debug_asserts)
///   3. `consumed_epoch` must never exceed `completed_epoch`
#[derive(Debug)]
pub struct BackgroundTaskBarrier {
    /// Number of background tasks that have completed.
    completed_epoch: u64,
    /// Number of completions the main agent has acknowledged.
    consumed_epoch: u64,
}

impl BackgroundTaskBarrier {
    /// Create a new barrier at epoch 0.
    pub fn new() -> Self {
        Self {
            completed_epoch: 0,
            consumed_epoch: 0,
        }
    }

    /// Signal that a background task has completed.
    /// Bumps the completed epoch.
    pub fn notify_completed(&mut self) {
        self.completed_epoch = self.completed_epoch.saturating_add(1);
    }

    /// The main agent acknowledges (consumes) one completed background
    /// task result.
    ///
    /// # Panics (debug_assert)
    /// Panics in debug builds if `consumed_epoch >= completed_epoch`,
    /// which would mean the agent is reading a result that hasn't been
    /// completed yet (invariant #11 violation).
    pub fn consume(&mut self) {
        debug_assert!(
            self.consumed_epoch < self.completed_epoch,
            "invariant #11 violated: consumed_epoch ({}) >= completed_epoch ({}) \
             — main agent read a background result before it completed",
            self.consumed_epoch,
            self.completed_epoch,
        );
        if self.consumed_epoch < self.completed_epoch {
            self.consumed_epoch += 1;
        }
    }

    /// How many completions are pending (completed but not yet consumed).
    pub fn pending_count(&self) -> u64 {
        self.completed_epoch.saturating_sub(self.consumed_epoch)
    }

    /// The current completed epoch.
    pub fn completed_epoch(&self) -> u64 {
        self.completed_epoch
    }

    /// The current consumed epoch.
    pub fn consumed_epoch(&self) -> u64 {
        self.consumed_epoch
    }

    /// Reset both counters (e.g. on session boundary).
    pub fn reset(&mut self) {
        self.completed_epoch = 0;
        self.consumed_epoch = 0;
    }
}

impl Default for BackgroundTaskBarrier {
    fn default() -> Self {
        Self::new()
    }
}

/// Turn completion message from a spawned turn task.
struct TurnCompletion {
    turn_id: grodex_core::id::TurnId,
    outcome: TurnOutcome,
    /// The prompt index at which this turn started (for rewind).
    #[allow(dead_code)]
    prompt_index: usize,
}

/// The session control plane.
pub struct SessionSupervisor {
    session: Session,
    chat_state: ChatStateHandle,
    #[allow(dead_code)]
    coordinator: TurnCoordinator,
    cmd_rx: mpsc::Receiver<SessionCommand>,
    event_tx: mpsc::Sender<SessionEvent>,
    completion_rx: mpsc::UnboundedReceiver<TurnCompletion>,
    completion_tx: mpsc::UnboundedSender<TurnCompletion>,
    /// Single shared journal writer — the coordinator writes through the
    /// same instance, so user input / turn-complete / tool results all
    /// share one monotonic seq stream.
    writer: Option<RolloutWriter>,
    /// Context items restored from a prior session during `resume`. Written
    /// to the new session's journal as a `ContextRestored` event at startup
    /// so a second crash does not lose the recovered history.
    recovered_context: Option<Vec<grodex_core::context::ContextItem>>,
    /// SessionStarted/ContextRestored 是否已写入 journal（惰性：首轮对话才写，
    /// 空会话不在磁盘留任何 journal 内容）。
    session_start_written: bool,
    /// recovered_context 尚未落盘（随首个 turn 的 SessionStarted 一起写）。
    recovered_context_pending: bool,
    /// Model configuration for Turn creation.
    model_config: ModelConfig,
    /// Optional memory database for RAG context injection (SQLite + FTS5).
    memory: Option<Arc<grodex_memory::MemoryDatabase>>,
    /// Optional embedding model for hybrid RAG (FTS5 + vector). If None,
    /// `retrieve_hybrid_memory` degrades to pure FTS5 (fail-open).
    embedding: Option<Arc<dyn grodex_memory::EmbeddingModel + Send + Sync>>,
    /// Working directory for instruction discovery (AGENTS.md / .agent/rules).
    cwd: PathBuf,
    /// Whether the workspace is trusted (controls whether untrusted
    /// AGENTS.md content is included in the prompt — fail-closed).
    workspace_trusted: bool,
    /// Discovery configuration for instruction discovery (Doc 19 §7.3
    /// compat vendor opt-in). Default: no compat vendors scanned.
    discovery_config: grodex_prompt::DiscoveryConfig,
    /// Handle to the currently running turn task (for cancellation).
    current_turn_handle: Option<tokio::task::AbortHandle>,
    /// CancellationToken for the current turn.
    current_turn_cancel: Option<CancellationToken>,
    /// Shared permission manager — the SAME instance the TurnCoordinator
    /// holds (extracted via `permission_handle()` at construction). The
    /// supervisor calls `resolve()` on it when a `ResolveApproval`
    /// command arrives, completing the approval round-trip the broker
    /// started when `check()` returned `Ask` (Design Doc 16 §10, second
    /// half). Without this handle the tool future parked on
    /// `decision_rx` would time out and the approval would be a no-op.
    permission: Arc<Mutex<PermissionManager>>,
    /// Cached SkillCatalog — discovered once from project + user paths,
    /// reused across turns so we don't walk the filesystem every prompt.
    /// P1-2: without this cache, `with_skills()` was never called at all,
    /// so skills were invisible to the model.
    skill_catalog: Option<SkillCatalog>,
    /// Previous Turn's skill snapshot for change detection (Design Doc 08 §6).
    /// On the next Turn, skills are re-discovered and hashes compared.
    /// If any changed, `skill_generation` is bumped so the model sees
    /// the updated content.
    prev_skill_snapshot: Option<Vec<grodex_skills::SkillSnapshot>>,
    /// Monotonic skill generation counter — bumped when skill content
    /// changes are detected between Turns.
    skill_generation: u64,
    /// Cached instruction discovery nodes (AGENTS.md / .agent/rules).
    /// Discovered once per session, reused across turns — the filesystem
    /// walk is the expensive part; `build()` is just string assembly.
    cached_discovered_nodes: Option<Vec<InstructionNode>>,
    /// Invariant #11 fence: tracks background task completions vs main
    /// agent consumption. Ensures the agent never reads a result that
    /// hasn't been fully produced yet.
    background_barrier: BackgroundTaskBarrier,
    /// W4 evidence extractor: two-tier (LLM first, rule-based fallback).
    /// When `None`, memory extraction is skipped entirely (e.g. when the
    /// memory database itself is disabled).
    memory_extractor: Option<Arc<dyn grodex_memory::EvidenceExtractor + Send + Sync>>,
}

impl SessionSupervisor {
    pub fn new(
        session: Session,
        chat_state: ChatStateHandle,
        mut coordinator: TurnCoordinator,
        writer: Option<RolloutWriter>,
        recovered_context: Option<Vec<grodex_core::context::ContextItem>>,
        memory: Option<Arc<grodex_memory::MemoryDatabase>>,
        embedding: Option<Arc<dyn grodex_memory::EmbeddingModel + Send + Sync>>,
        memory_extractor: Option<Arc<dyn grodex_memory::EvidenceExtractor + Send + Sync>>,
        model_config: ModelConfig,
        cwd: PathBuf,
        workspace_trusted: bool,
    ) -> (Self, SessionHandle) {
        // Attach the shared writer to the coordinator if one was created
        // in the builder. This is the single chokepoint that guarantees a
        // coherent seq stream across both layers.
        let writer = match writer {
            Some(w) => {
                coordinator = coordinator.with_rollout(w.clone());
                Some(w)
            }
            None => None,
        };

        // Extract the shared permission handle BEFORE the coordinator is
        // moved into Self. This is the SAME Arc<Mutex<PermissionManager>>
        // the TurnCoordinator dispatches tool calls through, so a
        // `resolve()` here wakes the exact tool future parked on the
        // ticket's oneshot receiver.
        let permission = coordinator.permission_handle();

        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let (event_tx, event_rx) = mpsc::channel(64);
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();

        let supervisor = Self {
            session,
            chat_state,
            coordinator,
            cmd_rx,
            event_tx,
            completion_rx,
            completion_tx,
            writer,
            recovered_context,
            session_start_written: false,
            recovered_context_pending: false,
            memory,
            embedding,
            model_config,
            cwd,
            workspace_trusted,
            discovery_config: grodex_prompt::DiscoveryConfig::default(),
            current_turn_handle: None,
            current_turn_cancel: None,
            permission,
            skill_catalog: None,
            prev_skill_snapshot: None,
            skill_generation: 1,
            cached_discovered_nodes: None,
            background_barrier: BackgroundTaskBarrier::new(),
            memory_extractor,
        };

        let handle = SessionHandle { cmd_tx, event_rx };
        (supervisor, handle)
    }

    /// Override the instruction discovery configuration (Doc 19 §7.3
    /// compat vendor opt-in). Call before `run()`.
    pub fn set_discovery_config(&mut self, cfg: grodex_prompt::DiscoveryConfig) {
        self.discovery_config = cfg;
    }

    /// Main event loop with tokio::select!.
    ///
    /// Following Grok's pattern: completions and commands are multiplexed.
    /// Turn execution runs in spawned tasks so the loop stays responsive.
    #[tracing::instrument(
        level = "info",
        skip(self),
        fields(session_id = %self.session.id)
    )]
    pub async fn run(&mut self) {
        tracing::info!("session supervisor starting");
        if let Err(e) = self.session.transition_to(SessionState::Idle) {
            let _ = self.event_tx.send(SessionEvent::Error { message: e }).await;
            return;
        }

        // ── Crash recovery: replay the journal into the live transcript ──
        // On startup, if a rollout exists for this session we fold it back
        // into the ChatStateActor so a restarted session continues from
        // where it crashed — the journal is the single source of truth
        // (invariant #13), not the in-memory transcript. The reducer also
        // validates seq continuity, orphaned tool calls and generation
        // monotonicity; any violation surfaces as an error event.
        if let Some(ref writer) = self.writer {
            match self.recover_from_journal(writer).await {
                Ok(Some(seq_count)) => {
                    // Continue the writer's seq after replay so new events
                    // don't collide with replayed ones.
                    writer.resume_from(seq_count);
                    // Informational, not an Error — a successful recovery is
                    // the expected resume path, not a failure.
                    let _ = self.event_tx
                        .send(SessionEvent::Info {
                            message: format!("已从 journal 恢复会话（{seq_count} 条事件）"),
                        })
                        .await;
                }
                Ok(None) => {} // fresh session, nothing to replay
                Err(e) => {
                    let _ = self.event_tx
                        .send(SessionEvent::Error {
                            message: format!("journal recovery failed: {e}"),
                        })
                        .await;
                }
            }
        }

        // ── Persist recovered context from a prior session (resume) ──────
        // When `resume` injects a rebuilt transcript into a fresh session,
        // those items live only in memory. Write them to the new session's
        // journal as a `ContextRestored` event so a second crash does not
        // lose the recovered history. This is a no-op for fresh sessions
        // (recovered_context is None) and for sessions that already have a
        // journal (the items were already replayed above).
        if let Some(items) = &self.recovered_context {
            if !items.is_empty() {
                // Inject into the live chat state so the first sampling step
                // sees the restored transcript. The journal write is DEFERRED
                // to the first turn (empty-session hygiene: a session with no
                // conversation leaves nothing on disk).
                self.chat_state.replace_conversation(items.clone(), false).await;
                self.recovered_context_pending = true;
            }
        }

        // Approval-ticket expiry sweeper: the coordinator's 120s wait
        // timeout Denies the *tool future*, but only this sweep marks the
        // broker/DB ticket Expired (and journals the resolution) so
        // recovered/re-injected tickets cannot linger forever.
        let mut expiry_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        expiry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                // Turn completions (higher priority — process before new commands).
                Some(completion) = self.completion_rx.recv() => {
                    self.handle_turn_completion(completion).await;
                }
                _ = expiry_tick.tick() => {
                    let expired = self.permission.lock().await.expired_ticket_infos();
                    if !expired.is_empty() {
                        for (ticket_id, info) in &expired {
                            if let Some(ref writer) = self.writer {
                                let _ = writer
                                    .write_approval_resolved(ticket_id, info.call_id.as_deref(), "expired", None, None)
                                    .await;
                            }
                            let _ = self.event_tx
                                .send(SessionEvent::Info {
                                    message: format!(
                                        "审批超时：{}（ticket={ticket_id}）",
                                        info.tool_name.as_deref().unwrap_or("?")
                                    ),
                                })
                                .await;
                        }
                        self.permission.lock().await.expire_timed_out();
                    }
                }
                // Commands from the frontend.
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(SessionCommand::Shutdown) | None => {
                            self.shutdown().await;
                            break;
                        }
                        Some(cmd) => {
                            if !self.handle_command(cmd).await {
                                break;
                            }
                        }
                    }
                }
            }
        }

        self.session.state = SessionState::Closed;
        let _ = self.event_tx.send(SessionEvent::Shutdown).await;
    }

    #[tracing::instrument(
        level = "debug",
        skip(self, cmd),
        fields(session_id = %self.session.id)
    )]
    async fn handle_command(&mut self, cmd: SessionCommand) -> bool {
        match cmd {
            SessionCommand::StartTurn { user_input } => {
                self.start_turn(user_input).await;
                true
            }
            SessionCommand::Steer { user_input } => {
                // Steer: cancel current turn, start new one with modified goal.
                self.cancel_turn().await;
                self.start_turn(user_input).await;
                true
            }
            SessionCommand::AdoptPermissionPolicy { policy } => {
                // Config hot-reload: adopt the recompiled policy. The
                // revocation epoch bump fences every in-flight lease —
                // a mid-Turn tool call revalidates against the NEW
                // policy before its side effect (invariant #16).
                self.permission.lock().await.adopt_policy(policy);
                let epoch = self.permission.lock().await.revocation_epoch();
                let _ = self
                    .event_tx
                    .send(SessionEvent::Info {
                        message: format!("权限策略已热更新（revocation epoch → {epoch}）"),
                    })
                    .await;
                true
            }
            SessionCommand::CancelTurn => {
                self.cancel_turn().await;
                true
            }
            SessionCommand::ResolveApproval { ticket_id, decision, narrowed_args, always_allow } => {
                // Design Doc 16 §10 (second half) + Doc 17 §9 — the
                // frontend resolved an approval ticket. Forward the
                // decision to the SAME PermissionManager the
                // TurnCoordinator dispatched the tool through: a
                // successful `resolve()` completes the oneshot the
                // broker stored when `check()` returned `Ask`, which
                // wakes the parked tool future. The coordinator's
                // `execute_single_tool` is then free to mint its
                // PermissionLease and run.
                //
                // P0-4 FIX: narrowed_args must NOT be silently dropped.
                // We do three things with it:
                //   1. Pass `narrowed_args` into permission.resolve so
                //      the broker overwrites the ticket's stored
                //      arguments snapshot (persisted in SQLite).
                //   2. Write an ApprovalResolved rollout event with the
                //      narrowed_args so replay sees the same resolution
                //      the live session saw.
                //   3. Write an EffectiveToolCallRevisionCreated event
                //      so the tool future / coordinator can look up the
                //      narrowed args when building the actual invocation.
                let accepted = matches!(decision, PolicyDecision::Allow);
                let resolution_str = match (&decision, narrowed_args.is_some(), always_allow) {
                    (PolicyDecision::Deny, _, _) => "rejected",
                    (PolicyDecision::Ask, _, _)  => "rejected", // Ask should never appear in resolve()
                    (PolicyDecision::Allow, true, _) => "narrowed",
                    (PolicyDecision::Allow, false, true) => "approved_session",
                    (PolicyDecision::Allow, false, false) => "approved",
                };

                // Look up the pending ticket's associated tool_call_id /
                // tool_name now so we can annotate both rollout events.
                // The in-memory ticket is about to be removed by
                // resolve() below, so snapshot first.
                let pending = self.permission.lock().await
                    .pending_ticket_info(&ticket_id);
                let call_id = pending.as_ref().and_then(|p| p.call_id.as_deref());
                let tool_name = pending.as_ref().and_then(|p| p.tool_name.as_deref());
                let pending_args = pending.as_ref().and_then(|p| p.args.clone());

                // (2) Persist ApprovalResolved to journal BEFORE
                // calling resolve() — if we crash between the broker
                // accept and the journal write, resume would see a
                // missing resolution and re-prompt the user.
                if let Some(ref writer) = self.writer {
                    let write_res = writer
                        .write_approval_resolved(
                            &ticket_id,
                            call_id,
                            resolution_str,
                            None, // resolved_by: UI sets later for audit
                            narrowed_args.as_ref(),
                        )
                        .await;
                    if let Err(e) = write_res {
                        eprintln!("[warn] rollout write_approval_resolved failed: {e}");
                    }
                }

                // (3) EffectiveToolCallRevisionCreated — durable record
                // that the tool invocation going forward uses the
                // narrowed args, NOT the original model-issued ones.
                // The revision number increments on each narrow.
                if let (Some(writer), Some(cid), Some(na)) =
                    (&self.writer, call_id, &narrowed_args)
                {
                    let write_res = writer
                        .write_effective_tool_call_revision(
                            cid,
                            tool_name,
                            1u64, // revision 1 = first narrow
                            na,
                        )
                        .await;
                    if let Err(e) = write_res {
                        eprintln!("[warn] rollout write_effective_tool_call_revision failed: {e}");
                    }
                }

                // "Always allow": mint a session-level grant keyed to the
                // PRECISE call shape (doc 10 §20.12 / doc 16 §15) —
                // tool + path / command_prefix / host constraints — so
                // approving one narrow command does not grant the whole
                // tool for the session.
                if always_allow && accepted {
                    if let Some(tool_name) = tool_name {
                        let matcher = grodex_permission::compiler::always_allow_matcher_for(
                            tool_name,
                            pending_args.as_ref().unwrap_or(&serde_json::Value::Null),
                        );
                        let generation = self.permission.lock().await.revocation_epoch();
                        let grant = grodex_permission::session_grant::SessionPolicyGrant {
                            grant_id: format!("grant_{ticket_id}"),
                            origin_approval_id: ticket_id.clone(),
                            subject_id: "user".into(),
                            capability_id: tool_name.to_string(),
                            normalized_operation_matcher: matcher.clone(),
                            normalized_resource_or_command_matcher: None,
                            ceiling_hash: format!("policy_gen:{generation}"),
                            policy_generation_created: generation,
                            created_at: chrono::Utc::now(),
                            expires_at: None, // session lifetime
                            max_uses: None,
                            revoked_at: None,
                        };
                        // Durable (doc 16 §15): the grant survives a restart
                        // via journal replay.
                        if let Some(ref writer) = self.writer {
                            let _ = writer
                                .write_session_grant_created(
                                    grant.grant_id.as_str(),
                                    tool_name,
                                    matcher.as_str(),
                                    generation,
                                )
                                .await;
                        }
                        self.permission.lock().await.add_session_grant(grant);
                        tracing::info!(ticket_id = %ticket_id, tool = %tool_name, matcher = %matcher, "session grant minted (always allow)");
                    }
                }

                // (1) Finally tell the broker to apply the decision +
                // update arguments_snapshot in SQLite.
                let resolved = self.permission.lock().await.resolve(&ticket_id, decision, narrowed_args);
                let _ = self.event_tx
                    .send(SessionEvent::ApprovalResolved {
                        ticket_id,
                        accepted: resolved && accepted,
                    })
                    .await;
                true
            }
            SessionCommand::ResumeSession { last_seq, idempotency_key: _, emit_snapshot_to_frontend } => {
                // Design Doc 17 §10 — client reconnected and tells us the last
                // seq it processed. Rebuild a snapshot from the journal, emit
                // it so the UI can resync without replaying every event, AND
                // inject the reduced context into Session.context so future
                // turns carry history (fallback for non-ACP resume paths).
                //
                // When `emit_snapshot_to_frontend = false` we *only* perform
                // the context-inject work and suppress the SnapshotReady
                // broadcast — used by ACP main.rs which already shipped a
                // Snapshot frame to the TUI on its own (otherwise the
                // frontend sees a second, empty, snapshot that clobbers the
                // already-rendered history).
                self.handle_resume_session(last_seq, emit_snapshot_to_frontend).await;
                true
            }
            SessionCommand::RestoreContext { items, persist } => {
                // Restore full conversation context so future turns carry
                // the resumed history. NOTE: there are TWO context
                // projections we must sync:
                //   1. self.session.context  — used by the Session state
                //      machine for admit_turn / turn bookkeeping.
                //   2. self.chat_state conversation — used by
                //      TurnCoordinator to build CanonicalModelRequest for
                //      the model. (This is what PromptBuilder/StepRunner
                //      actually read. Missing this write caused "resume
                //      still thinks this is a brand-new chat".)
                if !items.is_empty() {
                    // Merge strategy: keep only bootstraps (System/Developer)
                    // above restored chat; otherwise replace entirely.
                    let has_bootstrap_only = self.session.context.iter().all(|c| {
                        matches!(c, ContextItem::System { .. } | ContextItem::Developer { .. })
                    });
                    let merged = if has_bootstrap_only {
                        let mut m = self.session.context.clone();
                        m.extend(items.clone());
                        m
                    } else {
                        items.clone()
                    };
                    self.session.context = merged.clone();
                    // ── Critical: replace chat_state conversation ──
                    // TurnCoordinator pulls `context_items` from chat_state,
                    // NOT from self.session.context. If we skip this line
                    // the model receives zero prior turns on every new
                    // prompt after resume.
                    self.chat_state
                        .replace_conversation(merged.clone(), false)
                        .await;
                    // Persist ContextRestored to the new session's journal
                    // so subsequent resumes of the new session id see the
                    // recovered state without re-cross-reading the old id.
                    // ONLY for boot-restore into a fresh session (`persist`):
                    // same-journal `/resume` already has every item on disk,
                    // and re-writing the full context per resume snowballed
                    // journals (436 MB of duplicate ContextRestored seen).
                    if persist {
                        if let Some(w) = &self.writer {
                            if let Err(e) = w.write_context_restored(&items).await {
                                let _ = self
                                    .event_tx
                                    .send(SessionEvent::Error {
                                        message: format!(
                                            "resume: persist ContextRestored failed: {e}"
                                        ),
                                    })
                                    .await;
                            }
                        }
                    }
                    // Internal diagnostic — use tracing, NOT SessionEvent::Info
                    // which would leak implementation details to the user.
                    tracing::info!(
                        items = items.len(),
                        "[resume] restored history items"
                    );
                }
                true
            }
            SessionCommand::RebindRolloutWriter { new_store, new_session_id, next_seq } => {
                // Swap the attached rollout store + session id + reseed the
                // monotonic seq counter so every subsequent journal commit
                // appends to the RESUMED session's durable file instead of
                // the ephemeral boot-new-session empty directory. All
                // writer clones (supervisor, coordinator, durable
                // sub-agent) see the same swap because the inner state is
                // behind a single shared `Arc<RwLock<Inner>>`.
                //
                // NOTE: ordering matters in main.rs — this command MUST be
                // sent BEFORE RestoreContext so the ContextRestored event
                // is persisted to the correct (old) journal.
                if let Some(w) = &self.writer {
                    w.rebind(new_store, new_session_id, next_seq);
                    // Also align the Session struct id so downstream
                    // readers (chat state, diagnostics) report the
                    // resumed id rather than the transient new one.
                    self.session.id = new_session_id;
                    // Lazy SessionStarted: the rebound (resumed) journal may
                    // already contain one from the previous process. Scan and
                    // arm the flag accordingly — the FIRST TURN writes the
                    // anchor if the journal lacks it.
                    let has_start = w
                        .store()
                        .replay_from(0)
                        .await
                        .map(|evs| {
                            evs.iter().any(|e| {
                                matches!(e.event_type, grodex_rollout::event::RolloutEventType::SessionStarted)
                            })
                        })
                        .unwrap_or(false);
                    self.session_start_written = has_start;
                    self.recovered_context_pending = false;
                }
                // Internal diagnostic — use tracing, NOT SessionEvent::Info
                // which would leak session_id/next_seq to the user.
                tracing::info!(
                    session_id = %new_session_id,
                    next_seq,
                    "[resume] rollout_writer rebound"
                );
                true
            }
            SessionCommand::Shutdown => false,
            SessionCommand::ResolveIndeterminate { call_id, resolution, content } => {
                // Human adjudication for an Indeterminate tool call.
                // Write the durable ToolOutcomeResolved event so the
                // journal records the user's decision for future resumes.
                //
                // R14-4: Look up operation_id from the journal so the
                // resolved event can be correlated with the original
                // Prepared/Started events for idempotency on replay.
                let op_id = if let Some(ref writer) = self.writer {
                    if let Ok(events) = writer.store().replay_from(0).await {
                        events.iter()
                            .find(|ev| {
                                ev.payload.get("call_id").and_then(|v| v.as_str()) == Some(&call_id)
                                    && ev.event_type == grodex_rollout::event::RolloutEventType::ToolExecutionStarted
                            })
                            .and_then(|ev| ev.payload.get("operation_id").and_then(|v| v.as_str()))
                            .map(|s| s.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                };
                match resolution {
                    crate::command::IndeterminateResolution::Succeeded => {
                        if let Some(ref writer) = self.writer {
                            let _ = writer
                                .write_tool_outcome_resolved(
                                    &call_id,
                                    op_id.as_deref(),
                                    "succeeded",
                                    content.as_deref(),
                                    Some("user"),
                                )
                                .await;
                        }
                        let _ = self.event_tx
                            .send(SessionEvent::Info {
                                message: format!(
                                    "Indeterminate call {call_id} resolved as Succeeded",
                                ),
                            })
                            .await;
                    }
                    crate::command::IndeterminateResolution::Failed => {
                        if let Some(ref writer) = self.writer {
                            let _ = writer
                                .write_tool_outcome_resolved(
                                    &call_id,
                                    op_id.as_deref(),
                                    "failed",
                                    content.as_deref(),
                                    Some("user"),
                                )
                                .await;
                        }
                        let _ = self.event_tx
                            .send(SessionEvent::Info {
                                message: format!(
                                    "Indeterminate call {call_id} resolved as Failed",
                                ),
                            })
                            .await;
                    }
                    crate::command::IndeterminateResolution::Retry => {
                        // No ToolOutcomeResolved — the model will re-issue
                        // the call in a future Turn. Just inform the frontend.
                        let _ = self.event_tx
                            .send(SessionEvent::Info {
                                message: format!(
                                    "Indeterminate call {call_id} discarded — model will retry",
                                ),
                            })
                            .await;
                    }
                }
                true
            }
        }
    }

    /// Build a session snapshot from the journal and optionally emit
    /// `SnapshotReady` to the frontend.
    ///
    /// The snapshot covers the full session state (not just the delta from
    /// `last_seq`) — a delta-only replay would require the client to stitch
    /// events, which is fragile after a disconnect. The full snapshot is
    /// simpler and correct; incremental deltas can be added later if the
    /// snapshot grows too large.
    ///
    /// `emit_snapshot_to_frontend` controls whether the SnapshotReady event
    /// is pushed onto `event_tx`. Set this to `false` when the caller has
    /// already delivered a snapshot to the frontend on its own (e.g. ACP
    /// main.rs does so for cross-session-id resumes) — otherwise the
    /// frontend receives a second, typically empty, snapshot that
    /// clobbers the already-rendered chat history.
    ///
    /// Regardless of this flag, the method **always** writes the reduced
    /// context items back into `self.session.context + chat_state` so
    /// subsequent `start_turn()` passes take the resumed history into
    /// prompt building.
    async fn handle_resume_session(&mut self, last_seq: u64, emit_snapshot_to_frontend: bool) {
        let writer = match &self.writer {
            Some(w) => w,
            None => {
                let _ = self.event_tx
                    .send(SessionEvent::Error {
                        message: "cannot resume: no rollout store attached".into(),
                    })
                    .await;
                return;
            }
        };

        // Lean replay when the store is file-backed: skips materializing
        // redundant multi-MB ContextRestored payloads (legacy journals
        // snowballed to hundreds of MB of duplicated snapshots), which
        // was the dominant cost of "first resume is very slow".
        // Non-file stores fall back to the regular replay.
        let events = match writer.store().journal_path() {
            Some(path) => match crate::reducer::replay_journal_lean(&path, &self.session.id) {
                Ok((evts, _last_seq, _ctx)) => evts,
                Err(e) => {
                    let _ = self.event_tx
                        .send(SessionEvent::Error {
                            message: format!("resume: read journal: {e}"),
                        })
                        .await;
                    return;
                }
            },
            None => match writer.store().replay_from(0).await {
                Ok(evts) => evts,
                Err(e) => {
                    let _ = self.event_tx
                        .send(SessionEvent::Error {
                            message: format!("resume: read journal: {e}"),
                        })
                        .await;
                    return;
                }
            },
        };

        if events.is_empty() {
            // Even when there are 0 events, honor `emit_snapshot_to_frontend`.
            // Without this guard, ACP /resume triggers TWO snapshots:
            //   1. main.rs's hand-built ServerFrame::Snapshot (items=9, correct)
            //   2. this early-return empty SnapshotReady  →  items=0 clobber
            if emit_snapshot_to_frontend {
                let _ = self.event_tx
                    .send(SessionEvent::SnapshotReady {
                        last_seq: 0,
                        generation: 0,
                        current_turn_id: None,
                        items_json: "[]".into(),
                    })
                    .await;
            }
            return;
        }

        let mut reducer = SessionReducer::new(self.session.id).tolerant_orphans();
        if let Err(e) = reducer.apply_all(&events) {
            let _ = self.event_tx
                .send(SessionEvent::Error {
                    message: format!("resume: replay: {e}"),
                })
                .await;
            return;
        }

        let generation = reducer.generation().as_u64();
        let context = reducer.finish();
        let event_count = events.len() as u64;

        // ── Inject context into Session + chat_state ────────────────────
        // Same merge rule as SessionCommand::RestoreContext: keep existing
        // System/Developer bootstrap context above restored chat, otherwise
        // replace. This fallback fires on non-ACP resume paths where the
        // caller could not send RestoreContext.
        if !context.is_empty() {
            let has_bootstrap_only = self.session.context.iter().all(|c| {
                matches!(c, ContextItem::System { .. } | ContextItem::Developer { .. })
            });
            let merged = if has_bootstrap_only {
                let mut m = self.session.context.clone();
                m.extend(context.clone());
                m
            } else {
                context.clone()
            };
            self.session.context = merged.clone();
            // Critical double-write: TurnCoordinator pulls context_items
            // for the model from chat_state, NOT from self.session.context.
            // Without this call resume + "what did we just talk about?"
            // still gets the "brand new chat" response.
            self.chat_state
                .replace_conversation(merged.clone(), false)
                .await;
            // Also persist via write_context_restored so subsequent
            // re-resumes of the *new* session id immediately see the
            // recovered context on their next resume (rather than
            // requiring yet-another cross-id read).
            //
            // Only when WE are the replay source of truth
            // (`emit_snapshot_to_frontend = true`, self-resume fallback).
            // ACP same-journal resume sends emit=false AND follows up with
            // RestoreContext — persisting here too would write the whole
            // context back into the journal we literally just replayed
            // (the snowball that grew journals by a full copy per resume).
            if emit_snapshot_to_frontend {
                if let Some(w) = &self.writer {
                    let _ = w.write_context_restored(&context).await;
                }
            }
        }

        // Build snapshot items from the context — each ContextItem becomes
        // a SnapshotItem the UI can render. Contents are soft-capped so a
        // mid-sized session never grows a Snapshot JSON past the ACP
        // transport's 16MB line limit (which would cause the entire frame
        // to be dropped by the TUI's fail-closed size guard).
        const CAP_USER: usize = 4000;
        const CAP_ASSISTANT: usize = 8000;
        const CAP_REASONING: usize = 2000;
        const CAP_TOOL_CALL: usize = 4000;
        const CAP_TOOL_RESULT: usize = 4000;
        const CAP_DEFAULT: usize = 2000;
        fn truncate_for_snapshot(s: &str, max: usize) -> String {
            let count = s.chars().count();
            if count <= max {
                return s.to_string();
            }
            let head: String = s.chars().take(max).collect();
            format!("{head}\n…[{count} chars total, truncated for snapshot]")
        }

        let snapshot_items: Vec<serde_json::Value> = context
            .iter()
            .map(|item| {
                let (item_type, content) = match item {
                    ContextItem::User { content, .. } => {
                        ("user", truncate_for_snapshot(content, CAP_USER))
                    }
                    ContextItem::Assistant { content, .. } => {
                        ("assistant", truncate_for_snapshot(content, CAP_ASSISTANT))
                    }
                    ContextItem::ToolResult { content, .. } => {
                        ("tool_result", truncate_for_snapshot(content, CAP_TOOL_RESULT))
                    }
                    ContextItem::System { content, .. } => {
                        ("system", truncate_for_snapshot(content, CAP_DEFAULT))
                    }
                    ContextItem::Developer { content, .. } => {
                        ("developer", truncate_for_snapshot(content, CAP_DEFAULT))
                    }
                    ContextItem::ToolCall { name, arguments, .. } => {
                        let joined = format!("{name}: {arguments}");
                        ("tool_call", truncate_for_snapshot(&joined, CAP_TOOL_CALL))
                    }
                    ContextItem::CompactionSummary { summary, .. } => {
                        ("compaction", truncate_for_snapshot(summary, CAP_DEFAULT))
                    }
                    ContextItem::ReasoningSummary { content, .. } => {
                        ("reasoning", truncate_for_snapshot(content, CAP_REASONING))
                    }
                    ContextItem::ImagePlaceholder { mime_type, artifact_ref } => {
                        ("image", format!("{mime_type}:{artifact_ref}"))
                    }
                };
                serde_json::json!({
                    "item_type": item_type,
                    "content": content,
                    "complete": true,
                })
            })
            .collect();

        let items_json = serde_json::to_string(&snapshot_items).unwrap_or_else(|_| "[]".into());

        if emit_snapshot_to_frontend {
            let _ = self.event_tx
                .send(SessionEvent::SnapshotReady {
                    last_seq: event_count.max(last_seq),
                    generation,
                    current_turn_id: None,
                    items_json,
                })
                .await;
        }

        // ── Indeterminate tool call detection ───────────────────────
        // After replay, check for tool calls that started but never
        // finished (ToolExecutionStarted without matching
        // ToolExecutionFinished/ToolResultCommitted). These represent
        // side effects in an unknown state.
        //
        // R14-6b: consult SideEffectClass to avoid blocking resume behind
        // a human decision for trivially-replayable tools. ReadOnly and
        // Idempotent tools are classified as NotStarted (safe to
        // auto-replay); only NonIdempotent / unknown tools become
        // Indeterminate (human must adjudicate).
        let side_effect_map = {
            let cap_handle = self.coordinator.capability_handle();
            let cap = cap_handle.lock().await;
            cap.side_effect_map()
        };
        let checkpoint = grodex_rollout::recovery::recover_from_journal_with_metadata(
            &events,
            &side_effect_map,
        );
        for (call_id, fate) in &checkpoint.call_fate {
            if matches!(fate, grodex_rollout::recovery::ToolCallFate::Indeterminate) {
                // Look up tool name and operation_id from the journal events.
                let started_event = events
                    .iter()
                    .find(|ev| {
                        ev.payload.get("call_id").and_then(|v| v.as_str()) == Some(call_id.as_str())
                            && ev.event_type == grodex_rollout::event::RolloutEventType::ToolExecutionStarted
                    });
                let tool_name = started_event
                    .and_then(|ev| ev.payload.get("name").and_then(|v| v.as_str()))
                    .unwrap_or("unknown")
                    .to_string();
                let op_id = started_event
                    .and_then(|ev| ev.payload.get("operation_id").and_then(|v| v.as_str()))
                    .map(|s| s.to_string());
                // Write the durable indeterminate marker so future
                // resumes don't re-report the same call.
                if let Some(w) = &self.writer {
                    let _ = w
                        .write_tool_outcome_indeterminate(
                            call_id,
                            op_id.as_deref(),
                            &tool_name,
                            "crash_recovery: started without finished",
                        )
                        .await;
                }
                // Surface to frontend so the user can adjudicate.
                let _ = self.event_tx
                    .send(SessionEvent::IndeterminateToolCall {
                        call_id: call_id.clone(),
                        tool_name: tool_name.clone(),
                        message: format!(
                            "Tool '{}' (call_id={}) was executing when the session crashed. \
                             The side effect state is unknown — inspect the real-world result \
                             and resolve as Succeeded, Failed, or Retry.",
                            tool_name, call_id,
                        ),
                    })
                    .await;
            }
        }

        // ── Pending approval restoration ────────────────────────────
        // Re-surface approval tickets that were requested but never
        // resolved before the crash. The user needs to adjudicate them
        // so the corresponding tool calls can proceed (or be denied).
        //
        // R14-2 fix: Previously these tickets were only re-surfaced to the
        // frontend as events, but NOT re-injected into the ApprovalBroker
        // memory table. This meant `ResolveApproval` → `permission.resolve()`
        // would return false (ticket not found in broker), and the user's
        // decision was silently dropped. Now we re-inject each pending
        // ticket into the broker with a fresh oneshot channel, so resolve()
        // can find it and the decision is properly recorded.
        for ticket in checkpoint.pending_approval_tickets() {
            // Re-inject into the broker so ResolveApproval can find it.
            self.permission.lock().await.reinject_pending_ticket(
                &ticket.ticket_id,
                &ticket.tool_name,
                ticket.call_id.as_deref(),
                ticket.args.as_ref(),
            );
            let _ = self.event_tx
                .send(SessionEvent::ApprovalRequested {
                    ticket_id: ticket.ticket_id.clone(),
                    tool_name: ticket.tool_name.clone(),
                    summary: format!(
                        "[recovered-pending] Tool '{}' approval was requested \
                         before the session disconnected. Please resolve.",
                        ticket.tool_name,
                    ),
                    risk: "recovered".into(),
                    timeout_remaining_ms: 120_000,
                    args: ticket.args.clone(),
                    call_id: ticket.call_id.clone(),
                })
                .await;
        }
    }

    #[tracing::instrument(
        level = "info",
        skip(self),
        fields(session_id = %self.session.id)
    )]
    async fn start_turn(&mut self, user_input: String) {
        // Invariant #1/#2: at most one Turn is admitted at a time. The
        // session state machine rejects a second admit while one is running,
        // and the supervisor never holds an existing turn handle here.
        debug_assert!(
            self.current_turn_handle.is_none(),
            "invariant #1: a new Turn started while another was still running"
        );

        // Admit turn in session state machine.
        let turn_id = match self.session.admit_turn(user_input.clone()) {
            Ok(id) => id,
            Err(e) => {
                let _ = self.event_tx.send(SessionEvent::Error { message: e }).await;
                return;
            }
        };

        // Capture user input for memory query (before move).
        let user_input_for_memory = user_input.clone();

        // Push user message to transcript.
        let user_item = ContextItem::User {
            content: user_input,
            message_id: None,
        };
        self.chat_state.push_user_message(user_item).await;

        let _ = self.event_tx.send(SessionEvent::TurnStarted { turn_id }).await;

        // Lazy session anchors (empty-session hygiene): SessionStarted +
        // ContextRestored are written on the FIRST turn — a session that
        // never had a conversation leaves nothing in its journal.
        if !self.session_start_written {
            if let Some(ref writer) = self.writer {
                let details = serde_json::json!({
                    "cwd": self.cwd.to_string_lossy(),
                    "model_provider": self.model_config.provider,
                    "model": self.model_config.model,
                });
                let _ = writer.write_session_started(&details).await;
                if self.recovered_context_pending {
                    if let Some(items) = &self.recovered_context {
                        if let Err(e) = writer.write_context_restored(items).await {
                            tracing::warn!("failed to persist recovered context: {e}");
                        }
                    }
                }
                self.session_start_written = true;
            }
        }

        // Write rollout event.
        if let Some(ref writer) = self.writer {
            if let Err(e) = writer.write_user_input(turn_id, &user_input_for_memory).await {
                let _ = self.event_tx
                    .send(SessionEvent::Error {
                        message: format!("rollout journal write failed (UserInputAccepted): {e}"),
                    })
                    .await;
            }
            // Telemetry anchor: durable TurnStarted (turns.started_at).
            let _ = writer
                .write_turn_started(turn_id, user_input_for_memory.chars().count())
                .await;
        }

        // Build system instructions via PromptBuilder + Memory + Discovery.
        //
        // Skill management (progressive disclosure, like Tool registration):
        //   - SkillCatalog is discovered ONCE at session start (in `new()`)
        //   - Per-Turn: just reuse the registered catalog (no disk re-scan)
        //   - Only skill metadata (name + description + hash) is in memory
        //   - Content is loaded on-demand when the model reads the file
        //   - Snapshot is written to journal once for version auditing
        if self.skill_catalog.is_none() {
            let catalog = SkillCatalog::discover(&self.cwd, self.workspace_trusted);
            let snapshot = catalog.snapshot();
            if let Some(ref writer) = self.writer {
                let skills_json: Vec<serde_json::Value> = snapshot.iter().map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "source": format!("{:?}", s.source),
                        "path": s.path.to_string_lossy(),
                        "content_hash": s.content_hash,
                        "trusted": s.trusted,
                    })
                }).collect();
                let _ = writer
                    .write_skill_snapshot(turn_id, &skills_json, self.skill_generation)
                    .await;
            }
            self.prev_skill_snapshot = Some(snapshot);
            self.skill_catalog = Some(catalog);
        }
        if self.cached_discovered_nodes.is_none() {
            let mut tmp = grodex_prompt::PromptBuilder::new()
                .with_discovery_config(self.discovery_config.clone());
            tmp.discover_instructions(&self.cwd, self.workspace_trusted);
            // Doc 19 §7.3 diagnostics (compat precedence / unknown
            // vendors) are explanatory — surface via tracing only.
            for d in tmp.discovery_diagnostics() {
                tracing::info!("instruction discovery diagnostic: {d}");
            }
            // Doc 19 §12: structural conflict detection (boundary
            // violations / scope overrides / duplicates) — explanatory,
            // surfaces via tracing; also embedded in the manifest built
            // below for `prompt explain` visibility.
            let conflict_report = grodex_prompt::detect_conflicts(tmp.discovered_nodes());
            for c in &conflict_report.conflicts {
                tracing::info!("instruction conflict: {}", c.message);
            }
            self.cached_discovered_nodes = Some(tmp.discovered_nodes().to_vec());
        }

        let static_memory = std::env::var("HOME")
            .map(|h| grodex_memory::StaticContextLoader::load(
                std::path::Path::new(&h), &self.cwd,
            ))
            .unwrap_or_default()
            .content;
        let mut builder = grodex_prompt::PromptBuilder::new()
            .with_skills(self.skill_catalog.clone().unwrap_or_default())
            .with_discovered_nodes(
                self.cached_discovered_nodes.clone().unwrap_or_default(),
            )
            .with_static_context(static_memory);
        // Memory RAG results are VOLATILE (depend on the current user
        // input). They must NOT be baked into the system prompt — that
        // would change the request prefix every turn and defeat provider
        // prompt caching. Instead they travel as a trailing Developer
        // instruction block, which the sampler emits AFTER the stable
        // system prompt (see client.rs) so the cached prefix survives.
        //
        // P1-bugfix: this used to be a single-pipeline call into
        // `db.retrieve_hybrid_memory` (Memory only). SkillRetriever and
        // EvidenceRetriever were dead-code (only wired inside the offline
        // eval harness). We now run the full V2 3-way choreography:
        //   IntentRouter → [SkillRetriever; MemoryRetriever; EvidenceRetriever]
        //   → capacity cap → merged block injection.
        // This makes the live turn surface exactly the same retrieval
        // graph that the eval harness exercises, so offline quality
        // metrics actually correlate with live behaviour.
        let mut memory_block: Option<String> = None;
        if let Some(ref db) = self.memory {
            use grodex_memory::{
                IntentRouter, RetrievalConfig, RetrievedUnit, ResultSource,
                retrievers::{RetrievalDiagnostics, SkillRetriever},
            };

            let decision = IntentRouter::route(&user_input_for_memory);
            let cfg = RetrievalConfig::default();

            // ── 3 concurrent retrieval legs ────────────────────────────
            // Leg 1: SkillRetriever (FTS-only, pure sync + blocking)
            let skill_db = db.clone();
            let query_skill = user_input_for_memory.clone();
            let cfg_skill = cfg.clone();
            let skill_handle = if decision.skill_enabled {
                Some(tokio::task::spawn_blocking(move || {
                    let (res, diag) =
                        SkillRetriever::new((*skill_db).clone(), cfg_skill).retrieve(&query_skill);
                    (res, diag)
                }))
            } else {
                None
            };

            // Leg 2: Memory (FTS + Vector RRF) — `retrieve_hybrid_memory`
            // already does: access counter bump, provenance summary, top-K
            // truncation, fail-open to pure FTS when embedding is None.
            let mem_top_k = cfg.max_results.min(cfg.memory_quota + cfg.preference_quota);
            let mem_query = user_input_for_memory.clone();
            let db_mem = db.clone();
            let emb_mem = self.embedding.clone();
            let mem_handle = if decision.memory_enabled {
                Some(tokio::spawn(async move {
                    db_mem
                        .retrieve_hybrid_memory(&mem_query, mem_top_k, emb_mem.as_ref())
                        .await
                        .unwrap_or_default()
                }))
            } else {
                None
            };

            // Leg 3: Evidence — BREAKPOINT-1 FIX. Previously
            // `retrieve_hybrid_evidence` had ZERO production callers;
            // now it's live in exactly the same wiring as Memory, so
            // evidence units actually surface in the prompt on the next
            // turn that happens to query for them.
            let ev_top_k = cfg.max_results.min(cfg.evidence_quota.max(3));
            let ev_query = user_input_for_memory.clone();
            let db_ev = db.clone();
            let emb_ev = self.embedding.clone();
            let inc_sup = decision.include_superseded;
            let ev_handle = if decision.evidence_enabled {
                Some(tokio::spawn(async move {
                    db_ev
                        .retrieve_hybrid_evidence(&ev_query, ev_top_k, inc_sup, emb_ev.as_ref())
                        .await
                        .unwrap_or_default()
                }))
            } else {
                None
            };

            let retrieval_started = std::time::Instant::now();

            // Join all three legs — each join arm returns empty on a
            // failed handle (never kill the turn because of memory).
            let (skill_out, diagnostics_skill): (
                Vec<grodex_memory::RetrievalResult>,
                Option<RetrievalDiagnostics>,
            ) = match skill_handle {
                Some(h) => match h.await {
                    Ok((r, d)) => (r, Some(d)),
                    Err(_) => (Vec::new(), None),
                },
                None => (Vec::new(), None),
            };
            let memory_units: Vec<RetrievedUnit> = match mem_handle {
                Some(h) => h.await.unwrap_or_default(),
                None => Vec::new(),
            };
            let evidence_units: Vec<RetrievedUnit> = match ev_handle {
                Some(h) => h.await.unwrap_or_default(),
                None => Vec::new(),
            };
            let mut diagnostics: Vec<RetrievalDiagnostics> =
                diagnostics_skill.into_iter().collect();

            // Skill pipeline returns RetrievalResult; convert to RetrievedUnit.
            let skill_units: Vec<RetrievedUnit> = skill_out
                .iter()
                .map(|r| RetrievedUnit {
                    unit_id: r.unit_id.clone(),
                    path: r.path.clone(),
                    content: r.content.clone(),
                    source: ResultSource::Skill,
                    unit_kind: "skill".into(),
                    section: r.section.clone(),
                    updated_at: None,
                    rollout_id: None,
                    superseded_by: None,
                    provenance: Vec::new(),
                })
                .collect();

            // ── Enforce global Memory + Evidence cap ──────────────────
            // Same semantics as retrieve_all: trim Evidence first, then
            // Memory if still over.
            let cfg_cap = RetrievalConfig::default().max_results;
            let (mut memory_final, mut evidence_final) = (memory_units, evidence_units);
            {
                let mut total = memory_final.len() + evidence_final.len();
                if total > cfg_cap {
                    let excess = total - cfg_cap;
                    let from_ev = excess.min(evidence_final.len());
                    evidence_final.truncate(evidence_final.len() - from_ev);
                    total = memory_final.len() + evidence_final.len();
                    let remaining = total.saturating_sub(cfg_cap);
                    if remaining > 0 {
                        memory_final.truncate(memory_final.len() - remaining);
                    }
                }
            }

            // P3 telemetry.
            if let Some(ref writer) = self.writer {
                writer.emit_out_of_band_telemetry(
                    grodex_telemetry::kind::MEMORY_RETRIEVAL,
                    Some(turn_id),
                    None,
                    &serde_json::json!({
                        "query_chars": user_input_for_memory.chars().count(),
                        "memory_count": memory_final.len(),
                        "evidence_count": evidence_final.len(),
                        "skill_count": skill_units.len(),
                        "router_memory": decision.memory_enabled,
                        "router_evidence": decision.evidence_enabled,
                        "router_skill": decision.skill_enabled,
                        "duration_ms": retrieval_started.elapsed().as_millis() as u64,
                        "router_kind": "3way_hybrid_rrf",
                        "vector_enabled": self.embedding.is_some(),
                    }),
                );
            }

            let total = skill_units.len() + memory_final.len() + evidence_final.len();
            if total > 0 {
                let mut formatted = String::new();
                if !skill_units.is_empty() {
                    formatted.push_str("## Recommended Skills\n\n");
                    formatted.push_str(
                        "These SKILL modules matched the user's intent. Prefer them over \
                         ad-hoc tooling when a match applies — the entry path points at the \
                         workflow doc.\n\n"
                    );
                    formatted.push_str(&RetrievedUnit::format_for_prompt(&skill_units));
                    formatted.push('\n');
                }
                // Provenance-rich memory block.
                formatted.push_str(&RetrievedUnit::format_for_prompt(&memory_final));
                if !evidence_final.is_empty() {
                    if memory_final.is_empty() {
                        formatted.push_str(
                            "## Relevant Evidence from Past Sessions\n\n\
                             The entries below are HISTORICAL EVIDENCE (past tool results, \
                             assistant turn summaries). They are context only — do NOT \
                             treat them as stable facts; promote them to MEMORY only when \
                             they recur across multiple sessions.\n\n"
                        );
                    }
                    formatted.push_str(&RetrievedUnit::format_for_prompt(&evidence_final));
                }
                if !diagnostics.is_empty() {
                    let empty_retrievals = diagnostics
                        .iter()
                        .filter(|d| d.qualified_count == 0 && d.returned_count == 0)
                        .count();
                    if empty_retrievals > 0 {
                        formatted.push_str(&format!(
                            "\n_Router diagnostics: {} empty-result pipelines, {} total legs_\n",
                            empty_retrievals,
                            diagnostics.len()
                        ));
                    }
                }
                if !decision.reason_codes.is_empty() {
                    formatted.push_str(&format!(
                        "\n_Router reason codes: {}_\n",
                        decision.reason_codes.join(", ")
                    ));
                }
                if let Some(reason) = &decision.hard_skip_reason {
                    formatted.push_str(&format!(
                        "_Router hard-skip: {}_\n",
                        reason
                    ));
                }
                memory_block = Some(formatted);
            }
        }

        let manifest = builder.build();
        // The manifest already assembled all instruction nodes in four-zone
        // order (A → C → B → D) into `content`. We pass it as a single
        // system instruction block — the zone ordering is preserved.
        let mut instructions = vec![grodex_provider::canonical_request::InstructionBlock {
            role: grodex_provider::canonical_request::InstructionRole::System,
            content: manifest.content.clone(),
            priority: 0,
        }];
        if let Some(mem_text) = memory_block {
            instructions.push(grodex_provider::canonical_request::InstructionBlock {
                role: grodex_provider::canonical_request::InstructionRole::Developer,
                content: mem_text,
                priority: 1,
            });
        }

        // Spawn turn as a tokio task.
        let turn_ctx = TurnContext::with_model(
            self.session.id, turn_id, instructions,
            &self.model_config.provider, &self.model_config.model, self.model_config.wire_protocol,
        );
        let completion_tx = self.completion_tx.clone();
        let cancel_token = CancellationToken::new();
        let cancel_token_child = cancel_token.clone();

        // Spawn turn execution with streaming channel.
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let coordinator = self.coordinator.clone();
        let event_tx = self.event_tx.clone();
        let handle = tokio::spawn(async move {
            // Forward ALL streaming fragments (not just text) to the
            // frontend wire. This forwarding loop is intentionally
            // 1-to-1: every StreamFragment becomes exactly one
            // SessionEvent with the same payload, so the CLI/TUI can
            // render reasoning + tool cards + tool results incrementally.
            let stream_handle = tokio::spawn(async move {
                while let Some(frag) = stream_rx.recv().await {
                    let ev = match frag {
                        StreamFragment::Text(t) => SessionEvent::TextDelta { text: t },
                        StreamFragment::Reasoning(t) => SessionEvent::ReasoningDelta { text: t },
                        StreamFragment::ToolCallStart { call_id, name } => {
                            SessionEvent::ToolCallStart { call_id, name }
                        }
                        StreamFragment::ToolCallArgs { call_id, args_delta } => {
                            SessionEvent::ToolCallArgs { call_id, args_delta }
                        }
                        StreamFragment::ToolCallEnd { call_id } => SessionEvent::ToolCallEnd { call_id },
                        StreamFragment::ToolResult { call_id, content, is_error } => {
                            SessionEvent::ToolResult { call_id, content, is_error }
                        }
                        StreamFragment::ApprovalRequested {
                            ticket_id,
                            tool_name,
                            summary,
                            risk,
                            timeout_remaining_ms,
                            args,
                        } => SessionEvent::ApprovalRequested {
                            ticket_id,
                            tool_name,
                            summary,
                            risk,
                            timeout_remaining_ms,
                            args: args,
                            call_id: None,
                        },
                        StreamFragment::CompactionStatus { phase } => {
                            SessionEvent::CompactionStatus { phase }
                        }
                    };
                    let _ = event_tx.send(ev).await;
                }
            });
            let outcome = coordinator.run(turn_ctx, cancel_token_child, Some(stream_tx)).await;
            stream_handle.abort();
            let _ = completion_tx.send(TurnCompletion { turn_id, outcome, prompt_index: 0 });
        });

        self.current_turn_handle = Some(handle.abort_handle());
        self.current_turn_cancel = Some(cancel_token);
    }

    async fn handle_turn_completion(&mut self, completion: TurnCompletion) {
        self.current_turn_handle = None;
        self.current_turn_cancel = None;

        let text = completion.outcome.final_text.clone();
        if !text.is_empty() {
            let _ = self.event_tx.send(SessionEvent::StepCompleted {
                turn_id: completion.turn_id,
                text: text.clone(),
            }).await;
        } else {
            // Check for sampling errors first.
            let mut found_error = false;
            for step in &completion.outcome.steps {
                if let Some(ref err) = step.error {
                    let _ = self.event_tx.send(SessionEvent::Error { message: format!("{err}") }).await;
                    found_error = true;
                }
            }
            // ── Issue #4 fix: output-break-after-tool-failure ──────
            // When the model produced no text AND no sampling error was
            // recorded, the likely cause is that tool(s) failed and the
            // model either returned empty text or never got to respond.
            // Without a fallback message the TUI shows "Done (with errors)"
            // with no explanation — the user sees the output "break".
            //
            // Surface a summary of what happened so the user always sees
            // *something* informative after a turn, even when the model
            // went silent.
            if !found_error {
                let mut tools_called_count = 0usize;
                for step in &completion.outcome.steps {
                    tools_called_count += step.tool_calls.len();
                }
                // If tools were called but no text came back, this is
                // almost always a tool-failure scenario. Surface a
                // helpful summary so the user isn't left wondering.
                if tools_called_count > 0 {
                    let summary = format!(
                        "本轮调用了 {} 个工具但未生成回复。可能有工具执行失败，请检查上方的工具结果卡片。",
                        tools_called_count,
                    );
                    let _ = self.event_tx.send(SessionEvent::Info { message: summary }).await;
                } else if completion.outcome.steps.is_empty() {
                    let _ = self.event_tx.send(SessionEvent::Info {
                        message: "本轮未产生任何输出。".into(),
                    }).await;
                }
            }
        }

        // ── Step-budget exhaustion notice ─────────────────────
        // The coordinator forces a wrap-up summary when max_steps is
        // hit; make that VISIBLE to the user so a long task ending
        // early is never mistaken for a completed one.
        // NOTE: this must be OUTSIDE the if/else above — when the
        // wrap-up summary produces text, final_text is non-empty and
        // the if-branch runs; the steps_exhausted notice would be
        // silently skipped if placed inside the else branch.
        if completion.outcome.steps_exhausted {
            let _ = self.event_tx.send(SessionEvent::Info {
                message: format!(
                    "⚠️ 已达到本轮最大执行步数（{} 步），上方已自动生成进展总结。发一条消息（如“继续”）即可接着完成未完成的工作。",
                    completion.outcome.steps.len().saturating_sub(1)
                ),
            }).await;
        }

        // Always complete the turn (even if text is empty) so the session
        // transitions back to Idle and the next admit_turn() succeeds.
        // If complete_turn fails (e.g. no current turn), fall back to
        // cancel_turn to guarantee the session is unblocked.
        if let Err(e) = self.session.complete_turn(&text) {
            let _ = self.session.cancel_turn();
            let _ = self.event_tx.send(SessionEvent::Error { message: e }).await;
        }

        // Aggregate token usage across all steps so the frontend can
        // show the prompt-cache hit rate for this turn.
        let (input_tokens, cached_tokens) = completion
            .outcome
            .steps
            .iter()
            .filter_map(|s| s.usage.as_ref())
            .fold((0u64, 0u64), |(inp, cac), u| {
                (inp + u.input_tokens, cac + u.cached_input_tokens)
            });
        let _ = self
            .event_tx
            .send(SessionEvent::TurnCompleted {
                turn_id: completion.turn_id,
                input_tokens,
                cached_tokens,
            })
            .await;

        // Write TurnCompleted rollout event with the structured
        // termination reason + aggregate counters (telemetry projection).
        if let Some(ref writer) = self.writer {
            let metrics_json = serde_json::to_value(completion.outcome.metrics)
                .unwrap_or(serde_json::Value::Null);
            if let Err(e) = writer
                .write_turn_completed_with(
                    completion.turn_id,
                    completion.outcome.termination_reason,
                    &metrics_json,
                )
                .await
            {
                let _ = self.event_tx
                    .send(SessionEvent::Error {
                        message: format!("rollout journal write failed (TurnCompleted): {e}"),
                    })
                    .await;
            }
        }

        // ── W4 memory extraction + proposal commit (post-turn) ──────
        //
        // Fail-open everywhere:
        //   * No memory DB → skip.
        //   * No extractor configured → skip.
        //   * Conversation scan yields no usable turn slice → skip.
        //   * Extractor returns Err → CompositeExtractor already fell back
        //     to rule tier; a second Err is rare and we log it silently.
        //   * propose_and_commit rejects claims → they're captured in the
        //     report for diagnostics, not propagated.
        //
        // Spawned as a detached task so extraction never blocks the next
        // user turn being admitted. This matches the design note: W2/W3
        // run on a background schedule; W4 follows the same pattern with
        // per-turn triggering.
        // NOTE: bind owned values to locals BEFORE the if-let to avoid
        // borrowing into temporaries (cloned tuples drop immediately).
        let memory_db = self.memory.clone();
        let memory_extractor_opt = self.memory_extractor.clone();
        if let (Some(db), Some(extractor)) = (memory_db, memory_extractor_opt) {
            let chat_state = self.chat_state.clone();
            let session_id = self.session.id.to_string();
            // Prefer the rollout writer's session id (bound to the on-disk
            // journal) as the rollout provenance key. Fall back to the
            // supervisor-level session id when the writer isn't attached
            // (this only happens in tests / no-disk configs).
            let rollout_id = self
                .writer
                .as_ref()
                .map(|w| w.session_id().to_string())
                .unwrap_or(session_id);
            let turn_id_str = completion.turn_id.to_string();
            let outcome = completion.outcome.clone();
            let event_tx = self.event_tx.clone();

            tokio::spawn(async move {
                // 1) Slice conversation to the last turn's window.
                let conversation = chat_state.get_conversation().await;
                let Some(ctx) = assemble_extraction_context(
                    &conversation,
                    &rollout_id,
                    &turn_id_str,
                    &outcome,
                ) else {
                    return;
                };

                // 2) Run extractor.
                let result = match extractor.extract(&ctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            turn_id = %turn_id_str,
                            "memory extraction failed (all tiers exhausted)"
                        );
                        return;
                    }
                };

                if result.claims.is_empty() {
                    return;
                }

                // 3) propose_and_commit on a blocking pool (SQLite CRUD).
                let extractor_model = extractor_label(extractor.as_ref());
                let db_cloned = db.clone();
                // Make clones BEFORE the spawn_blocking `move` closure so
                // the outer task can still reuse `turn_id_str` below.
                let turn_id_for_blocking = turn_id_str.clone();
                let turn_id_for_join = turn_id_str.clone();
                let committed_ids: Vec<String> = match tokio::task::spawn_blocking(move || {
                    let turn_id_str = turn_id_for_blocking;
                    let report = grodex_memory::propose_and_commit(&db_cloned, &result, &extractor_model);
                    if report.proposed > 0 || report.committed > 0 || !report.rejected.is_empty() {
                        tracing::debug!(
                            proposed = report.proposed,
                            committed = report.committed,
                            rejected = report.rejected.len(),
                            turn_id = %turn_id_str,
                            "memory proposal report"
                        );
                    }
                    if !report.rejected.is_empty() {
                        for rej in &report.rejected {
                            tracing::debug!(
                                reason = %rej.reason,
                                fact = %truncate_for_log(&rej.fact, 120),
                                turn_id = %turn_id_str,
                                "memory proposal rejected"
                            );
                        }
                    }
                    report.committed_ids
                })
                .await
                {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            turn_id = %turn_id_for_join,
                            "propose_and_commit background task panicked"
                        );
                        return;
                    }
                };

                // 4) Surface a non-intrusive info banner only when we
                // actually wrote a new explicit preference/fact — users
                // expect "remember my name" to visibly succeed.
                if !committed_ids.is_empty() {
                    let summary = format!(
                        "🧠 已记住 {} 条长期记忆。",
                        committed_ids.len()
                    );
                    let _ = event_tx.send(SessionEvent::Info { message: summary }).await;
                }
            });
        }
    }
}

// ───────────────────────── Private W4 helpers ─────────────────────

/// Walk the conversation transcript and carve out the slice
/// corresponding to the **most recent user turn** (the last
/// `ContextItem::User` → EOF). This gives us exactly the turn window
/// the extractor is allowed to see.
///
/// Returns `None` when there is no User tail (shouldn't happen for a
/// TurnCompleted event, but guards against empty transcripts).
fn assemble_extraction_context(
    conversation: &[grodex_core::context::ContextItem],
    rollout_id: &str,
    turn_id: &str,
    outcome: &crate::step::TurnOutcome,
) -> Option<grodex_memory::ExtractionContext> {
    use grodex_core::context::ContextItem;
    use grodex_memory::{SourceRef, ToolCallSummary, ToolResultSummary};

    // Find the last User-item starting boundary; everything after it
    // belongs to the turn we just finished.
    let start = conversation.iter().rposition(|c| matches!(c, ContextItem::User { .. }))?;
    let tail = &conversation[start..];

    let mut user_input = String::new();
    let mut assistant_content: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCallSummary> = Vec::new();
    let mut tool_results: Vec<ToolResultSummary> = Vec::new();

    for item in tail {
        match item {
            ContextItem::User { content, .. } => {
                if !user_input.is_empty() {
                    user_input.push('\n');
                }
                user_input.push_str(content);
            }
            ContextItem::Assistant { content } => {
                if !content.trim().is_empty() {
                    assistant_content.push(content.clone());
                }
            }
            ContextItem::ToolCall { name, arguments, .. } => {
                // W4 filter: skip SubAgent / Delegate tool noise.
                if is_subagent_tool_noise(name) {
                    continue;
                }
                let args = match arguments {
                    serde_json::Value::String(s) => s.clone(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };
                tool_calls.push(ToolCallSummary {
                    name: name.clone(),
                    arguments: truncate_for_context(&args, 2000),
                });
            }
            ContextItem::ToolResult { call_id, content, is_error } => {
                let name = tool_name_for_call_id(tail, call_id).unwrap_or_else(|| "unknown".into());
                if is_subagent_tool_noise(&name) {
                    continue;
                }
                tool_results.push(ToolResultSummary {
                    name,
                    is_error: *is_error,
                    content: truncate_for_context(content, 2000),
                });
            }
            _ => {}
        }
    }

    // Include TurnOutcome.final_text if the transcript didn't (it's
    // produced by the coordinator post-tool loop so sometimes it's
    // present only there).
    if !outcome.final_text.trim().is_empty() {
        let ft = outcome.final_text.trim();
        if assistant_content.iter().all(|a| a.trim() != ft) {
            assistant_content.push(ft.to_string());
        }
    }

    let source = SourceRef {
        rollout_id: rollout_id.to_string(),
        seq_start: 0,
        seq_end: 0,
        turn_id: turn_id.to_string(),
        step_id: None,
    };

    Some(grodex_memory::ExtractionContext {
        user_input,
        assistant_content,
        tool_calls,
        tool_results,
        adjacent_events: Vec::new(),
        existing_memory: Vec::new(),
        source,
    })
}

fn is_subagent_tool_noise(name: &str) -> bool {
    const NOISE_PREFIX: &[&str] = &[
        "subagent",
        "delegate",
        "durable_subagent",
        "internal_delegate",
    ];
    let lower = name.to_lowercase();
    NOISE_PREFIX.iter().any(|p| lower.contains(p))
}

fn tool_name_for_call_id(
    tail: &[grodex_core::context::ContextItem],
    target: &grodex_core::id::ToolCallId,
) -> Option<String> {
    use grodex_core::context::ContextItem;
    for item in tail {
        if let ContextItem::ToolCall { call_id, name, .. } = item {
            if call_id == target {
                return Some(name.clone());
            }
        }
    }
    None
}

fn truncate_for_context(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let taken: String = s.chars().take(max_chars).collect();
        format!("{taken}…")
    }
}

fn truncate_for_log(s: &str, max_chars: usize) -> String {
    truncate_for_context(s, max_chars).replace('\n', " ")
}

/// Best-effort label written to `memory_units.extractor_model` so
/// future governance passes can tell which tier produced a claim.
fn extractor_label(e: &dyn grodex_memory::EvidenceExtractor) -> String {
    e.tier_label().to_string()
}

impl SessionSupervisor {
    /// Replay the rollout journal and rebuild the live transcript.
    ///
    /// Returns `Ok(Some(event_count))` if there were events to replay (the
    /// writer should then `resume_from` that count to continue the seq),
    /// `Ok(None)` for a fresh session with an empty journal.
    async fn recover_from_journal(&self, writer: &RolloutWriter) -> Result<Option<u64>, String> {
        let events = writer
            .store()
            .replay_from(0)
            .await
            .map_err(|e| format!("read journal: {e}"))?;
        if events.is_empty() {
            return Ok(None);
        }

        // Fold events through the reducer — this validates seq continuity,
        // rejects orphaned tool results and generation regressions, and
        // produces the rebuilt context. Tolerant mode: an interrupted
        // turn leaves orphaned tool calls behind; heal them instead of
        // making boot recovery fail on a salvageable journal.
        let mut reducer = SessionReducer::new(self.session.id).tolerant_orphans();
        reducer
            .apply_all(&events)
            .map_err(|e| format!("replay: {e}"))?;

        let rebuilt = reducer.finish();
        if !rebuilt.is_empty() {
            self.chat_state.replace_conversation(rebuilt, false).await;
        }

        // Doc 16 §15: session grants survive a restart — re-mint every
        // SessionGrantCreated found in the journal (add_session_grant is
        // idempotent by grant_id).
        for ev in events.iter().filter(|e| matches!(e.event_type, RolloutEventType::SessionGrantCreated)) {
            let (Some(gid), Some(tool), Some(matcher)) = (
                ev.payload.get("grant_id").and_then(|v| v.as_str()),
                ev.payload.get("tool_name").and_then(|v| v.as_str()),
                ev.payload.get("matcher").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let generation = ev
                .payload
                .get("policy_generation")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let grant = grodex_permission::session_grant::SessionPolicyGrant {
                grant_id: gid.to_string(),
                origin_approval_id: gid.to_string(),
                subject_id: "user".into(),
                capability_id: tool.to_string(),
                normalized_operation_matcher: matcher.to_string(),
                normalized_resource_or_command_matcher: None,
                ceiling_hash: format!("policy_gen:{generation}"),
                policy_generation_created: generation,
                created_at: chrono::Utc::now(),
                expires_at: None,
                max_uses: None,
                revoked_at: None,
            };
            self.permission.lock().await.add_session_grant(grant);
        }
        tracing::debug!("journal replay: session grants re-minted");

        Ok(Some(events.len() as u64))
    }

    async fn cancel_turn(&mut self) {
        let had_turn = self.current_turn_handle.is_some();
        // Capture turn_id BEFORE session.cancel_turn() clears current_turn.
        let turn_id = self.session.current_turn.as_ref().map(|t| t.id);
        if let Some(handle) = self.current_turn_handle.take() {
            if let Some(token) = self.current_turn_cancel.take() {
                token.cancel();
            }
            handle.abort();
        }
        // Invariant #8: cancellation must let cleanup run before the
        // supervisor proceeds to admit another Turn. `handle.abort()`
        // schedules cancellation; yielding once lets the runtime pump the
        // abort into the task so the Turn's in-flight tool spawns observe
        // the CancellationToken before `start_turn` re-enters. (We cannot
        // `handle.await` here — the supervisor owns the same task.)
        tokio::task::yield_now().await;
        debug_assert!(
            self.current_turn_handle.is_none() && self.current_turn_cancel.is_none(),
            "invariant #8: cancellation did not clear the turn handle"
        );
        if let Err(e) = self.session.cancel_turn() {
            let _ = self.event_tx.send(SessionEvent::Error { message: e }).await;
        }
        // Repair the journal damage caused by the abort: when the turn
        // task was killed mid-tool-execution, the ToolCall events are
        // durable but their ToolResultCommitted will never be written.
        // Without this repair every later resume fails validation with
        // OrphanedToolResult and the transcript stays unpaired forever.
        if had_turn {
            if let Some(tid) = turn_id {
                self.heal_interrupted_tool_calls(tid).await;
            }
        }
        // Notify the frontend that the turn is over so it stops the
        // streaming indicator and marks in-flight tool cards as done.
        // Without this, the TUI shows "⏳ working… 3m09s" forever because
        // the aborted turn task never reaches handle_turn_completion()
        // which normally emits TurnCompleted.
        if had_turn {
            let _ = self.event_tx.send(SessionEvent::TurnCompleted {
                turn_id: turn_id.unwrap_or_default(),
                input_tokens: 0,
                cached_tokens: 0,
            }).await;
        }
    }

    /// Post-abort journal repair: synthesize an error result (journal +
    /// live transcript) for every tool call that no longer has a paired
    /// result, then seal the cancelled turn with TurnCompleted. This
    /// keeps the journal valid under strict replay and the live
    /// transcript wire-safe (no dangling tool_calls before the next
    /// user message).
    async fn heal_interrupted_tool_calls(&mut self, turn_id: grodex_core::id::TurnId) {
        let Some(ref writer) = self.writer else { return };

        // Scan the transcript for tool calls without a result.
        let conversation = self.chat_state.get_conversation().await;
        let mut pending: Vec<(grodex_core::id::ToolCallId, String)> = Vec::new();
        for item in &conversation {
            match item {
                ContextItem::ToolCall { call_id, name, .. } => {
                    pending.push((*call_id, name.clone()));
                }
                ContextItem::ToolResult { call_id, .. } => {
                    pending.retain(|(id, _)| id != call_id);
                }
                _ => {}
            }
        }

        for (call_id, name) in pending {
            let content = format!(
                "[interrupted] The `{name}` tool call was interrupted by the user; \
                 no result was obtained. Do not assume its side effects completed \
                 — verify actual state first if needed."
            );
            if let Err(e) = writer
                .write_tool_result_interrupted(turn_id, &call_id.to_string(), &content)
                .await
            {
                tracing::warn!("heal_interrupted_tool_calls: journal write failed: {e}");
                continue;
            }
            self.chat_state
                .push_tool_result(call_id, content, true)
                .await;
        }

        // Seal the cancelled turn so strict replays see a complete turn
        // boundary (the aborted task never reached handle_turn_completion).
        if let Err(e) = writer
            .write_turn_completed_with(turn_id, "cancelled", &serde_json::json!({}))
            .await
        {
            tracing::warn!("heal_interrupted_tool_calls: TurnCompleted write failed: {e}");
        }
    }

    async fn shutdown(&mut self) {
        self.cancel_turn().await;

        // ── W2 会话退出触发 rollout → Evidence 抽取 ────────────────────
        // 在清理空目录之前执行，确保当前 session 的 journal 还在磁盘上。
        // 抽取是同步 CPU/IO 操作，用 spawn_blocking 包一层 + 等待完成；
        // 抽取失败不影响退出（fail-open，启动时全量扫还会兜底）。
        if let (Some(db), Some(writer)) = (self.memory.clone(), &self.writer) {
            let session_id = self.session.id.to_string();
            let journal_path = writer
                .store()
                .session_dir_path()
                .map(|d| d.join("rollout.jsonl"));
            if let Some(journal) = journal_path {
                let db_for_task = Arc::clone(&db);
                // spawn_blocking 内只消耗 &session_id + &journal，不能 move
                // 这两个值 —— 外层 tracing 分支里还会用到。所以单独
                // clone 进 move closure，避免 E0382。
                let sid_for_task = session_id.clone();
                let journal_for_task = journal.clone();
                match tokio::task::spawn_blocking(move || {
                    db_for_task.extract_evidence_from_session(&sid_for_task, &journal_for_task)
                })
                .await
                {
                    Ok(Ok(n)) if n > 0 => {
                        tracing::info!(
                            target: "grodex_session",
                            session_id = %session_id,
                            evidence_created = n,
                            "shutdown rollout extract completed"
                        );
                    }
                    Ok(Ok(_)) => {
                        tracing::debug!(
                            target: "grodex_session",
                            session_id = %session_id,
                            "shutdown rollout extract completed (no new evidence)"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            target: "grodex_session",
                            session_id = %session_id,
                            error = %e,
                            "shutdown rollout extract failed (ignored)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "grodex_session",
                            session_id = %session_id,
                            error = %e,
                            "shutdown rollout extract task panicked (ignored)"
                        );
                    }
                }
            }
        }

        // Empty-session cleanup: a session that never recorded a single
        // journal event (no conversation happened) leaves only scaffolding
        // on disk (empty journal + approval db) — remove the whole session
        // directory. Journals WITH events are never touched here.
        if let Some(writer) = &self.writer {
            let empty = writer
                .store()
                .replay_from(0)
                .await
                .map(|evs| evs.is_empty())
                .unwrap_or(false);
            if empty {
                if let Some(dir) = writer.store().session_dir_path() {
                    match std::fs::remove_dir_all(&dir) {
                        Ok(()) => {
                            tracing::info!(target: "grodex_session", dir = %dir.display(), "removed empty session directory");
                        }
                        Err(e) => {
                            tracing::warn!(target: "grodex_session", dir = %dir.display(), error = %e, "empty session cleanup failed (ignored)");
                        }
                    }
                }
            }
        }
        // Doc 11 §22: reclaim this session's offloaded tool-result blobs
        // (revoke owner refs + GC) instead of leaking them on disk.
        self.coordinator
            .release_session_blobs(&self.session.id.to_string())
            .await;
        let _ = self.session.transition_to(SessionState::ShuttingDown);
    }
}

/// The frontend's handle to the session.
#[derive(Debug)]
pub struct SessionHandle {
    pub cmd_tx: mpsc::Sender<SessionCommand>,
    pub event_rx: mpsc::Receiver<SessionEvent>,
}

impl SessionHandle {
    pub async fn send(&self, cmd: SessionCommand) -> Result<(), String> {
        self.cmd_tx
            .send(cmd)
            .await
            .map_err(|_| "supervisor has stopped".into())
    }

    pub async fn recv(&mut self) -> Option<SessionEvent> {
        self.event_rx.recv().await
    }
}

#[cfg(test)]
mod barrier_tests {
    use super::BackgroundTaskBarrier;

    #[test]
    fn barrier_starts_at_zero() {
        let b = BackgroundTaskBarrier::new();
        assert_eq!(b.completed_epoch(), 0);
        assert_eq!(b.consumed_epoch(), 0);
        assert_eq!(b.pending_count(), 0);
    }

    #[test]
    fn notify_completed_bumps_epoch() {
        let mut b = BackgroundTaskBarrier::new();
        b.notify_completed();
        assert_eq!(b.completed_epoch(), 1);
        assert_eq!(b.pending_count(), 1);
        b.notify_completed();
        assert_eq!(b.completed_epoch(), 2);
        assert_eq!(b.pending_count(), 2);
    }

    #[test]
    fn consume_decrements_pending() {
        let mut b = BackgroundTaskBarrier::new();
        b.notify_completed();
        b.notify_completed();
        assert_eq!(b.pending_count(), 2);

        b.consume();
        assert_eq!(b.consumed_epoch(), 1);
        assert_eq!(b.pending_count(), 1);

        b.consume();
        assert_eq!(b.consumed_epoch(), 2);
        assert_eq!(b.pending_count(), 0);
    }

    #[test]
    fn consume_does_not_exceed_completed() {
        let mut b = BackgroundTaskBarrier::new();
        b.notify_completed();
        b.consume();
        // After consuming the only completion, pending_count is 0.
        // A second consume would violate the invariant (debug_assert),
        // so we verify the safe state instead.
        assert_eq!(b.consumed_epoch(), 1);
        assert_eq!(b.completed_epoch(), 1);
        assert_eq!(b.pending_count(), 0);
    }

    #[test]
    fn reset_clears_both_epochs() {
        let mut b = BackgroundTaskBarrier::new();
        b.notify_completed();
        b.notify_completed();
        b.consume();
        assert_eq!(b.completed_epoch(), 2);
        assert_eq!(b.consumed_epoch(), 1);

        b.reset();
        assert_eq!(b.completed_epoch(), 0);
        assert_eq!(b.consumed_epoch(), 0);
    }
}
