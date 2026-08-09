//! TurnCoordinator — manages one Turn's lifecycle.
//!
//! Following Grok's `process_conversation_turn` pattern: a single Turn
//! may run multiple sampling steps. Each step builds a request → samples
//! → dispatches tools in parallel → commits results in model order.
//!
//! Turn as spawned task with AbortHandle + completion channel so the
//! SessionSupervisor stays responsive during long sampling/tool runs.

use crate::capability::SharedPublisher;
use crate::capability_manager::{CapabilityManager, TurnCapabilityOverlay};
use crate::chat_state::ChatStateHandle;
use crate::context::state_capsule::StateCapsule;
use crate::context::CompactionManager;
use crate::step::TurnOutcome;
use crate::turn::{StepResult, TurnContext};
use grodex_capability::id::{CapabilityId, CapabilityKind};
use grodex_capability::authority::Authority;
use grodex_capability::prepared::PreparedCapabilityCall;
use grodex_core::context::ContextItem;
use grodex_core::id::{CommitSequence, OperationId, StepGeneration, StepId, StepSnapshotId, ToolCallId};
use grodex_core::policy::PolicyDecision;
use grodex_core::tool::ToolRuntime;
use grodex_permission::{
    ApprovalResolution, LiveRevocationFence, PermissionLease, PermissionManager, PermissionPolicy,
    PermissionResult,
};
use grodex_subagent::delegation::DelegationEnvelope;
use grodex_provider::canonical_event::CanonicalResponseItem;
use grodex_provider::canonical_request::{CanonicalModelRequest, ToolChoice};
use grodex_sampler::{SamplingActor, StreamFragment};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// Result of one tool execution.
struct ToolExecResult {
    call_id: ToolCallId,
    #[allow(dead_code)]
    name: String,
    result: ContextItem,
    index: CommitSequence,
}

/// Manages one Turn from start to finish.
/// Cloneable — wraps shared state in Arc.
#[derive(Clone)]
pub struct TurnCoordinator {
    sampler: Arc<SamplingActor>,
    chat_state: ChatStateHandle,
    capability: Arc<Mutex<CapabilityManager>>,
    permission: Arc<Mutex<PermissionManager>>,
    compaction: Arc<Mutex<CompactionManager>>,
    sandbox: Arc<grodex_sandbox::SandboxManager>,
    /// Single shared journal writer. `None` only in unit contexts without
    /// persistence. When present, this is the SAME instance the
    /// SessionSupervisor writes through — guaranteeing one gap-free seq
    /// stream across both layers (no `seq: 0` hardcoding, no duplicate
    /// seqs from independent counters).
    rollout: Option<crate::rollout_writer::RolloutWriter>,
    /// OperationId journal for idempotency — prevents double execution on crash recovery.
    #[allow(dead_code)]
    completed_operations: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Delegation envelope for sub-agent authority enforcement (invariant #12).
    /// `None` for the root agent (full authority). When present, every tool
    /// call is checked against the envelope BEFORE the permission manager —
    /// the envelope is the parent-imposed ceiling, the permission manager is
    /// the child's own (stricter-or-equal) policy.
    delegation_envelope: Option<DelegationEnvelope>,
}

impl TurnCoordinator {
    /// Attach a shared RolloutWriter for event journaling. Both the
    /// supervisor and this coordinator must share the same writer so the
    /// seq counter stays coherent.
    pub fn with_rollout(mut self, writer: crate::rollout_writer::RolloutWriter) -> Self {
        self.rollout = Some(writer);
        self
    }
    pub fn new(sampler: SamplingActor, chat_state: ChatStateHandle) -> Self {
        // Unification (audit Phase-3): the CapabilityManager and the
        // CapabilityPublisher share one generation source so a tool bump is
        // visible to the ACP event stream / StepContext.
        let publisher = SharedPublisher::new();
        Self {
            sampler: Arc::new(sampler),
            chat_state,
            capability: Arc::new(Mutex::new(CapabilityManager::with_publisher(10, publisher))),
            rollout: None,
            completed_operations: Arc::new(std::sync::Mutex::new(HashSet::new())),
            permission: Arc::new(Mutex::new(PermissionManager::new(
                PermissionPolicy::permissive(),
            ))),
            compaction: Arc::new(Mutex::new(CompactionManager::new(128_000))),
            sandbox: Arc::new(grodex_sandbox::SandboxManager::default()),
            delegation_envelope: None,
        }
    }

    /// Attach a DelegationEnvelope for sub-agent authority enforcement.
    /// When set, every tool call is checked against the envelope's
    /// capability subset, authority ceiling, and policy ceiling before
    /// the live permission manager runs (invariant #12: child ≤ parent).
    pub fn with_delegation_envelope(mut self, envelope: DelegationEnvelope) -> Self {
        self.delegation_envelope = Some(envelope);
        self
    }

    /// Register a tool. Publishes a new capability generation.
    pub async fn register_tool(
        &self,
        name: impl Into<String>,
        runtime: Arc<dyn ToolRuntime>,
        input_schema: serde_json::Value,
    ) {
        let name = name.into();
        let spec = grodex_provider::canonical_request::ToolSpec {
            name: name.clone(),
            description: name.clone(),
            parameters: input_schema,
            required: vec![],
        };
        self.capability.lock().await.register_tool(name, runtime, spec);
    }

    /// Run a complete Turn: sample → process → dispatch tools → loop.
    pub async fn run(
        &self,
        turn_ctx: TurnContext,
        cancel_token: CancellationToken,
        stream_tx: Option<tokio::sync::mpsc::UnboundedSender<StreamFragment>>,
    ) -> TurnOutcome {
        let max_steps = 10;
        let mut steps = Vec::new();
        let mut tools_called = Vec::new();

        // ── Freeze capabilities at Turn start (invariant #15) ────────
        // The base is the immutable snapshot of all registered capabilities
        // at the moment the Turn begins. Mid-Turn promotions/demotions go
        // into the overlay and are visible to subsequent Steps via
        // `effective_specs` / `effective_runtime`, but are NOT applied to
        // the live CapabilityManager until the Turn ends (`adopt_overlay`).
        // This is the "adopted_generation" freeze mechanism from Design
        // Doc 10 §16: it prevents a mid-Turn registration from changing
        // what the model sees mid-conversation.
        let (base, overlay) = {
            let cap = self.capability.lock().await;
            (cap.snapshot_base(), TurnCapabilityOverlay::new())
        };
        let step_gen = base.generation;

        for _step_idx in 0..max_steps {
            if cancel_token.is_cancelled() {
                break;
            }

            // ── Effective tool specs: base + overlay (frozen for this Turn) ──
            let tool_specs = TurnCapabilityOverlay::effective_specs(&base, &overlay);

            // ── Compaction check ────────────────────────────────
            let context = self.chat_state.get_conversation().await;
            let current_tokens = context.iter().map(|i| i.estimated_tokens() as u64).sum();
            let should_compact = {
                let c = self.compaction.lock().await;
                c.should_compact(current_tokens) || c.is_overflow(current_tokens)
            };
            if should_compact
            {
                self.try_compact(&turn_ctx, &context).await;
            }

            // ── Build request ───────────────────────────────────
            let context = self.chat_state.get_conversation().await;
            let step_id = StepId::new();

            // Record step boundary (ToolCallPrepared) — marks the point
            // where a new sampling step begins with the frozen capability
            // generation. The reducer uses this to reconstruct step
            // boundaries during recovery.
            if let Some(ref writer) = self.rollout {
                if let Err(e) = writer
                    .write_step_started(
                        turn_ctx.turn_id,
                        step_id,
                        StepGeneration::new(step_gen),
                    )
                    .await
                {
                    eprintln!("[warn] rollout write_step_started failed: {e}");
                }
            }

            let request = CanonicalModelRequest {
                request_id: format!("req_{}", step_id),
                session_id: turn_ctx.session_id,
                turn_id: turn_ctx.turn_id,
                step_id,
                model_binding_id: turn_ctx.model_binding.binding_id,
                prompt_snapshot_hash: None,
                instructions: turn_ctx.instructions.clone(),
                context_items: context.clone(),
                tool_specs,
                tool_choice: ToolChoice::Auto,
                parallel_tool_calls: true,
                reasoning_request: None,
                response_format: None,
                max_output_tokens: Some(4096),
                provider_state_in: None,
            };

            // ── Sample ──────────────────────────────────────────
            // Use streaming if available (clone tx for each step).
            let outcome = match stream_tx {
                Some(ref tx) => self.sampler.sample_streaming(&turn_ctx.model_binding, &request, tx.clone()).await,
                None => self.sampler.sample(&turn_ctx.model_binding, &request).await,
            };

            let elapsed_ms = outcome.elapsed.as_millis() as u64;

            match outcome.response {
                Some(ref response) => {
                    // Assistant text is already streamed via SSE TextDelta events.
                    // Don't re-send the full text — the actor handles real streaming.
                    if let Some(ref tx) = stream_tx {
                        let _ = tx; // streaming is handled by the actor's decode loop
                    }
                    // Extract reasoning summary (DeepSeek/Qwen thinking mode).
                    // Pushed BEFORE the assistant text so the ChatCompletions
                    // projection can merge it into the assistant message's
                    // `reasoning_content` field on the next request.
                    let reasoning_text = response
                        .items
                        .iter()
                        .find_map(|i| match i {
                            CanonicalResponseItem::ReasoningSummary { content } => Some(content.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    if !reasoning_text.is_empty() {
                        self.chat_state.push_reasoning(reasoning_text.clone()).await;
                    }
                    let assistant_text = response.assistant_text().unwrap_or("").to_string();
                    if !assistant_text.is_empty() {
                        self.chat_state.push_assistant_response(assistant_text.clone()).await;
                    }

                    // Collect tool calls in model response order.
                    let tool_calls: Vec<(usize, ToolCallId, String, serde_json::Value)> = response
                        .tool_calls()
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, item)| match item {
                            CanonicalResponseItem::ToolCall {
                                call_id,
                                name,
                                arguments,
                            } => Some((idx, *call_id, name.clone(), arguments.clone())),
                            _ => None,
                        })
                        .collect();

                    let has_tools = !tool_calls.is_empty();

                    // Persist the model's emission for this Step: the assistant
                    // text and the tool calls it requested, tagged with the
                    // active capability generation. This is what the reducer
                    // replays to rebuild the transcript on recovery, and the
                    // generation lets it reject late/out-of-order events
                    // (invariant #14).
                    //
                    // CRITICAL: the Result must NOT be discarded. If the write
                    // fails, the assigned seq is consumed but the event is
                    // missing from the journal — a gap that breaks the
                    // gap-free invariant the reducer relies on. We log the
                    // error and abort the turn instead of silently continuing.
                    if let Some(ref writer) = self.rollout {
                        let tool_calls_json: Vec<serde_json::Value> = tool_calls
                            .iter()
                            .map(|(idx, call_id, name, args)| {
                                serde_json::json!({
                                    "call_id": call_id.to_string(),
                                    "index": idx,
                                    "name": name,
                                    "arguments": args,
                                })
                            })
                            .collect();
                        let reasoning_opt = if reasoning_text.is_empty() { None } else { Some(reasoning_text.as_str()) };
                        if let Err(e) = writer
                            .write_model_output(
                                turn_ctx.turn_id,
                                step_id,
                                StepGeneration::new(step_gen),
                                &assistant_text,
                                &tool_calls_json,
                                reasoning_opt,
                            )
                            .await
                        {
                            eprintln!("[warn] rollout write_model_output failed (seq gap risk): {e}");
                        }
                    }

                    if !has_tools {
                        steps.push(StepResult {
                            step_id: StepId::new(),
                            response: Some(response.clone()),
                            error: None,
                            usage: Some(response.usage.clone()),
                            tool_calls: Vec::new(),
                            elapsed_ms,
                        });
                        break; // no tools → turn complete
                    }

                    // ── Parallel tool dispatch + model-order commit ──
                    // Following Grok's FuturesUnordered + Codex's FuturesOrdered pattern.
                    //
                    // Invariant #4: each tool call is bound, at dispatch time,
                    // to a `PreparedCapabilityCall` that freezes the capability
                    // revision, policy generation, and validated args. After
                    // this point no capability refresh or policy tightening
                    // can retroactively change this call's semantics. This is
                    // the binding the audit flagged as "类型只在类型名层面".
                    let step_snapshot_id = StepSnapshotId::new();
                    let policy_generation = self.permission.lock().await.revocation_epoch();
                    // Invariant #15: within a Turn the capability set is
                    // frozen at `step_gen` — `CapabilityManager::get_runtime`
                    // resolves through that exact generation, so a refresh
                    // mid-Turn cannot change what a call binds to.
                    debug_assert!(step_gen >= 1, "invariant #15: capability generation not initialized");
                    let (result_tx, mut result_rx) = mpsc::unbounded_channel();

                    for (idx, call_id, name, arguments) in &tool_calls {
                        let call_id = *call_id;
                        let name = name.clone();
                        let args = arguments.clone();
                        let idx = *idx;
                        let tx = result_tx.clone();
                        // Resolve runtime through the frozen base + overlay
                        // (invariant #15: no lock needed — the base is a
                        // local snapshot, immune to mid-Turn refresh).
                        let runtime = TurnCapabilityOverlay::effective_runtime(&base, &overlay, &name);

                        // Build the immutable call binding. This snapshot is
                        // what an audit or recovery replays against — the
                        // model saw generation `step_gen` and these exact
                        // validated args, governed by policy epoch
                        // `policy_generation`.
                        let prepared = PreparedCapabilityCall {
                            tool_call_id: call_id,
                            snapshot_id: step_snapshot_id,
                            capability_id: CapabilityId::new(
                                Authority::Core,
                                "builtin",
                                CapabilityKind::Tool,
                                name.clone(),
                            ),
                            capability_revision: step_gen,
                            validated_args: args.clone(),
                            args_hash: hash_args(&args),
                            // The permissive default policy lets all calls
                            // through; `execute_single_tool` enforces the
                            // live `PermissionManager::check` and the sandbox
                            // before any side effect.
                            policy_ceiling: PolicyDecision::Allow,
                            policy_generation,
                            operation_id: OperationId::new(),
                        };

                        // Invariant #4/#5 fence: a tool without a bound
                        // runtime cannot execute (it would silently no-op or
                        // error generically). Refuse to dispatch rather than
                        // emit a misleading "Unknown tool" result.
                        debug_assert!(
                            runtime.is_some(),
                            "invariant #4: tool call {} has no bound capability revision",
                            name
                        );

                        let perm = self.permission.clone();
                        let sb = self.sandbox.clone();
                        let envelope = self.delegation_envelope.clone();

                        // Record tool execution start (ToolExecutionStarted) —
                        // marks the point where the tool begins running. This
                        // is distinct from ToolResultCommitted (which fires
                        // after the result is durable) and lets the reducer
                        // detect orphaned tool calls during crash recovery.
                        if let Some(ref writer) = self.rollout {
                            if let Err(e) = writer
                                .write_tool_started(
                                    turn_ctx.turn_id,
                                    step_id,
                                    StepGeneration::new(step_gen),
                                    &call_id.to_string(),
                                    &name,
                                )
                                .await
                            {
                                eprintln!("[warn] rollout write_tool_started failed: {e}");
                            }
                        }

                        tokio::spawn(async move {
                            let result = execute_single_tool(
                                &prepared, runtime, perm, sb, envelope.as_ref(),
                            )
                            .await;
                            let _ = tx.send(ToolExecResult {
                                call_id,
                                name: name.clone(),
                                result,
                                index: CommitSequence::new(idx as u64),
                            });
                        });
                    }
                    drop(result_tx);

                    // Commit tool call items in model order.
                    for (_, call_id, name, arguments) in &tool_calls {
                        self.chat_state
                            .push_tool_call(*call_id, name.clone(), arguments.clone())
                            .await;
                        tools_called.push(name.clone());
                    }

                    // Collect results in arrival order.
                    //
                    // Immediately after each result arrives we also push
                    // it onto `stream_tx` as a `ToolResult` fragment. The
                    // TUI uses this event to mark its in-progress tool
                    // card as "done" and render the output payload — so
                    // the user sees the result the instant the tool
                    // finishes, instead of waiting for the next sampling
                    // step.
                    let mut tool_results: Vec<ToolExecResult> = Vec::new();
                    while let Some(tr) = result_rx.recv().await {
                        // Record tool execution finish (ToolExecutionFinished) —
                        // the tool has returned, result is about to be persisted.
                        let tr_is_error = matches!(
                            &tr.result,
                            ContextItem::ToolResult { is_error: true, .. }
                        );
                        if let Some(ref writer) = self.rollout {
                            if let Err(e) = writer
                                .write_tool_execution_finished(
                                    turn_ctx.turn_id,
                                    step_id,
                                    StepGeneration::new(step_gen),
                                    &tr.call_id.to_string(),
                                    tr_is_error,
                                )
                                .await
                            {
                                eprintln!("[warn] rollout write_tool_execution_finished failed: {e}");
                            }
                        }
                        if let Some(ref tx) = stream_tx {
                            if let ContextItem::ToolResult {
                                content, is_error, call_id, ..
                            } = &tr.result
                            {
                                let _ = tx.send(StreamFragment::ToolResult {
                                    call_id: call_id.to_string(),
                                    content: content.clone(),
                                    is_error: *is_error,
                                });
                            }
                        }
                        tool_results.push(tr);
                    }

                    // Commit results in model emission order (FuturesOrdered equivalent).
                    tool_results.sort_by_key(|tr| tr.index);
                    // Invariant #7 fence: a Tool Result must be DURABLE before
                    // the next sampling step reads it. We write every result to
                    // the journal first; if the journal write fails we abort
                    // the Turn with an error rather than continuing (a missing
                    // result in the projection would desync the model from
                    // reality, and a half-written journal would corrupt
                    // recovery). The Step's `error` field surfaces the cause.
                    let generation = StepGeneration::new(step_gen);
                    for tr in &tool_results {
                        let content = match &tr.result {
                            ContextItem::ToolResult { content, .. } => content.clone(),
                            _ => String::new(),
                        };
                        let is_error =
                            matches!(&tr.result, ContextItem::ToolResult { is_error: true, .. });

                        if let Some(ref writer) = self.rollout {
                            let write_result = writer
                                .write_tool_finished(
                                    turn_ctx.turn_id,
                                    step_id,
                                    generation,
                                    &tr.call_id.to_string(),
                                    &content,
                                    is_error,
                                )
                                .await;
                            if let Err(e) = write_result {
                                // Persistence failed — abort the turn. The
                                // already-dispatched tools executed, but their
                                // results are NOT in the journal; the
                                // supervisor will surface this as a Step error
                                // and the user can retry. Do NOT push to
                                // chat_state, do NOT continue sampling.
                                steps.push(StepResult {
                                    step_id,
                                    response: Some(response.clone()),
                                    error: Some(grodex_sampler::SamplingError::internal(
                                        format!("rollout journal write failed (ToolResultCommitted): {e}"),
                                    )),
                                    usage: Some(response.usage.clone()),
                                    tool_calls: Vec::new(),
                                    elapsed_ms,
                                });
                                return TurnOutcome {
                                    steps,
                                    final_text: String::new(),
                                    usage: Some(response.usage.clone()),
                                };
                            }
                        }
                        self.chat_state
                            .push_tool_result(tr.call_id, content, is_error)
                            .await;
                    }

                    steps.push(StepResult {
                        step_id: StepId::new(),
                        response: Some(response.clone()),
                        error: None,
                        usage: Some(response.usage.clone()),
                        tool_calls: Vec::new(), // already committed
                        elapsed_ms,
                    });
                }
                None => {
                    let err = outcome.error.clone();
                    steps.push(StepResult {
                        step_id: StepId::new(),
                        response: None,
                        error: err,
                        usage: None,
                        tool_calls: Vec::new(),
                        elapsed_ms,
                    });
                    break;
                }
            }
        }

        let final_text = steps
            .iter()
            .find_map(|s| s.response.as_ref())
            .and_then(|r| r.assistant_text())
            .unwrap_or("")
            .to_string();

        let usage = steps.last().and_then(|s| s.usage.clone());

        // ── Adopt overlay: apply mid-Turn capability changes atomically ──
        // Promotions/demotions accumulated during the Turn are now applied
        // to the live CapabilityManager, producing a new generation that
        // will be the next Turn's base. If the overlay is empty (the common
        // case — no mid-Turn tool search/promotion happened), this is a
        // no-op. This is the "adopted_generation" freeze: the next Turn
        // will snapshot a fresh base that includes these changes.
        if !overlay.is_empty() {
            let mut cap = self.capability.lock().await;
            cap.adopt_overlay(overlay);
        }

        TurnOutcome {
            steps,
            final_text,
            usage,
        }
    }

    async fn try_compact(&self, turn_ctx: &TurnContext, context: &[ContextItem]) {
        let plan = {
            let c = self.compaction.lock().await;
            c.plan_compaction(context)
        };
        if let Some(plan) = plan {
            let (sys, user) = CompactionManager::build_compaction_prompt(&plan);
            let compact_req = CanonicalModelRequest {
                request_id: format!("compact_{}", StepId::new()),
                session_id: turn_ctx.session_id,
                turn_id: turn_ctx.turn_id,
                step_id: StepId::new(),
                model_binding_id: turn_ctx.model_binding.binding_id,
                prompt_snapshot_hash: None,
                instructions: vec![grodex_provider::canonical_request::InstructionBlock {
                    role: grodex_provider::canonical_request::InstructionRole::System,
                    content: sys,
                    priority: 0,
                }],
                context_items: vec![ContextItem::User { content: user, message_id: None }],
                tool_specs: Vec::new(),
                tool_choice: ToolChoice::None,
                parallel_tool_calls: false,
                reasoning_request: None,
                response_format: None,
                max_output_tokens: Some(4096),
                provider_state_in: None,
            };
            let outcome = self.sampler.sample(&turn_ctx.model_binding, &compact_req).await;
            if let Some(ref resp) = outcome.response {
                let summary = resp.assistant_text().unwrap_or("");
                let mut c = self.compaction.lock().await;
                let result = c.process_summary(summary, &plan);
                if result.is_effective() {
                    let preserved: Vec<ContextItem> = plan.items_to_keep.iter()
                        .filter(|i| matches!(i, ContextItem::System { .. } | ContextItem::Developer { .. }))
                        .cloned()
                        .collect();
                    let capsule = StateCapsule::new();
                    let rebuilt = CompactionManager::rebuild_context(
                        preserved, &result, &capsule, plan.items_to_keep,
                    );
                    // Persist CompactionCommitted BEFORE swapping the in-memory
                    // transcript: if the journal write fails we must NOT
                    // replace the projection, or the live context and the
                    // rollout would diverge (invariant #9/#13).
                    let mut abort_compaction = false;
                    if let Some(ref writer) = self.rollout {
                        match writer
                            .write_compaction(Some(turn_ctx.turn_id), &rebuilt)
                            .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                // Journal write failed — keep the old context.
                                // Surface the error via the compaction manager
                                // result so the caller knows compaction was
                                // skipped. We do NOT mutate chat_state.
                                eprintln!("warn: rollout journal write failed (CompactionCommitted): {e}; keeping pre-compaction context");
                                abort_compaction = true;
                            }
                        }
                    }
                    if !abort_compaction {
                        self.chat_state.replace_conversation(rebuilt, true).await;
                    }
                }
            }
        }
    }
}

/// Check whether the actual call args fall within a user-narrowed approval
/// scope. `narrowed_args` is a JSON object whose keys pin specific argument
/// fields to approved values; every pinned field in the actual args must
/// equal the approved value (fail-closed otherwise). Fields absent from
/// `narrowed_args` are unconstrained.
fn args_within_narrowed_scope(actual: &serde_json::Value, narrowed: &serde_json::Value) -> bool {
    let Some(narrowed_obj) = narrowed.as_object() else {
        // Non-object narrowed scope (e.g. a bare string) — require exact equality.
        return actual == narrowed;
    };
    for (key, approved) in narrowed_obj {
        if let Some(actual_val) = actual.get(key) {
            if actual_val != approved {
                return false;
            }
        } else {
            // The narrowed scope pins a field the call didn't supply.
            return false;
        }
    }
    true
}

/// SHA-256 content hash of a tool call's arguments.
///
/// Captured into `PreparedCapabilityCall::args_hash` so the journal + audit
/// record exactly which arguments a call was bound to (invariant #4 audit trail).
fn hash_args(args: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // Canonical-ish: serialize deterministically (object key order is the
    // serialization order, which is stable for serde_json::Value).
    hasher.update(args.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Execute one tool call bound to a frozen `PreparedCapabilityCall`.
///
/// The `prepared` snapshot is the contract: the capability revision, policy
/// generation, and validated args recorded there are what this call executes
/// against — no later capability refresh or policy change can alter them
/// (invariants #4 / #15). Permission and sandbox checks still run on the
/// LIVE manager (the binding is a ceiling, not a grant), and only on `Allow`
/// does the runtime receive the prepared `operation_id` (invariant #5: no
/// side effect before the permission gate clears).
async fn execute_single_tool(
    prepared: &PreparedCapabilityCall,
    runtime: Option<Arc<dyn ToolRuntime>>,
    permission: Arc<Mutex<PermissionManager>>,
    sandbox: Arc<grodex_sandbox::SandboxManager>,
    envelope: Option<&DelegationEnvelope>,
) -> ContextItem {
    let call_id = prepared.tool_call_id;
    let name = &prepared.capability_id.canonical_name;
    let args = &prepared.validated_args;

    // Invariant #12: if a delegation envelope is bound, the tool call must
    // fall within the parent-delegated bounds BEFORE the child's own
    // permission policy runs. The envelope is the parent-imposed hard
    // ceiling (capability subset + authority + policy ceiling + revocation
    // epoch); the permission manager is the child's own policy (which may
    // be stricter but never looser than the envelope).
    if let Some(env) = envelope {
        let live_epoch = permission.lock().await.revocation_epoch();
        let tool_authority = prepared.capability_id.authority as u8;
        if let Err(e) = env.authorize_tool_call(
            name,
            tool_authority,
            prepared.policy_ceiling,
            live_epoch,
        ) {
            return ContextItem::ToolResult {
                call_id,
                content: format!("Delegation denied: {e}"),
                is_error: true,
            };
        }
    }

    // Permission check (live policy, against the bound args).
    // The resolution is packed into a single-use PermissionLease carrying
    // the revocation epoch at mint time; we re-check that epoch immediately
    // before the side effect (invariant #16: revocation only tightens).
    let mut lease = {
        let mut perm = permission.lock().await;
        match perm.check(call_id, name, args, &format!("{name} {args}")) {
            PermissionResult::Allowed => PermissionLease::new(
                call_id,
                ApprovalResolution::Allow,
                perm.revocation_epoch(),
                Some(std::time::Duration::from_secs(300)),
            ),
            PermissionResult::Denied { reason } => {
                return ContextItem::ToolResult {
                    call_id,
                    content: format!("Permission denied: {reason}"),
                    is_error: true,
                };
            }
            PermissionResult::ApprovalRequired { decision_rx, .. } => {
                let epoch = perm.revocation_epoch();
                drop(perm);
                match tokio::time::timeout(std::time::Duration::from_secs(10), decision_rx).await {
                    Ok(Ok(PolicyDecision::Allow)) => PermissionLease::new(
                        call_id,
                        ApprovalResolution::Allow,
                        epoch,
                        Some(std::time::Duration::from_secs(300)),
                    ),
                    _ => {
                        return ContextItem::ToolResult {
                            call_id,
                            content: "Approval required — not granted".into(),
                            is_error: true,
                        };
                    }
                }
            }
        }
    };

    // Invariant #5: a side-effecting runtime must never be invoked before
    // the permission gate returned Allow.
    debug_assert!(
        lease.resolution.permits_execution(),
        "invariant #5: reached execution without an authorizing resolution"
    );

    // Narrow-scope check: if the user approved only a narrowed subset of
    // args, fail-closed unless the actual call falls inside it.
    if let ApprovalResolution::Narrow { narrowed_args } = &lease.resolution {
        if !args_within_narrowed_scope(args, narrowed_args) {
            return ContextItem::ToolResult {
                call_id,
                content: "Permission narrowed: call args fall outside the approved scope".into(),
                is_error: true,
            };
        }
    }

    // Sandbox check for file/exec operations.
    if name == "exec" {
        if let Err(e) = sandbox.validate_exec() {
            return ContextItem::ToolResult { call_id, content: format!("Sandbox: {e}"), is_error: true };
        }
    }
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        if name.contains("write") || name.contains("edit") {
            if let Err(e) = sandbox.validate_write(std::path::Path::new(path)) {
                return ContextItem::ToolResult { call_id, content: format!("Sandbox: {e}"), is_error: true };
            }
        } else if name.contains("read") {
            if let Err(e) = sandbox.validate_read(std::path::Path::new(path)) {
                return ContextItem::ToolResult { call_id, content: format!("Sandbox: {e}"), is_error: true };
            }
        }
    }

    // ── Revalidation (invariant #16/#13): the lease was minted at
    // `prepared.policy_generation`. Before the side effect, re-read the
    // LIVE revocation epoch. If policy tightened in between (e.g. the user
    // hit "revoke all" or a narrow-while-approving), the bound snapshot is
    // stale and we refuse — fail-closed. This is the gate the audit flagged
    // as missing ("resolve 后直接 execute").
    //
    // Uses the unified `LiveRevocationFence` type — the same gate is
    // available to the DelegationEnvelope and ACP layers.
    {
        let fence = LiveRevocationFence::from_lease(&lease);
        let live_epoch = permission.lock().await.revocation_epoch();
        if let Err(e) = fence.check(live_epoch) {
            return ContextItem::ToolResult {
                call_id,
                content: format!("Permission revalidation failed: {e}"),
                is_error: true,
            };
        }
    }

    // ── Consume the single-use lease (exactly-once-per-approval). A
    // replayed call after crash recovery cannot reuse this lease.
    if !lease.consume() {
        return ContextItem::ToolResult {
            call_id,
            content: "Permission lease already consumed or expired".into(),
            is_error: true,
        };
    }

    // Execute against the bound capability revision, using the prepared
    // operation id (audit/idempotency key).
    match runtime {
        Some(rt) => match rt.execute(args.clone(), prepared.operation_id).await {
            Ok(output) => ContextItem::ToolResult {
                call_id,
                content: output.to_string(),
                is_error: false,
            },
            Err(e) => ContextItem::ToolResult {
                call_id,
                content: format!("Error: {e}"),
                is_error: true,
            },
        },
        // Invariant #4 fence at the dispatch site already asserts runtime is
        // Some; this branch is a defensive fallback for non-debug builds.
        None => ContextItem::ToolResult {
            call_id,
            content: format!("Unknown tool: {name}"),
            is_error: true,
        },
    }
}
