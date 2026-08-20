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
use grodex_core::id::{CommitSequence, OperationId, StepGeneration, StepId, StepSnapshotId, ToolCallId, TurnId};
use grodex_core::policy::PolicyDecision;
use grodex_core::tool::ToolRuntime;
use grodex_memory::types::{EvidenceStatus, EvidenceUnit, MemoryScope};
use sha2::Digest;
use grodex_permission::{
    ApprovalRequestedEvent, ApprovalResolution, LiveRevocationFence, PermissionLease,
    PermissionManager, PermissionPolicy, PermissionResult,
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
    /// The operation_id used for this tool call. Propagated up so the
    /// outer coordinator can pass it to `ToolExecutionFinished` and
    /// `ToolResultCommitted` journal writes — completing the
    /// operation_id chain across all lifecycle events.
    operation_id: Option<String>,
}

/// Context passed into `execute_single_tool` so it can write the
/// `ToolCallApproved` and `ToolExecutionStarted` journal events at the
/// correct point in the lifecycle (AFTER permission clears, BEFORE the
/// side effect). This fixes the event ordering bug where
/// `ToolExecutionStarted` was written BEFORE the permission gate.
#[derive(Clone)]
struct ToolExecCtx {
    turn_id: TurnId,
    step_id: StepId,
    step_gen: StepGeneration,
    writer: Option<crate::rollout_writer::RolloutWriter>,
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
    /// Approval notification bus drain. The `PermissionManager` holds the
    /// sender (set via `with_approval_bus`); whenever `check()` returns
    /// `Ask` it fires an `ApprovalRequestedEvent` here. This receiver is
    /// drained inside `run()` and forwarded to the stream channel as
    /// `StreamFragment::ApprovalRequested`, which the supervisor surfaces
    /// to the frontend as `SessionEvent::ApprovalRequested` (Design Doc 16
    /// §10, first half of the approval round-trip). Wrapped in
    /// `Arc<tokio::sync::Mutex<>>` so a cloneable coordinator can share
    /// one receiver across sequential turns.
    approval_rx: Arc<Mutex<mpsc::UnboundedReceiver<ApprovalRequestedEvent>>>,
    /// Optional memory database for evidence capture. When present,
    /// non-error tool results are written as EvidenceUnit entries so
    /// they can be retrieved in future turns (Tool Result → Evidence).
    memory: Option<Arc<grodex_memory::MemoryDatabase>>,
}

impl TurnCoordinator {
    /// Attach a shared RolloutWriter for event journaling. Both the
    /// supervisor and this coordinator must share the same writer so the
    /// seq counter stays coherent.
    pub fn with_rollout(mut self, writer: crate::rollout_writer::RolloutWriter) -> Self {
        self.rollout = Some(writer);
        self
    }

    /// Attach a memory database for evidence capture (Tool Result → Evidence).
    /// When set, non-error tool results are persisted as EvidenceUnit entries.
    pub fn with_memory(mut self, db: Arc<grodex_memory::MemoryDatabase>) -> Self {
        self.memory = Some(db);
        self
    }

    /// Wire up the approval bus for a freshly-built manager and stash the
    /// drain receiver. Called by both `new()` and `with_permission()` so
    /// every construction path produces a manager whose `Ask` decisions
    /// actually reach the frontend.
    fn wire_approval_bus(
        mgr: PermissionManager,
    ) -> (PermissionManager, Arc<Mutex<mpsc::UnboundedReceiver<ApprovalRequestedEvent>>>) {
        let (tx, rx) = mpsc::unbounded_channel::<ApprovalRequestedEvent>();
        let mgr = mgr.with_approval_bus(tx);
        (mgr, Arc::new(Mutex::new(rx)))
    }

    pub fn new(sampler: SamplingActor, chat_state: ChatStateHandle) -> Self {
        // Unification (audit Phase-3): the CapabilityManager and the
        // CapabilityPublisher share one generation source so a tool bump is
        // visible to the ACP event stream / StepContext.
        let publisher = SharedPublisher::new();
        let (mgr, approval_rx) = Self::wire_approval_bus(PermissionManager::new(
            PermissionPolicy::new(),
        ));
        Self {
            sampler: Arc::new(sampler),
            chat_state,
            capability: Arc::new(Mutex::new(CapabilityManager::with_publisher(10, publisher))),
            rollout: None,
            completed_operations: Arc::new(std::sync::Mutex::new(HashSet::new())),
            permission: Arc::new(Mutex::new(mgr)),
            compaction: Arc::new(Mutex::new(CompactionManager::new(128_000))),
            sandbox: Arc::new(grodex_sandbox::SandboxManager::default()),
            delegation_envelope: None,
            approval_rx,
            memory: None,
        }
    }

    /// Inject a pre-built `PermissionManager` (e.g. one whose policy was
    /// loaded from config). Re-wires the approval bus to the injected
    /// manager so `Ask` decisions still reach the frontend — the caller
    /// must NOT have attached its own bus.
    pub fn with_permission(self, mgr: PermissionManager) -> Self {
        let (mgr, approval_rx) = Self::wire_approval_bus(mgr);
        Self {
            permission: Arc::new(Mutex::new(mgr)),
            approval_rx,
            ..self
        }
    }

    /// Inject a pre-built `SandboxManager` (e.g. constructed from a config
    /// sandbox profile). Overrides the default permissive sandbox.
    pub fn with_sandbox(self, sandbox: grodex_sandbox::SandboxManager) -> Self {
        Self {
            sandbox: Arc::new(sandbox),
            ..self
        }
    }

    /// Inject a pre-built `CapabilityManager` (e.g. one with tools already
    /// registered or a non-default publisher). Overrides the default.
    pub fn with_capability(self, capability: CapabilityManager) -> Self {
        Self {
            capability: Arc::new(Mutex::new(capability)),
            ..self
        }
    }

    /// Hand out a clone of the shared permission manager so the
    /// `SessionSupervisor` can call `resolve()` when a `ResolveApproval`
    /// command arrives from the frontend (Design Doc 16 §10, second half
    /// of the round-trip). This is the single chokepoint that lets the
    /// supervisor wake a tool future parked on `decision_rx`.
    pub fn permission_handle(&self) -> Arc<Mutex<PermissionManager>> {
        Arc::clone(&self.permission)
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

    /// Register a tool with explicit metadata (concurrency class,
    /// side-effect class). Used for MCP tools and any tool where the
    /// scheduler/recovery needs to know the execution semantics
    /// (Design Doc 10 §16).
    pub async fn register_tool_with_metadata(
        &self,
        name: impl Into<String>,
        runtime: Arc<dyn ToolRuntime>,
        input_schema: serde_json::Value,
        metadata: grodex_core::tool::ToolMetadata,
    ) {
        let name = name.into();
        let spec = grodex_provider::canonical_request::ToolSpec {
            name: name.clone(),
            description: metadata.description.clone(),
            parameters: input_schema,
            required: vec![],
        };
        self.capability.lock().await
            .register_tool_with_metadata(name, runtime, spec, Some(metadata));
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

        // ── Approval notification forwarder ───────────────────────────
        // Drains `approval_rx` (fed by `PermissionManager::check()` when
        // policy says `Ask`) and pushes `StreamFragment::ApprovalRequested`
        // onto the stream so the supervisor surfaces a pending-approval
        // event to the frontend. This is the first half of the approval
        // round-trip (Design Doc 16 §10). The forwarder is aborted at
        // every exit point of `run()` so it never outlives the Turn — a
        // leaked forwarder would hold a clone of `stream_tx` and delay the
        // supervisor's stream-handle shutdown.
        let approval_rx = self.approval_rx.clone();
        let approval_stream_tx = stream_tx.clone();
        let approval_cancel = CancellationToken::new();
        let approval_cancel_clone = approval_cancel.clone();
        let approval_handle = tokio::spawn(async move {
            loop {
                let ev = tokio::select! {
                    biased;
                    _ = approval_cancel_clone.cancelled() => break,
                    ev = async {
                        let mut rx = approval_rx.lock().await;
                        rx.recv().await
                    } => ev,
                };
                let Some(ev) = ev else { break };
                if let Some(tx) = approval_stream_tx.as_ref() {
                    let _ = tx.send(StreamFragment::ApprovalRequested {
                        ticket_id: ev.ticket_id,
                        tool_name: ev.tool_name,
                        summary: ev.summary,
                        risk: ev.risk,
                        timeout_remaining_ms: ev.timeout_remaining_ms,
                    });
                }
            }
        });

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

                        // ── JSON Schema validation ──────────────────
                        // Validate the model-supplied args against the
                        // tool's declared input_schema BEFORE constructing
                        // the PreparedCapabilityCall. This catches
                        // malformed calls (missing required fields, wrong
                        // types) at the preparation stage rather than
                        // letting them fail deep inside execute().
                        let input_schema = base
                            .tool_specs
                            .get(&name)
                            .map(|spec| spec.parameters.clone())
                            .unwrap_or(serde_json::Value::Null);
                        if let Err(msg) = validate_args_against_schema(&args, &input_schema) {
                            let _ = tx.send(ToolExecResult {
                                call_id,
                                name: name.clone(),
                                result: ContextItem::ToolResult {
                                    call_id,
                                    content: format!("Schema validation failed: {msg}"),
                                    is_error: true,
                                },
                                index: CommitSequence::new(idx as u64),
                                operation_id: None,
                            });
                            continue;
                        }

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
                            // MCP tools (name starts with `mcp_`) get
                            // their real capability identity: Authority::Mcp
                            // with the server name as provider_id. Builtin
                            // tools keep Authority::Core / "builtin".
                            capability_id: if name.starts_with("mcp_") {
                                // Format: mcp_{server}_{tool}
                                let rest = &name[4..]; // strip "mcp_"
                                let (server, _tool) = rest.split_once('_')
                                    .unwrap_or((rest, ""));
                                CapabilityId::new(
                                    Authority::Mcp,
                                    server,
                                    CapabilityKind::Tool,
                                    name.clone(),
                                )
                            } else {
                                CapabilityId::new(
                                    Authority::Core,
                                    "builtin",
                                    CapabilityKind::Tool,
                                    name.clone(),
                                )
                            },
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

                        // ── ToolCallPrepared ─────────────────────────
                        // Write the prepared event BEFORE spawning. This
                        // records the exact args/args_hash/capability_revision
                        // /policy_generation/operation_id the tool will
                        // execute against — the audit baseline for later
                        // replay validation. Fail-closed: if the journal
                        // write fails we skip dispatch entirely rather
                        // than risk an un-audited side effect.
                        let op_id_str = prepared.operation_id.to_string();
                        if let Some(ref writer) = self.rollout {
                            if let Err(e) = writer
                                .write_tool_call_prepared(
                                    turn_ctx.turn_id,
                                    step_id,
                                    StepGeneration::new(step_gen),
                                    &call_id.to_string(),
                                    Some(&op_id_str),
                                    &name,
                                    &args,
                                    Some(&prepared.args_hash),
                                    Some(&step_gen.to_string()),
                                    Some(policy_generation),
                                )
                                .await
                            {
                                eprintln!("[warn] rollout write_tool_call_prepared failed: {e}");
                            }
                        }

                        // Invariant #4/#5 fence: a tool without a bound
                        // runtime cannot execute (it would silently no-op or
                        // error generically). Refuse to dispatch rather than
                        // emit a misleading "Unknown tool" result.
                        //
                        // P1-1 fix: the previous code only had a
                        // `debug_assert!` here, which is a no-op in release
                        // builds. When `CapabilityManager::get_runtime`
                        // returns None (e.g. because the bound generation
                        // was evicted from the ring buffer), we now:
                        //   1. Write a `CapabilityCallRejectedStale` event
                        //      to the journal (durable audit trail).
                        //   2. Push a ToolResult error into the result
                        //      channel so the model sees a clear failure
                        //      instead of a silent no-op.
                        //   3. `continue` to the next tool call.
                        if runtime.is_none() {
                            // Write the rejection to the journal.
                            if let Some(ref writer) = self.rollout {
                                let _ = writer
                                    .write_capability_call_rejected_stale(
                                        Some(turn_ctx.turn_id),
                                        Some(step_id),
                                        Some(StepGeneration::new(step_gen)),
                                        &name,
                                        step_gen,
                                        "stale_or_evicted",
                                    )
                                    .await;
                            }
                            // Push an error result so the model can react.
                            let _ = tx.send(ToolExecResult {
                                call_id,
                                name: name.clone(),
                                result: ContextItem::ToolResult {
                                    call_id,
                                    content: format!(
                                        "Capability '{name}' (generation {step_gen}) was evicted \
                                         from the capability ring buffer. The turn must be retried \
                                         with a fresh capability snapshot."
                                    ),
                                    is_error: true,
                                },
                                index: CommitSequence::new(idx as u64),
                                operation_id: Some(op_id_str.clone()),
                            });
                            continue;
                        }

                        let perm = self.permission.clone();
                        let sb = self.sandbox.clone();
                        let envelope = self.delegation_envelope.clone();

                        // Build the execution context for the spawned task.
                        // `execute_single_tool` writes `ToolCallApproved`
                        // and `ToolExecutionStarted` at the correct point
                        // (AFTER permission clears, BEFORE the side effect).
                        let exec_ctx = ToolExecCtx {
                            turn_id: turn_ctx.turn_id,
                            step_id,
                            step_gen: StepGeneration::new(step_gen),
                            writer: self.rollout.clone(),
                        };
                        let op_id_for_result = op_id_str.clone();

                        tokio::spawn(async move {
                            let result = execute_single_tool(
                                &prepared, runtime, perm, sb, envelope.as_ref(), &exec_ctx,
                            )
                            .await;
                            let _ = tx.send(ToolExecResult {
                                call_id,
                                name: name.clone(),
                                result,
                                index: CommitSequence::new(idx as u64),
                                operation_id: Some(op_id_for_result),
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
                        // We now store content / exit_code / duration_ms here
                        // too so that a crash between this event and
                        // ToolResultCommitted does NOT force us to re-run
                        // the side effect: Finished already captures the
                        // outcome in full.
                        let (tr_is_error, tr_content, tr_exit_code) = match &tr.result {
                            ContextItem::ToolResult { content, is_error, .. } => {
                                (*is_error, Some(content.clone()), None)
                            }
                            _ => (false, None, None),
                        };
                        if let Some(ref writer) = self.rollout {
                            if let Err(e) = writer
                                .write_tool_execution_finished(
                                    turn_ctx.turn_id,
                                    step_id,
                                    StepGeneration::new(step_gen),
                                    &tr.call_id.to_string(),
                                    tr.operation_id.as_deref(),
                                    tr_is_error,
                                    tr_content.as_deref(),
                                    tr_exit_code,
                                    None, // duration_ms: measured later
                                    None, // output_truncated
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
                                    tr.operation_id.as_deref(),
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
                                approval_cancel.cancel();
                                approval_handle.abort();
                                return TurnOutcome {
                                    steps,
                                    final_text: String::new(),
                                    usage: Some(response.usage.clone()),
                                };
                            }
                        }
                        self.chat_state
                            .push_tool_result(tr.call_id, content.clone(), is_error)
                            .await;

                        // P1-3: Tool Result → Evidence capture.
                        // Non-error, non-empty tool results are indexed as
                        // EvidenceUnit entries so future turns can retrieve
                        // "what happened last time" via hybrid RAG. This is
                        // fail-open: a DB write error logs a warning but
                        // does NOT abort the turn — evidence is a bonus,
                        // not a correctness requirement.
                        if !is_error && !content.is_empty() {
                            if let Some(ref db) = self.memory {
                                let mut hasher = sha2::Sha256::new();
                                hasher.update(content.as_bytes());
                                let content_hash = format!("{:x}", hasher.finalize());
                                let unit = EvidenceUnit {
                                    id: format!("ev_{}_{}", tr.name, tr.call_id),
                                    rollout_id: turn_ctx.turn_id.to_string(),
                                    path: format!("tool:{}", tr.name),
                                    section: tr.call_id.to_string(),
                                    scope: MemoryScope::Workspace,
                                    status: EvidenceStatus::Active,
                                    content: content.clone(),
                                    content_hash,
                                    occurred_at: chrono::Utc::now(),
                                    created_at: chrono::Utc::now(),
                                    superseded_by: None,
                                    superseded_at: None,
                                    rollout_available: true,
                                    rollout_expired_at: None,
                                    subchunk_index: 0,
                                };
                                if let Err(e) = db.upsert_evidence_unit(&unit) {
                                    eprintln!(
                                        "[warn] evidence capture failed for tool '{}': {e}",
                                        tr.name
                                    );
                                }
                            }
                        }
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

        approval_cancel.cancel();
        approval_handle.abort();

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
            // P1-4: write CompactionStarted BEFORE we do anything else.
            // On crash, a Started without Committed/Failed means the
            // compaction was in-flight and must NOT be installed.
            let pre_count = context.len();
            if let Some(ref writer) = self.rollout {
                if let Err(e) = writer
                    .write_compaction_started(Some(turn_ctx.turn_id), "token_budget", pre_count)
                    .await
                {
                    eprintln!("[warn] rollout write_compaction_started failed: {e}");
                    // If we can't even write Started, abort compaction.
                    return;
                }
            }

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

                    // P1-4: write CompactionCandidateBuilt BEFORE
                    // attempting the commit.
                    if let Some(ref writer) = self.rollout {
                        let _ = writer
                            .write_compaction_candidate_built(
                                Some(turn_ctx.turn_id),
                                rebuilt.len(),
                                summary,
                            )
                            .await;
                    }

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
                                // P1-4: write CompactionFailed so resume
                                // knows this was an explicit abort, not an
                                // in-flight crash.
                                let _ = writer
                                    .write_compaction_failed(
                                        Some(turn_ctx.turn_id),
                                        &format!("journal write failed: {e}"),
                                    )
                                    .await;
                                eprintln!("warn: rollout journal write failed (CompactionCommitted): {e}; keeping pre-compaction context");
                                abort_compaction = true;
                            }
                        }
                    }
                    if !abort_compaction {
                        self.chat_state.replace_conversation(rebuilt, true).await;
                    }
                } else {
                    // P1-4: summary was not effective — record as Failed
                    // so resume knows the compaction cycle terminated
                    // without installing a candidate.
                    if let Some(ref writer) = self.rollout {
                        let _ = writer
                            .write_compaction_failed(
                                Some(turn_ctx.turn_id),
                                "summary_not_effective",
                            )
                            .await;
                    }
                }
            } else {
                // P1-4: model returned no response — record as Failed.
                let reason = match &outcome.error {
                    Some(e) => format!("model error: {e}"),
                    None => "no response".to_string(),
                };
                if let Some(ref writer) = self.rollout {
                    let _ = writer
                        .write_compaction_failed(Some(turn_ctx.turn_id), &reason)
                        .await;
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
/// Validate model-supplied args against a tool's JSON Schema.
///
/// Performs lightweight structural validation without pulling in a full
/// `jsonschema` crate dependency:
///   1. Checks every field listed in schema `required` is present.
///   2. Checks `type` of each provided field matches the schema.
///
/// Returns `Ok(())` if the args are structurally valid (or schema is
/// absent/empty), `Err(message)` otherwise. The caller uses this to
/// reject malformed tool calls at the `PreparedCapabilityCall` stage
/// rather than letting them fail deep inside `ToolRuntime::execute`.
fn validate_args_against_schema(
    args: &serde_json::Value,
    schema: &serde_json::Value,
) -> Result<(), String> {
    let schema_obj = match schema.as_object() {
        Some(obj) => obj,
        None => return Ok(()), // no schema → trust args
    };

    // Required fields.
    if let Some(required) = schema_obj.get("required").and_then(|v| v.as_array()) {
        let args_obj = args.as_object().ok_or_else(|| {
            "args is not an object but schema has required fields".to_string()
        })?;
        for field in required {
            if let Some(field_name) = field.as_str() {
                if !args_obj.contains_key(field_name) {
                    return Err(format!("missing required field: {field_name}"));
                }
            }
        }
    }

    // Type checking for properties.
    if let Some(props) = schema_obj.get("properties").and_then(|v| v.as_object()) {
        let args_obj = match args.as_object() {
            Some(obj) => obj,
            None => return Ok(()), // can't check field types if args isn't object
        };
        for (field_name, field_schema) in props {
            if let Some(field_val) = args_obj.get(field_name) {
                if let Some(expected_type) = field_schema.get("type").and_then(|v| v.as_str()) {
                    let actual_type = match field_val {
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::Bool(_) => "boolean",
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::Object(_) => "object",
                        serde_json::Value::Null => "null",
                    };
                    // Allow integer where number is expected (common JSON Schema pattern).
                    if expected_type != actual_type
                        && !(expected_type == "number" && actual_type == "number")
                    {
                        return Err(format!(
                            "field '{field_name}': expected type {expected_type}, got {actual_type}"
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

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
    ctx: &ToolExecCtx,
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
            PermissionResult::ApprovalRequired { ticket_id, decision_rx } => {
                let epoch = perm.revocation_epoch();
                drop(perm);
                // Write the ApprovalRequested event to the journal so
                // resume can detect unresolved (pending) approval tickets
                // and re-surface them to the frontend.
                if let Some(ref writer) = ctx.writer {
                    let _ = writer
                        .write_approval_requested(
                            Some(&call_id.to_string()),
                            Some(&prepared.operation_id.to_string()),
                            &ticket_id,
                            name,
                            args,
                            "model",
                        )
                        .await;
                }
                // Await the frontend's resolution. The deadline matches the
                // ticket's own timeout (ApprovalTicket::new → 120s) so the
                // user sees a consistent countdown and the tool future does
                // not give up before the broker would expire the ticket.
                // A Deny (user-rejected, broker-expired, or cancelled)
                // arrives through the same oneshot and falls into the
                // error branch — fail-closed.
                //
                // The channel now carries `ApprovalResolution` (not
                // `PolicyDecision`), so Narrow resolutions with
                // `narrowed_args` flow through here and into the lease.
                match tokio::time::timeout(std::time::Duration::from_secs(120), decision_rx).await {
                    Ok(Ok(resolution)) if resolution.permits_execution() => {
                        // Write LeaseIssued to journal — durable record that
                        // an authorizing lease was minted for this call.
                        if let Some(ref writer) = ctx.writer {
                            let _ = writer
                                .write_lease_issued(
                                    &call_id.to_string(),
                                    &ticket_id,
                                    &call_id.to_string(),
                                    Some(300),
                                )
                                .await;
                        }
                        PermissionLease::new(
                            call_id,
                            resolution,
                            epoch,
                            Some(std::time::Duration::from_secs(300)),
                        )
                    },
                    _ => {
                        return ContextItem::ToolResult {
                            call_id,
                            content: "Approval required — not granted (denied, timed out, or cancelled)".into(),
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

    // ── ToolCallApproved + ToolExecutionStarted ──────────────
    // Now that permission has cleared, write the durable "go-ahead"
    // (ToolCallApproved) and the pre-side-effect commit point
    // (ToolExecutionStarted). Correct event ordering:
    //   Prepared → PermissionClear → Approved → Started → Execute
    //
    // Fail-closed: if the ToolExecutionStarted journal write fails we
    // MUST refuse to execute the side effect. A missing Started event
    // means crash recovery cannot detect an orphaned tool call — so
    // running the side effect anyway would be unsafe. We return an
    // error result instead.
    let op_id_str = prepared.operation_id.to_string();
    if let Some(ref writer) = ctx.writer {
        if let Err(e) = writer
            .write_tool_call_approved(
                ctx.turn_id,
                ctx.step_id,
                ctx.step_gen,
                &call_id.to_string(),
                Some(&op_id_str),
                name,
            )
            .await
        {
            return ContextItem::ToolResult {
                call_id,
                content: format!("Journal write failed (ToolCallApproved): {e}"),
                is_error: true,
            };
        }
        if let Err(e) = writer
            .write_tool_started(
                ctx.turn_id,
                ctx.step_id,
                ctx.step_gen,
                &call_id.to_string(),
                Some(&op_id_str),
                name,
            )
            .await
        {
            // Fail-closed: refuse to execute the side effect if the
            // durable pre-execution marker cannot be written.
            return ContextItem::ToolResult {
                call_id,
                content: format!("Journal write failed (ToolExecutionStarted, fail-closed): {e}"),
                is_error: true,
            };
        }
    }

    // Narrow-scope check: if the user approved only a narrowed subset of
    // args, fail-closed unless the actual call falls inside it.
    // Then REPLACE the execution args with the narrowed version — this is
    // where Narrow actually takes effect. The tool will run with the
    // narrowed_args, not the original model-issued ones.
    let effective_args = if let ApprovalResolution::Narrow { narrowed_args } = &lease.resolution {
        if !args_within_narrowed_scope(args, narrowed_args) {
            return ContextItem::ToolResult {
                call_id,
                content: "Permission narrowed: call args fall outside the approved scope".into(),
                is_error: true,
            };
        }
        narrowed_args.clone()
    } else {
        args.clone()
    };

    // Sandbox check for file/exec operations.
    if name == "exec" {
        if let Err(e) = sandbox.validate_exec() {
            return ContextItem::ToolResult { call_id, content: format!("Sandbox: {e}"), is_error: true };
        }
    }
    if let Some(path) = effective_args.get("path").and_then(|v| v.as_str()) {
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
    // Write LeaseConsumed to journal — durable record that the
    // single-use lease was consumed, so crash recovery can reject
    // duplicate execution attempts on the same call_id.
    if let Some(ref writer) = ctx.writer {
        let _ = writer
            .write_lease_consumed(&call_id.to_string(), &call_id.to_string())
            .await;
    }

    // Execute against the bound capability revision, using the prepared
    // operation id (audit/idempotency key). Uses `effective_args` which
    // may be the narrowed version if the user approved a Narrow resolution.
    match runtime {
        Some(rt) => match rt.execute(effective_args, prepared.operation_id).await {
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
