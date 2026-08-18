//! SessionSupervisor — tokio::select! event loop with spawned turns.
//!
//! Following Grok's `run_session` pattern: the actor loop multiplexes
//! multiple event sources (commands, turn completions, timers, rollout)
//! while turn execution runs in spawned tasks with AbortHandle.

use crate::chat_state::ChatStateHandle;
use crate::command::{SessionCommand, SessionEvent};
use crate::reducer::SessionReducer;
use crate::rollout_writer::RolloutWriter;
use crate::session::Session;
use crate::step::TurnOutcome;
use crate::turn::TurnContext;
use crate::turn_coordinator::TurnCoordinator;
use grodex_core::context::ContextItem;
use grodex_core::policy::PolicyDecision;
use grodex_core::state::SessionState;
use grodex_permission::PermissionManager;
use grodex_provider::descriptor::WireProtocol;
use grodex_sampler::StreamFragment;

/// Model configuration for Turn creation.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    pub provider: String,
    pub model: String,
    pub wire_protocol: WireProtocol,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self { provider: "openai".into(), model: "gpt-5".into(), wire_protocol: WireProtocol::Responses }
    }
}
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

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
            memory,
            embedding,
            model_config,
            cwd,
            workspace_trusted,
            current_turn_handle: None,
            current_turn_cancel: None,
            permission,
        };

        let handle = SessionHandle { cmd_tx, event_rx };
        (supervisor, handle)
    }

    /// Main event loop with tokio::select!.
    ///
    /// Following Grok's pattern: completions and commands are multiplexed.
    /// Turn execution runs in spawned tasks so the loop stays responsive.
    pub async fn run(&mut self) {
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
                    let _ = self.event_tx
                        .send(SessionEvent::Error {
                            message: format!("recovered session from journal ({} events)", seq_count),
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
        if let (Some(writer), Some(items)) = (&self.writer, &self.recovered_context) {
            if !items.is_empty() {
                // Inject into the live chat state so the first sampling step
                // sees the restored transcript.
                self.chat_state.replace_conversation(items.clone(), false).await;
                if let Err(e) = writer.write_context_restored(items).await {
                    let _ = self.event_tx
                        .send(SessionEvent::Error {
                            message: format!("failed to persist recovered context: {e}"),
                        })
                        .await;
                }
            }
        }

        loop {
            tokio::select! {
                biased;
                // Turn completions (higher priority — process before new commands).
                Some(completion) = self.completion_rx.recv() => {
                    self.handle_turn_completion(completion).await;
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
            SessionCommand::CancelTurn => {
                self.cancel_turn().await;
                true
            }
            SessionCommand::ResolveApproval { ticket_id, decision, narrowed_args: _ } => {
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
                // `narrowed_args` is currently consumed by the live
                // revalidation path inside the tool future (the broker
                // only carries a flat PolicyDecision); full Narrow
                // support that re-runs schema/policy on revised args is
                // a later phase (Doc 16 §10 "Narrow" paragraph).
                let accepted = matches!(decision, PolicyDecision::Allow);
                let resolved = self.permission.lock().await.resolve(&ticket_id, decision);
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
            SessionCommand::RestoreContext { items } => {
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
                    let _ = self
                        .event_tx
                        .send(SessionEvent::Info {
                            message: format!(
                                "[resume] 已恢复 {} 条历史消息（session.context + chat_state 双写）",
                                items.len()
                            ),
                        })
                        .await;
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
                }
                let _ = self
                    .event_tx
                    .send(SessionEvent::Info {
                        message: format!(
                            "[resume] rollout_writer rebind → session_id={}, next_seq={}",
                            new_session_id, next_seq
                        ),
                    })
                    .await;
                true
            }
            SessionCommand::Shutdown => false,
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

        let events = match writer.store().replay_from(0).await {
            Ok(evts) => evts,
            Err(e) => {
                let _ = self.event_tx
                    .send(SessionEvent::Error {
                        message: format!("resume: read journal: {e}"),
                    })
                    .await;
                return;
            }
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

        let mut reducer = SessionReducer::new(self.session.id);
        if let Err(e) = reducer.apply_all(&events) {
            let _ = self.event_tx
                .send(SessionEvent::Error {
                    message: format!("resume: replay: {e}"),
                })
                .await;
            return;
        }

        let context = reducer.context().to_vec();
        let generation = reducer.generation().as_u64();
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
            if let Some(w) = &self.writer {
                let _ = w.write_context_restored(&context).await;
            }
        }

        // Build snapshot items from the context — each ContextItem becomes
        // a SnapshotItem the UI can render.
        let snapshot_items: Vec<serde_json::Value> = context
            .iter()
            .map(|item| {
                let (item_type, content) = match item {
                    ContextItem::User { content, .. } => ("user", content.clone()),
                    ContextItem::Assistant { content, .. } => ("assistant", content.clone()),
                    ContextItem::ToolResult { content, .. } => {
                        ("tool_result", content.clone())
                    }
                    ContextItem::System { content, .. } => ("system", content.clone()),
                    ContextItem::Developer { content, .. } => ("developer", content.clone()),
                    ContextItem::ToolCall { name, arguments, .. } => {
                        ("tool_call", format!("{name}: {arguments}"))
                    }
                    ContextItem::CompactionSummary { summary, .. } => {
                        ("compaction", summary.clone())
                    }
                    ContextItem::ReasoningSummary { content, .. } => {
                        ("reasoning", content.clone())
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
    }

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

        // Write rollout event.
        if let Some(ref writer) = self.writer {
            if let Err(e) = writer.write_user_input(turn_id, &user_input_for_memory).await {
                let _ = self.event_tx
                    .send(SessionEvent::Error {
                        message: format!("rollout journal write failed (UserInputAccepted): {e}"),
                    })
                    .await;
            }
        }

        // Build system instructions via PromptBuilder + Memory + Discovery.
        //
        // Discovery (Design Doc 19 §7) walks three layers:
        //   1. Fixed roots (~/.agent/AGENTS.md, ~/.agent/rules/*.md)
        //   2. Workspace chain (root→cwd scanning AGENTS.md + .agent/rules)
        //   3. Compatibility (.grok/.codex/.claude/.cursor)
        // Untrusted workspace content is excluded (fail-closed).
        // Without this call, AGENTS.md / .agent/rules are never injected
        // into the live prompt — the model cannot see project instructions.
        let mut builder = grodex_prompt::PromptBuilder::new();
        builder.discover_instructions(&self.cwd, self.workspace_trusted);
        if let Some(ref db) = self.memory {
            // Hybrid RRF retrieval (FTS5 + vector, fail-open to pure FTS).
            // emb=None → vector list empty → RRF degrades to pure FTS5 ranking.
            // emb=Some → embed query, search vectors, fuse with FTS5 results.
            match db.retrieve_hybrid_memory(&user_input_for_memory, 5, self.embedding.as_ref()).await {
                Ok(units) if !units.is_empty() => {
                    let mem_text = grodex_memory::RetrievedUnit::format_for_prompt(&units);
                    builder.base_instructions.push(mem_text);
                }
                Ok(_) => {} // no results — nothing to inject
                Err(e) => {
                    // Fail-open: memory unavailable must not block the turn.
                    eprintln!("[warn] memory retrieve_hybrid_memory failed: {e}");
                }
            }
        }
        let manifest = builder.build();
        // The manifest already assembled all instruction nodes in four-zone
        // order (A → C → B → D) into `content`. We pass it as a single
        // system instruction block — the zone ordering is preserved.
        let instructions = vec![grodex_provider::canonical_request::InstructionBlock {
            role: grodex_provider::canonical_request::InstructionRole::System,
            content: manifest.content.clone(),
            priority: 0,
        }];

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
                        } => SessionEvent::ApprovalRequested {
                            ticket_id,
                            tool_name,
                            summary,
                            risk,
                            timeout_remaining_ms,
                        },
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

        let text = completion.outcome.final_text;
        if !text.is_empty() {
            let _ = self.event_tx.send(SessionEvent::StepCompleted {
                turn_id: completion.turn_id,
                text: text.clone(),
            }).await;
        } else {
            // Check for errors.
            for step in &completion.outcome.steps {
                if let Some(ref err) = step.error {
                    let _ = self.event_tx.send(SessionEvent::Error { message: format!("{err}") }).await;
                }
            }
        }

        // Always complete the turn (even if text is empty) so the session
        // transitions back to Idle and the next admit_turn() succeeds.
        // If complete_turn fails (e.g. no current turn), fall back to
        // cancel_turn to guarantee the session is unblocked.
        if let Err(e) = self.session.complete_turn(&text) {
            let _ = self.session.cancel_turn();
            let _ = self.event_tx.send(SessionEvent::Error { message: e }).await;
        }

        let _ = self.event_tx.send(SessionEvent::TurnCompleted { turn_id: completion.turn_id }).await;

        // Write TurnCompleted rollout event.
        if let Some(ref writer) = self.writer {
            if let Err(e) = writer.write_turn_completed(completion.turn_id).await {
                let _ = self.event_tx
                    .send(SessionEvent::Error {
                        message: format!("rollout journal write failed (TurnCompleted): {e}"),
                    })
                    .await;
            }
        }
    }

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
        // produces the rebuilt context.
        let mut reducer = SessionReducer::new(self.session.id);
        reducer
            .apply_all(&events)
            .map_err(|e| format!("replay: {e}"))?;

        let rebuilt = reducer.into_context();
        if !rebuilt.is_empty() {
            self.chat_state.replace_conversation(rebuilt, false).await;
        }

        Ok(Some(events.len() as u64))
    }

    async fn cancel_turn(&mut self) {
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
    }

    async fn shutdown(&mut self) {
        self.cancel_turn().await;
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
