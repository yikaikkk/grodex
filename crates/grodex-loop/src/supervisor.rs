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
use grodex_core::state::SessionState;
use grodex_rollout::store::RolloutStore;
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
use std::sync::Arc;
use tokio::sync::mpsc;
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
    /// Optional memory retriever for context injection.
    memory: Option<grodex_memory::LegacyRetriever>,
    /// Handle to the currently running turn task (for cancellation).
    current_turn_handle: Option<tokio::task::AbortHandle>,
    /// CancellationToken for the current turn.
    current_turn_cancel: Option<CancellationToken>,
}

impl SessionSupervisor {
    pub fn new(
        session: Session,
        chat_state: ChatStateHandle,
        mut coordinator: TurnCoordinator,
        rollout: Option<Arc<dyn RolloutStore>>,
        recovered_context: Option<Vec<grodex_core::context::ContextItem>>,
        memory: Option<grodex_memory::LegacyRetriever>,
        model_config: ModelConfig,
    ) -> (Self, SessionHandle) {
        // If a store was supplied, wrap it in the shared writer and attach
        // that SAME writer to the coordinator. This is the single chokepoint
        // that guarantees a coherent seq stream across both layers.
        let writer = match rollout {
            Some(store) => {
                let w = RolloutWriter::new(store, session.id);
                coordinator = coordinator.with_rollout(w.clone());
                Some(w)
            }
            None => None,
        };

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
            model_config,
            current_turn_handle: None,
            current_turn_cancel: None,
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
                // Design Doc 17 §9 — the frontend resolved an approval ticket.
                // The actual ApprovalBroker lives inside the turn coordinator's
                // permission pipeline; the supervisor acknowledges the resolution
                // here. Full broker integration (threading the broker through
                // the supervisor) is a separate task — for now we emit the
                // acknowledgement event so the protocol round-trip is complete.
                let _ = self.event_tx
                    .send(SessionEvent::ApprovalResolved {
                        ticket_id,
                        accepted: true,
                    })
                    .await;
                let _ = decision; // consumed by the broker when wired
                true
            }
            SessionCommand::ResumeSession { last_seq, idempotency_key: _ } => {
                // Design Doc 17 §10 — client reconnected and tells us the last
                // seq it processed. We rebuild a snapshot from the journal and
                // emit it so the UI can resync without replaying every event.
                self.handle_resume_session(last_seq).await;
                true
            }
            SessionCommand::Shutdown => false,
        }
    }

    /// Build a session snapshot from the journal and emit `SnapshotReady`.
    ///
    /// The snapshot covers the full session state (not just the delta from
    /// `last_seq`) — a delta-only replay would require the client to stitch
    /// events, which is fragile after a disconnect. The full snapshot is
    /// simpler and correct; incremental deltas can be added later if the
    /// snapshot grows too large.
    async fn handle_resume_session(&self, last_seq: u64) {
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
            let _ = self.event_tx
                .send(SessionEvent::SnapshotReady {
                    last_seq: 0,
                    generation: 0,
                    current_turn_id: None,
                    items_json: "[]".into(),
                })
                .await;
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

        let _ = self.event_tx
            .send(SessionEvent::SnapshotReady {
                last_seq: event_count.max(last_seq),
                generation,
                current_turn_id: None,
                items_json,
            })
            .await;
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

        // Build system instructions via PromptBuilder + Memory.
        let mut builder = grodex_prompt::PromptBuilder::new();
        if let Some(ref mem) = self.memory {
            let entries = mem.query(&user_input_for_memory);
            if !entries.is_empty() {
                let mem_text = grodex_memory::LegacyRetriever::format_for_prompt(&entries);
                builder.base_instructions.push(mem_text);
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
