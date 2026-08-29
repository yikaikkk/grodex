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
use crate::step::{classify_step, StepDisposition, TurnOutcome};
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
use grodex_provider::prompt_snapshot::PromptSnapshot;
use grodex_sampler::{SamplingActor, StreamFragment};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

/// Per-turn budget for repair sampling (P0-1).
///
/// 当模型以 `StopReason::Stop` 结束且没有产生 Tool Call、文本非空时，它
/// 很可能只是「描述了下一步计划」而非真正完成。此时注入一条 repair prompt
/// 让模型二选一（总结收尾 / 调用工具继续），并重新采样。
///
/// 预算防止无限循环：连续两次无工具 `Stop`（中间没有工具调度）即视为自然
/// 完成。取值 1 对齐文档「一次 repair sampling」，同时把真正完成的 turn 的
/// 额外采样成本限制在 1 次以内。
const REPAIR_SAMPLING_BUDGET: u8 = 1;

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
    /// T10: tool execution wall-clock duration in milliseconds (from
    /// `ToolExecutionStarted` to result return). `None` when the tool
    /// failed before reaching the execution phase (permission denied,
    /// journal write fail, etc.).
    duration_ms: Option<u64>,
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
    /// JSON Schema for the tool being executed (if available).
    /// Used by Narrow flow to validate narrowed_args structure.
    tool_schema: Option<serde_json::Value>,
    /// T5: oversized-result offload threshold. When the tool output
    /// exceeds this many bytes, it is offloaded to the blob store (or
    /// temp file) RIGHT HERE — before the result enters the channel —
    /// so the channel and receiver loop never hold the full payload.
    /// `0` disables early offload (results flow through unchanged).
    max_tool_result_bytes: usize,
    /// T5: managed blob store for oversized result offload. When
    /// present, early offload writes to the blob store; otherwise it
    /// falls back to the temp-file path.
    blob_store: Option<Arc<grodex_tools::ManagedBlobStore<grodex_tools::FileBlobStore>>>,
    /// T5: session_id for blob ownership + temp-dir isolation.
    session_id: String,
    /// T10: per-tool execution timeout in seconds. `0` = no timeout.
    /// When the tool runs longer than this, it is cancelled and an
    /// error result is returned (the underlying tool future is dropped,
    /// which for async tools cancels the pending operation).
    tool_timeout_secs: u64,
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
    /// Maximum size (bytes) of a single tool result kept in-context.
    /// Larger results are offloaded to a temp file and replaced with a
    /// short preview + file reference, preventing one huge output (e.g.
    /// reading a big file) from bloating the context window.
    /// Configurable via `max_tool_result_bytes` (default 32KB).
    max_tool_result_bytes: usize,
    /// Maximum number of sampling steps per turn. When exhausted the
    /// coordinator forces one final tool-less summary sample so a long
    /// task never dies silently mid-way. Configurable via
    /// `max_steps_per_turn` (default 40).
    max_steps: usize,
    /// Managed blob store backing oversized tool-result offload (Design
    /// Doc 11 §22). When present, offloaded results are stored as owned
    /// blobs (owner = `ToolResult` / session id) whose lifetime is
    /// governed by the `blob_refs` projection instead of scattered temp
    /// files; when absent, the legacy temp-file path is used.
    blob_store: Option<Arc<grodex_tools::ManagedBlobStore<grodex_tools::FileBlobStore>>>,
    /// T10: per-tool execution timeout in seconds. `0` = no timeout.
    /// When a tool runs longer than this, it is cancelled and an error
    /// result is returned to the model. Prevents a single long-running
    /// tool (e.g. a hung network call) from blocking the entire turn.
    tool_timeout_secs: u64,
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

    /// Set the model's context window size (in tokens) so compaction
    /// triggers at the right threshold. The default (128K) is too small
    /// for modern models with 1M+ windows — without this override,
    /// compaction fires prematurely or the context grows past the model's
    /// actual limit before compaction can catch up.
    pub fn with_context_window(self, window: u64) -> Self {
        // Use try_lock — the compaction mutex is only held briefly during
        // the compaction check; contention at construction time is impossible
        // because the coordinator hasn't started running yet.
        if let Ok(mut c) = self.compaction.try_lock() {
            c.set_context_window(window);
        }
        self
    }

    /// Set the compaction trigger threshold as a percentage of the context
    /// window (e.g. 85 → compact when usage reaches 85% of the window).
    /// Lets compaction fire proactively instead of waiting for a 413 /
    /// context-overflow error from the API.
    pub fn with_compaction_threshold(self, percent: u8) -> Self {
        if let Ok(mut c) = self.compaction.try_lock() {
            c.set_threshold_percent(percent);
        }
        self
    }

    /// Set the in-context size cap for a single tool result. Results
    /// larger than this are saved to a temp file and replaced with a
    /// preview + path reference. `0` disables offloading.
    pub fn with_max_tool_result_bytes(mut self, bytes: usize) -> Self {
        self.max_tool_result_bytes = bytes;
        self
    }

    /// Set the maximum sampling steps allowed in a single turn. When the
    /// budget is exhausted a final tool-less summary is still generated
    /// (see `run`). `0` falls back to the default (40).
    pub fn with_max_steps(mut self, steps: usize) -> Self {
        self.max_steps = if steps == 0 { 40 } else { steps };
        self
    }

    /// Attach a managed blob store used for oversized tool-result offload
    /// (Doc 11 §22). Offloaded content becomes an owned blob whose
    /// lifetime is governed by the `blob_refs` projection; call
    /// `release_session_blobs` when the session ends to reclaim them.
    pub fn with_blob_store(
        mut self,
        store: Arc<grodex_tools::ManagedBlobStore<grodex_tools::FileBlobStore>>,
    ) -> Self {
        self.blob_store = Some(store);
        self
    }

    /// T10: set the per-tool execution timeout. When a tool runs longer
    /// than this, it is cancelled and an error result is returned.
    /// `0` (default) disables the timeout.
    pub fn with_tool_timeout_secs(mut self, secs: u64) -> Self {
        self.tool_timeout_secs = secs;
        self
    }

    /// Reclaim all blobs owned by `session_id`'s offloaded tool results
    /// (Doc 11 §22): revoke the owner's references, then run GC at a
    /// point past the retention grace so the backing files are deleted
    /// deterministically at session shutdown (the grace period protects
    /// against concurrent readers, which no longer exist here).
    pub async fn release_session_blobs(&self, session_id: &str) {
        let Some(store) = &self.blob_store else {
            return;
        };
        let revoked = store.revoke_owner(grodex_tools::BlobOwnerKind::ToolResult, session_id);
        let deadline = std::time::SystemTime::now() + store.grace();
        let deleted = store.gc_at(deadline).await;
        if !revoked.is_empty() || !deleted.is_empty() {
            tracing::info!(
                session_id,
                revoked = revoked.len(),
                deleted = deleted.len(),
                "released session tool-result blobs"
            );
        }
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
            max_tool_result_bytes: 32 * 1024,
            max_steps: 40,
            blob_store: None,
            tool_timeout_secs: 0,
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

    /// Expose the shared CapabilityManager so callers (e.g. the supervisor
    /// during crash recovery) can consult ToolMetadata — specifically
    /// `SideEffectClass` — to decide whether an in-flight tool call is safe
    /// to auto-replay or requires human adjudication (R14-6b).
    pub fn capability_handle(&self) -> Arc<Mutex<CapabilityManager>> {
        Arc::clone(&self.capability)
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
    #[tracing::instrument(
        level = "info",
        skip(self, turn_ctx, cancel_token, stream_tx),
        fields(
            session_id = %turn_ctx.session_id,
            turn_id = %turn_ctx.turn_id,
        )
    )]
    pub async fn run(
        &self,
        turn_ctx: TurnContext,
        cancel_token: CancellationToken,
        stream_tx: Option<tokio::sync::mpsc::UnboundedSender<StreamFragment>>,
    ) -> TurnOutcome {
        let max_steps = self.max_steps;
        let mut steps = Vec::new();
        let mut tools_called = Vec::new();
        // `finished` = the loop exited via a terminal break (natural stop,
        // sampling error, cancel). If false after the loop AND not
        // cancelled, the step budget was exhausted — we then force a
        // wrap-up summary instead of ending the turn silently.
        let mut finished = false;
        // P0-1：无工具 `Stop` 响应的 repair sampling 剩余预算。
        let mut repair_budget = REPAIR_SAMPLING_BUDGET;

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
                self.try_compact(&turn_ctx, &context, stream_tx.as_ref()).await;
            }

            // ── Build request ───────────────────────────────────
            let context = self.chat_state.get_conversation().await;
            let step_id = StepId::new();

            tracing::debug!(
                step_idx = _step_idx,
                context_items = context.len(),
                step_id = %step_id,
                "starting sampling step"
            );

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
                prompt_snapshot_hash: Some(PromptSnapshot::capture(&context, &tool_specs).content_hash),
                instructions: turn_ctx.instructions.clone(),
                context_items: context.clone(),
                tool_specs: tool_specs.clone(), // clone: may need for 413 retry
                tool_choice: ToolChoice::Auto,
                parallel_tool_calls: true,
                reasoning_request: None,
                response_format: None,
                max_output_tokens: Some(16384),
                provider_state_in: None,
            };

            // ── Sample ──────────────────────────────────────────
            // Use streaming if available (clone tx for each step).
            let outcome = match stream_tx {
                Some(ref tx) => self.sampler.sample_streaming(&turn_ctx.model_binding, &request, tx.clone()).await,
                None => self.sampler.sample(&turn_ctx.model_binding, &request).await,
            };

            let elapsed_ms = outcome.elapsed.as_millis() as u64;

            tracing::debug!(
                step_id = %step_id,
                elapsed_ms,
                has_response = outcome.response.is_some(),
                has_error = outcome.error.is_some(),
                "sampling completed"
            );

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
                        // ── 结构化终止判断（StepDisposition）──
                        // 把散落的 if/else 收拢成 classify_step 分类函数，
                        // 终止协议可读、可单测。
                        let disposition = classify_step(&response, repair_budget);

                        // 非工具分支统一 push step result（工具分支在下方
                        // dispatch 段自行 push）。
                        steps.push(StepResult {
                            step_id: StepId::new(),
                            response: Some(response.clone()),
                            error: None,
                            usage: Some(response.usage.clone()),
                            tool_calls: Vec::new(),
                            elapsed_ms,
                        });

                        match disposition {
                            StepDisposition::ContinueForTools => {
                                // 防御：classify_step 返回 ContinueForTools
                                // 但 has_tools=false，说明 items 里有
                                // ToolCall 但 tool_calls() 返回空——
                                // 不应发生，落入 FinalAnswer 兜底。
                                tracing::warn!(
                                    step_id = %step_id,
                                    "classify_step returned ContinueForTools but has_tools=false — treating as final"
                                );
                                finished = true;
                                break;
                            }
                            StepDisposition::Truncated => {
                                // 输出被截断（max_output_tokens）。
                                // 注入 continuation prompt 让模型从断点继续。
                                // NOTE: assistant_text 已在上方 push 到
                                // chat_state，这里只加 continuation user msg。
                                tracing::info!(
                                    step_id = %step_id,
                                    "output truncated (StopReason::Length) — injecting continuation prompt"
                                );
                                self.chat_state.push_user_message(
                                    ContextItem::User {
                                        content: "[System: Your previous response was truncated because it exceeded the output length limit. \
                                         Continue exactly from where you left off. If you were in the middle of a tool call, \
                                         re-issue the complete tool call now.]".into(),
                                        message_id: None,
                                    }
                                ).await;
                                continue;
                            }
                            StepDisposition::Repair => {
                                // 无工具自然 Stop + 非空文本 + 预算未耗尽。
                                // 注入 repair prompt 迫使模型二选一：
                                //   总结收尾 → turn 结束
                                //   调用工具 → turn 继续
                                repair_budget -= 1;
                                tracing::info!(
                                    step_id = %step_id,
                                    repair_budget,
                                    "no-tool natural stop — injecting repair prompt"
                                );
                                self.chat_state.push_user_message(
                                    ContextItem::User {
                                        content: "[System: You stopped without calling a tool. \
                                         If the user's request is fully resolved, give a concise final summary. \
                                         If you were already in the middle of multi-step tool work that you \
                                         started earlier in this turn, continue with the next tool call now. \
                                         IMPORTANT: If your previous message asked the user a question or \
                                         proposed an action requiring their confirmation (e.g. \"要不要我…\" / \
                                         \"shall I…\" / \"do you want me to…\"), you MUST NOT auto-execute \
                                         that proposed action. Wait for the user's response instead. \
                                         Do not merely describe what you would do next — either summarize \
                                         your findings or continue already-started work.]".into(),
                                        message_id: None,
                                    }
                                ).await;
                                continue;
                            }
                            StepDisposition::FinalAnswer => {
                                // 无工具 Stop + 预算耗尽 → 自然结束。
                                finished = true;
                                break;
                            }
                            StepDisposition::Failed => {
                                // ContentFilter / 空文本 / Refusal → 报错结束。
                                // 旧代码这里落入 finished=true 但无任何标记，
                                // 现在显式记录失败原因。
                                tracing::warn!(
                                    step_id = %step_id,
                                    stop_reason = ?response.stop_reason,
                                    "step failed (ContentFilter / empty / Refusal) — ending turn"
                                );
                                finished = true;
                                break;
                            }
                        }
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

                    // R14-6: Check if any tool requires serial execution.
                    // If any tool has ConcurrencyClass::Serial, ALL tools in
                    // this batch are executed sequentially to respect the
                    // exclusivity constraint. Tools without registered
                    // metadata (e.g. MCP tools) default to Serial — their
                    // side effects are unknown, so parallel execution could
                    // race on the same files.
                    let has_serial = {
                        let cap = self.capability.lock().await;
                        tool_calls.iter().any(|(_, _, name, _)| {
                            cap.tool_metadata(name.as_str())
                                .map(|m| m.concurrency_class == grodex_core::tool::ConcurrencyClass::Serial)
                                .unwrap_or(true)
                        })
                    };

                    for (idx, call_id, name, arguments) in &tool_calls {
                        let call_id = *call_id;
                        let name = name.clone();
                        let mut args = arguments.clone();
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
                                duration_ms: None,
                            });
                            continue;
                        }
                        // 验证通过后归一整数値浮点（100.0 → 100），避免执行端反序列化到整数字段失败。
                        coerce_integer_args(&mut args, &input_schema);

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
                                // Fail-closed: if we cannot durably record the
                                // Prepared event, we MUST NOT dispatch the
                                // tool — otherwise the side effect would be
                                // un-audited and unrecoverable on crash.
                                let _ = tx.send(ToolExecResult {
                                    call_id,
                                    name: name.clone(),
                                    result: ContextItem::ToolResult {
                                        call_id,
                                        content: format!(
                                            "Journal write failed (ToolCallPrepared, fail-closed): {e}. \
                                             Tool dispatch aborted — the audit trail must be durable \
                                             before any side effect."
                                        ),
                                        is_error: true,
                                    },
                                    index: CommitSequence::new(idx as u64),
                                    operation_id: Some(op_id_str.clone()),
                                    duration_ms: None,
                                });
                                continue;
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
                                duration_ms: None,
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
                            tool_schema: {
                                let cap = self.capability.lock().await;
                                cap.tool_schema(name.as_str())
                            },
                            max_tool_result_bytes: self.max_tool_result_bytes,
                            blob_store: self.blob_store.clone(),
                            session_id: turn_ctx.session_id.to_string(),
                            tool_timeout_secs: self.tool_timeout_secs,
                        };
                        let op_id_for_result = op_id_str.clone();

                        if has_serial {
                            // R14-6: Serial mode — execute sequentially,
                            // waiting for each tool to finish before
                            // starting the next. Respects ConcurrencyClass::Serial.
                            // T10: measure the full execute_single_tool
                            // wall-clock (permission + sandbox + execute).
                            let t0 = std::time::Instant::now();
                            let result = execute_single_tool(
                                &prepared, runtime.clone(), perm.clone(), sb.clone(),
                                envelope.as_ref(), &exec_ctx,
                            ).await;
                            let duration_ms = t0.elapsed().as_millis() as u64;
                            let _ = tx.send(ToolExecResult {
                                call_id,
                                name: name.clone(),
                                result,
                                index: CommitSequence::new(idx as u64),
                                operation_id: Some(op_id_for_result),
                                duration_ms: Some(duration_ms),
                            });
                        } else {
                            // Parallel mode — spawn and collect via channel.
                            tokio::spawn(async move {
                                // T10: measure duration in the spawned task.
                                let t0 = std::time::Instant::now();
                                let result = execute_single_tool(
                                    &prepared, runtime, perm, sb, envelope.as_ref(), &exec_ctx,
                                )
                                .await;
                                let duration_ms = t0.elapsed().as_millis() as u64;
                                let _ = tx.send(ToolExecResult {
                                    call_id,
                                    name: name.clone(),
                                    result,
                                    index: CommitSequence::new(idx as u64),
                                    operation_id: Some(op_id_for_result),
                                    duration_ms: Some(duration_ms),
                                });
                            });
                        }
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
                    while let Some(mut tr) = result_rx.recv().await {
                        // Offload oversized tool results to a temp file
                        // BEFORE anything else sees the content (journal,
                        // stream, chat_state, evidence) so every consumer
                        // stays consistent and the context is protected
                        // from one huge output (e.g. reading a big file).
                        if self.max_tool_result_bytes > 0 {
                            if let ContextItem::ToolResult {
                                content, is_error, ..
                            } = &mut tr.result
                            {
                                if !*is_error && content.len() > self.max_tool_result_bytes {
                                    let session_id = turn_ctx.session_id.to_string();
                                    let path = if let Some(store) = &self.blob_store {
                                        // Doc 11 §22: store as an owned blob
                                        // (owner = this session's ToolResult
                                        // set) so the blob_refs projection —
                                        // not a temp-dir scan — governs its
                                        // lifetime. Revoke + GC happen at
                                        // session shutdown
                                        // (`release_session_blobs`).
                                        let (_blob_ref, hash) = store
                                            .store_owned(
                                                content.as_bytes().to_vec(),
                                                "text/plain".to_string(),
                                                grodex_tools::BlobOwnerKind::ToolResult,
                                                session_id.clone(),
                                                grodex_tools::BlobRefKind::ToolOutputBody,
                                                None,
                                            )
                                            .await;
                                        // FileBlobStore is content-addressed:
                                        // blob id == content hash.
                                        let p = store.inner().path_of(&hash);
                                        tracing::info!(
                                            bytes = content.len(),
                                            path = %p.display(),
                                            "offloaded oversized tool result to blob store"
                                        );
                                        Some(p)
                                    } else {
                                        offload_large_result(
                                            content, &tr.name, tr.call_id, &session_id,
                                        )
                                        .await
                                    };
                                    if let Some(path) = path {
                                        let orig_len = content.len();
                                        let preview = truncate_utf8(content, 2048);
                                        *content = format!(
                                            "工具结果过大（{orig_len} 字节），完整内容已保存到临时文件：{}\n\
                                             以下为前 2048 字节预览：\n{preview}\n\n\
                                             [预览截断] 如需完整内容，请用 read_artifact 工具读取：path=\"{}\"",
                                            path.display(),
                                            path.display()
                                        );
                                    }
                                }
                            }
                        }

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
                                    tr.duration_ms, // T10: measured at call site
                                    None, // output_truncated
                                )
                                .await
                            {
                                // The side effect has already happened, so we
                                // cannot "undo" it. But the journal is now
                                // missing the durable execution record —
                                // crash recovery would classify this call as
                                // Indeterminate. Mark the result as error so
                                // the model does not trust a success that
                                // cannot be reconstructed, and the subsequent
                                // ToolResultCommitted write (same store) will
                                // fail-closed and abort the Turn.
                                eprintln!(
                                    "[ERROR] rollout write_tool_execution_finished failed: {e} \
                                     — marking result as error (Indeterminate on crash recovery)"
                                );
                                tr.result = ContextItem::ToolResult {
                                    call_id: tr.call_id,
                                    content: format!(
                                        "Tool executed but journal write failed \
                                         (ToolExecutionFinished): {e}. Result marked as \
                                         error — crash recovery will classify this call \
                                         as Indeterminate."
                                    ),
                                    is_error: true,
                                };
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
                                    steps_exhausted: false,
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
                    // ── 413 / context-overflow → force compact + retry once ──
                    // When the proxy/API returns 413 (Request Entity Too Large)
                    // or a context-length error, the request body exceeds the
                    // limit. Force an aggressive compaction and retry once.
                    let is_too_large = outcome.error.as_ref().map_or(false, |e| {
                        e.is_payload_too_large() || e.is_context_length_error()
                    });
                    if is_too_large && _step_idx == 0 {
                        // Only retry on the first step of a turn to avoid
                        // infinite loops.
                        tracing::warn!(
                            "request too large (413/context-overflow) — forcing compaction and retrying"
                        );
                        let context = self.chat_state.get_conversation().await;
                        self.try_compact(&turn_ctx, &context, stream_tx.as_ref()).await;
                        // Re-fetch compacted context and retry.
                        let context = self.chat_state.get_conversation().await;
                        let retry_request = CanonicalModelRequest {
                            request_id: format!("req_retry_{}", step_id),
                            session_id: turn_ctx.session_id,
                            turn_id: turn_ctx.turn_id,
                            step_id,
                            model_binding_id: turn_ctx.model_binding.binding_id,
                            prompt_snapshot_hash: Some(PromptSnapshot::capture(&context, &tool_specs).content_hash),
                            instructions: turn_ctx.instructions.clone(),
                            context_items: context.clone(),
                            tool_specs: tool_specs.clone(),
                            tool_choice: ToolChoice::Auto,
                            parallel_tool_calls: true,
                            reasoning_request: None,
                            response_format: None,
                            max_output_tokens: Some(16384),
                            provider_state_in: None,
                        };
                        let retry_outcome = match stream_tx {
                            Some(ref tx) => self.sampler.sample_streaming(&turn_ctx.model_binding, &retry_request, tx.clone()).await,
                            None => self.sampler.sample(&turn_ctx.model_binding, &retry_request).await,
                        };
                        match retry_outcome.response {
                            Some(ref response) => {
                                // Retry succeeded — process normally.
                                if let Some(ref tx) = stream_tx { let _ = tx; }
                                let reasoning_text = response.items.iter().find_map(|i| match i {
                                    CanonicalResponseItem::ReasoningSummary { content } => Some(content.clone()),
                                    _ => None,
                                }).unwrap_or_default();
                                if !reasoning_text.is_empty() {
                                    self.chat_state.push_reasoning(reasoning_text.clone()).await;
                                }
                                let assistant_text = response.assistant_text().unwrap_or("").to_string();
                                if !assistant_text.is_empty() {
                                    self.chat_state.push_assistant_response(assistant_text.clone()).await;
                                }
                                let tool_calls: Vec<(usize, ToolCallId, String, serde_json::Value)> = response.tool_calls().iter().enumerate().filter_map(|(idx, item)| match item {
                                    CanonicalResponseItem::ToolCall { call_id, name, arguments } => Some((idx, *call_id, name.clone(), arguments.clone())),
                                    _ => None,
                                }).collect();
                                if tool_calls.is_empty() {
                                    steps.push(StepResult {
                                        step_id: StepId::new(),
                                        response: Some(response.clone()),
                                        error: None,
                                        usage: Some(response.usage.clone()),
                                        tool_calls: Vec::new(),
                                        elapsed_ms: retry_outcome.elapsed.as_millis() as u64,
                                    });
                                    finished = true;
                                    break;
                                }
                                // Retry produced tool calls — fall through to
                                // the normal tool dispatch path below by
                                // NOT pushing to steps and NOT breaking.
                                // We need to re-set the outcome for the tool
                                // dispatch code to pick up.
                                // For simplicity, push the step and break —
                                // tool calls from retry are rare.
                                steps.push(StepResult {
                                    step_id: StepId::new(),
                                    response: Some(response.clone()),
                                    error: None,
                                    usage: Some(response.usage.clone()),
                                    tool_calls: Vec::new(),
                                    elapsed_ms: retry_outcome.elapsed.as_millis() as u64,
                                });
                                finished = true;
                                break;
                            }
                            None => {
                                // Retry also failed — report the original error.
                                let err = outcome.error.clone();
                                steps.push(StepResult {
                                    step_id: StepId::new(),
                                    response: None,
                                    error: err,
                                    usage: None,
                                    tool_calls: Vec::new(),
                                    elapsed_ms,
                                });
                                finished = true;
                                break;
                            }
                        }
                    } else {
                        let err = outcome.error.clone();
                        steps.push(StepResult {
                            step_id: StepId::new(),
                            response: None,
                            error: err,
                            usage: None,
                            tool_calls: Vec::new(),
                            elapsed_ms,
                        });
                        finished = true;
                        break;
                    }
                }
            }
        }

        // ── Step-budget wrap-up ──────────────────────────────────────
        // The loop ran all `max_steps` without a terminal break. Instead
        // of silently ending the turn (the "long task stops mid-way"
        // symptom), force one tool-less sample so the model summarizes
        // what it has done so far and what remains.
        let steps_exhausted = !finished && !cancel_token.is_cancelled();
        if steps_exhausted {
            tracing::warn!(
                max_steps,
                turn_id = %turn_ctx.turn_id,
                "step budget exhausted — forcing wrap-up summary"
            );
            let mut wrap_context = self.chat_state.get_conversation().await;
            wrap_context.push(ContextItem::User {
                content: "你已用完本轮最大执行步数，不能再调用任何工具。请简明总结：已完成的工作、当前进展、剩余未完成的部分及建议的下一步。".into(),
                message_id: None,
            });
            let wrap_request = CanonicalModelRequest {
                request_id: format!("req_wrapup_{}", StepId::new()),
                session_id: turn_ctx.session_id,
                turn_id: turn_ctx.turn_id,
                step_id: StepId::new(),
                model_binding_id: turn_ctx.model_binding.binding_id,
                prompt_snapshot_hash: Some(PromptSnapshot::capture(&wrap_context, &[]).content_hash),
                instructions: turn_ctx.instructions.clone(),
                context_items: wrap_context,
                tool_specs: vec![],
                tool_choice: ToolChoice::None,
                parallel_tool_calls: false,
                reasoning_request: None,
                response_format: None,
                max_output_tokens: Some(2048),
                provider_state_in: None,
            };
            let wrap_outcome = match stream_tx {
                Some(ref tx) => {
                    self.sampler
                        .sample_streaming(&turn_ctx.model_binding, &wrap_request, tx.clone())
                        .await
                }
                None => self.sampler.sample(&turn_ctx.model_binding, &wrap_request).await,
            };
            if let Some(resp) = wrap_outcome.response {
                if let Some(t) = resp.assistant_text() {
                    if !t.is_empty() {
                        self.chat_state.push_assistant_response(t.to_string()).await;
                    }
                }
                steps.push(StepResult {
                    step_id: StepId::new(),
                    response: Some(resp.clone()),
                    error: None,
                    usage: Some(resp.usage.clone()),
                    tool_calls: Vec::new(),
                    elapsed_ms: wrap_outcome.elapsed.as_millis() as u64,
                });
            }
        }

        // Take the LAST non-empty assistant text: with multi-step turns
        // the final answer lives in the last step, not the first.
        let final_text = steps
            .iter()
            .rev()
            .find_map(|s| {
                s.response
                    .as_ref()
                    .and_then(|r| r.assistant_text())
                    .filter(|t| !t.is_empty())
            })
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
            steps_exhausted,
        }
    }

    #[tracing::instrument(
        level = "info",
        skip(self, turn_ctx, context),
        fields(turn_id = %turn_ctx.turn_id)
    )]
    async fn try_compact(
        &self,
        turn_ctx: &TurnContext,
        context: &[ContextItem],
        stream_tx: Option<&tokio::sync::mpsc::UnboundedSender<StreamFragment>>,
    ) {
        // UI 提示通道：压缩会额外跑一轮模型调用，没有提示时前端看起来像卡死。
        let emit_status = |phase: &'static str| {
            if let Some(tx) = stream_tx {
                let _ = tx.send(StreamFragment::CompactionStatus { phase: phase.to_string() });
            }
        };
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
            emit_status("started");

            let (sys, user) = CompactionManager::build_compaction_prompt(&plan);
            let compact_req = CanonicalModelRequest {
                request_id: format!("compact_{}", StepId::new()),
                session_id: turn_ctx.session_id,
                turn_id: turn_ctx.turn_id,
                step_id: StepId::new(),
                model_binding_id: turn_ctx.model_binding.binding_id,
                prompt_snapshot_hash: Some(PromptSnapshot::capture(&[ContextItem::User { content: user.clone(), message_id: None }], &[]).content_hash),
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
                        emit_status("finished");
                    } else {
                        emit_status("failed");
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
                    emit_status("failed");
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
                emit_status("failed");
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
                    // serde_json 的 Number 不区分整/浮：expected "integer" 必须接受任何整数形态（i64/u64，
                    // 以及模型偶尔输出的整数値浮点如 100.0）；expected "number" 接受一切数字。
                    // 之前把所有 Number 统一判为 "number"，导致 integer 字段一律报“expected type integer, got number”。
                    let type_ok = match expected_type {
                        "string" => field_val.is_string(),
                        "boolean" => field_val.is_boolean(),
                        "array" => field_val.is_array(),
                        "object" => field_val.is_object(),
                        "null" => field_val.is_null(),
                        "number" => field_val.is_number(),
                        "integer" => match field_val {
                            serde_json::Value::Number(n) => {
                                // 任何数字形态都放行（含真小数如 0.5）：模型对整数字段输出小数属于
                                // 形态漂移而非语义错误，由后续 coerce_integer_args 四舍五入归一，
                                // 避免一次本可容错的调用被打回重试。
                                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(f64::is_finite)
                            }
                            _ => false,
                        },
                        _ => true, // 未知 schema 类型 → 不拦
                    };
                    if !type_ok {
                        let actual_type = match field_val {
                            serde_json::Value::String(_) => "string",
                            serde_json::Value::Number(_) => "number",
                            serde_json::Value::Bool(_) => "boolean",
                            serde_json::Value::Array(_) => "array",
                            serde_json::Value::Object(_) => "object",
                            serde_json::Value::Null => "null",
                        };
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

/// 把模型输出的“整数値浮点”按 schema 归一成整数（如 `100.0 → 100`）。
/// 验证通过后调用：执行端的 `serde_json::from_value` 把 `100.0` 反序列化到整数字段会失败，
/// 在绑定进 PreparedCapabilityCall 之前统一修正，避免深层执行报错。
fn coerce_integer_args(args: &mut serde_json::Value, schema: &serde_json::Value) {
    let (Some(props), Some(args_obj)) = (
        schema.get("properties").and_then(|v| v.as_object()),
        args.as_object_mut(),
    ) else {
        return;
    };
    for (field_name, field_schema) in props {
        let is_integer = field_schema
            .get("type")
            .and_then(|v| v.as_str())
            .is_some_and(|t| t == "integer");
        if !is_integer {
            continue;
        }
        if let Some(serde_json::Value::Number(n)) = args_obj.get(field_name) {
            if n.is_i64() || n.is_u64() {
                continue;
            }
            if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    // 四舍五入归一：整数值浮点（100.0 → 100）与真小数（0.5 → 1）都收敛成整数，
                    // 避免执行端反序列化到整数字段失败。
                    let rounded = f.round();
                    let coerced = if rounded >= 0.0 {
                        (rounded as u64).into()
                    } else {
                        (rounded as i64).into()
                    };
                    args_obj.insert(field_name.clone(), serde_json::Value::Number(coerced));
                }
            }
        }
    }
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
#[tracing::instrument(
    level = "info",
    skip(prepared, runtime, permission, sandbox, envelope, ctx),
    fields(
        tool_name = %prepared.capability_id.canonical_name,
        call_id = %prepared.tool_call_id,
        operation_id = %prepared.operation_id,
    )
)]
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
                    if let Err(e) = writer
                        .write_approval_requested(
                            Some(&call_id.to_string()),
                            Some(&prepared.operation_id.to_string()),
                            &ticket_id,
                            name,
                            args,
                            "model",
                        )
                        .await
                    {
                        // Fail-closed: cannot durably record the approval
                        // request → refuse to proceed. Without this event
                        // in the journal, resume cannot detect the pending
                        // ticket, leaving the user unable to resolve it.
                        return ContextItem::ToolResult {
                            call_id,
                            content: format!(
                                "Journal write failed (ApprovalRequested, fail-closed): {e}. \
                                 Approval flow aborted — the request must be durable \
                                 to survive crash recovery."
                            ),
                            is_error: true,
                        };
                    }
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
                        // Fail-closed: if this write fails, we cannot prove
                        // the lease was granted on resume → refuse to execute.
                        if let Some(ref writer) = ctx.writer {
                            if let Err(e) = writer
                                .write_lease_issued(
                                    &call_id.to_string(),
                                    &ticket_id,
                                    &call_id.to_string(),
                                    Some(300),
                                )
                                .await
                            {
                                return ContextItem::ToolResult {
                                    call_id,
                                    content: format!(
                                        "Journal write failed (LeaseIssued, fail-closed): {e}. \
                                         Refusing to execute without a durable lease record."
                                    ),
                                    is_error: true,
                                };
                            }
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

    // ── Narrow atomic flow (R14-3) ──────────────────────────────
    // When the user approved with narrowed_args, the execution args are
    // REPLACED (not checked for membership). The narrowed args go through
    // the same safety gates as original args:
    //   1. Schema validation (structure must be valid)
    //   2. Policy re-validation (user can't escalate to a more dangerous call)
    //   3. Sandbox check (must pass path/resource validation)
    //   4. Revision already persisted by supervisor's ResolveApproval handler
    //   5. ToolExecutionStarted written above (durable execution marker)
    //   6. Execute with effective_args = narrowed_args
    let effective_args = if let ApprovalResolution::Narrow { narrowed_args } = &lease.resolution {
        // Step 1: Schema-validate the narrowed args.
        if let Some(ref schema) = ctx.tool_schema {
            if let Err(e) = validate_args_against_schema(narrowed_args, &schema) {
                return ContextItem::ToolResult {
                    call_id,
                    content: format!(
                        "Narrow rejected: narrowed_args fail schema validation: {e}"
                    ),
                    is_error: true,
                };
            }
        }

        // Step 2: Policy re-validation — the user's narrowed args must
        // still pass the live policy. This prevents a user from approving
        // a narrow that escalates privileges (e.g. changing path to /etc).
        {
            let mut perm = permission.lock().await;
            match perm.check(call_id, name, narrowed_args, &format!("{name} (narrowed)")) {
                PermissionResult::Allowed => {} // ok
                PermissionResult::Denied { reason } => {
                    return ContextItem::ToolResult {
                        call_id,
                        content: format!(
                            "Narrow rejected: narrowed_args denied by policy: {reason}"
                        ),
                        is_error: true,
                    };
                }
                PermissionResult::ApprovalRequired { .. } => {
                    // The narrowed args themselves require approval.
                    // Fail-closed rather than starting a nested approval loop.
                    return ContextItem::ToolResult {
                        call_id,
                        content: "Narrow rejected: narrowed_args require approval (nested approval not supported)".into(),
                        is_error: true,
                    };
                }
            }
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
    // Fail-closed: if this write fails, a crash after execution could
    // allow replay to re-consume the same lease → duplicate side effect.
    if let Some(ref writer) = ctx.writer {
        if let Err(e) = writer
            .write_lease_consumed(&call_id.to_string(), &call_id.to_string())
            .await
        {
            return ContextItem::ToolResult {
                call_id,
                content: format!(
                    "Journal write failed (LeaseConsumed, fail-closed): {e}. \
                     Refusing to execute — lease consumption must be durable \
                     to prevent duplicate execution on crash recovery."
                ),
                is_error: true,
            };
        }
    }

    // Execute against the bound capability revision, using the prepared
    // operation id (audit/idempotency key). Uses `effective_args` which
    // may be the narrowed version if the user approved a Narrow resolution.
    //
    // T10: wrap rt.execute() in a per-tool timeout. When the timeout
    // fires, the tool future is dropped (cancelling the pending async
    // operation) and an error result is returned to the model so the
    // turn does not hang indefinitely on a single slow tool.
    //
    // T5: after a successful execution, immediately check whether the
    // output exceeds the offload threshold and offload it to the blob
    // store / temp file RIGHT HERE — before the result enters the channel
    // and receiver loop. This avoids the full payload sitting in the
    // channel and the receiver's memory.
    match runtime {
        Some(rt) => {
            tracing::debug!("executing tool");
            let exec_future = rt.execute(effective_args, prepared.operation_id);
            let output = if ctx.tool_timeout_secs > 0 {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(ctx.tool_timeout_secs),
                    exec_future,
                )
                .await
                {
                    Ok(res) => res,
                    Err(_) => {
                        tracing::warn!(
                            timeout_secs = ctx.tool_timeout_secs,
                            tool = %name,
                            "tool execution timed out"
                        );
                        return ContextItem::ToolResult {
                            call_id,
                            content: format!(
                                "Error: tool execution timed out after {}s",
                                ctx.tool_timeout_secs
                            ),
                            is_error: true,
                        };
                    }
                }
            } else {
                exec_future.await
            };
            match output {
                Ok(output) => {
                    tracing::info!("tool executed successfully");
                    let content = output.to_string();
                    // T5: early offload oversized results before they
                    // enter the channel, so the receiver loop and all
                    // downstream consumers only see the small preview +
                    // path reference.
                    let content = early_offload_tool_result(
                        content,
                        ctx.max_tool_result_bytes,
                        ctx.blob_store.as_ref(),
                        &ctx.session_id,
                        &name,
                        call_id,
                    )
                    .await;
                    ContextItem::ToolResult {
                        call_id,
                        content,
                        is_error: false,
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "tool execution failed");
                    ContextItem::ToolResult {
                        call_id,
                        content: format!("Error: {e}"),
                        is_error: true,
                    }
                }
            }
        }
        // Invariant #4 fence at the dispatch site already asserts runtime is
        // Some; this branch is a defensive fallback for non-debug builds.
        None => {
            tracing::error!("tool runtime is None — capability evicted or not registered");
            ContextItem::ToolResult {
                call_id,
                content: format!("Unknown tool: {name}"),
                is_error: true,
            }
        }
    }
}

/// Offload 根目录：按会话隔离子目录 `{tmp}/grodex-tool-results/{session_id}/`，
/// 使文件生命周期与会话生命周期挂钩，避免无主文件永久残留。
fn offload_root(session_id: &str) -> std::path::PathBuf {
    let safe_session: String = session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    std::env::temp_dir()
        .join("grodex-tool-results")
        .join(safe_session)
}

/// Offload 目录保留天数：工具结果可能含敏感数据，不能无限期散落在系统临时目录。
/// 每次进程首次 offload 时按 mtime 清扫超过该期限的其他会话目录（启动兜底清理）。
const OFFLOAD_RETENTION_DAYS: u64 = 7;

/// 清扫过期的会话 offload 目录（mtime 早于保留期限的整目录删除）。
async fn cleanup_stale_offload_dirs() {
    let root = std::env::temp_dir().join("grodex-tool-results");
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return; // 根目录不存在 → 无事可做
    };
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(OFFLOAD_RETENTION_DAYS * 24 * 3600);
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else { continue };
        if !meta.is_dir() {
            continue;
        }
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < cutoff {
            let path = entry.path();
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => tracing::info!("cleaned stale tool-result dir {:?}", path),
                Err(e) => tracing::warn!("failed to clean stale tool-result dir {:?}: {e}", path),
            }
        }
    }
}

/// 进程级一次性清扫闸门：首次 offload 时触发，后续调用零开销。
static OFFLOAD_CLEANUP: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Offload an oversized tool result to a temp file, returning the path.
/// Returns None if the file could not be written — the caller should then
/// keep the original (truncated-in-place) content rather than failing.
async fn offload_large_result(
    content: &str,
    tool_name: &str,
    call_id: grodex_core::id::ToolCallId,
    session_id: &str,
) -> Option<std::path::PathBuf> {
    // 首次 offload：兜底清扫超过保留期的旧会话目录（防磁盘累积与敏感数据残留）。
    OFFLOAD_CLEANUP.get_or_init(cleanup_stale_offload_dirs).await;
    let dir = offload_root(session_id);
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        tracing::warn!("failed to create tool-result offload dir {:?}", dir);
        return None;
    }
    // Sanitize the tool name for use as a filename component.
    let safe_name: String = tool_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = dir.join(format!("{safe_name}_{call_id}.txt"));
    if let Err(e) = tokio::fs::write(&path, content).await {
        tracing::warn!("failed to offload tool result to {:?}: {e}", path);
        return None;
    }
    tracing::info!(
        bytes = content.len(),
        path = %path.display(),
        "offloaded oversized tool result to temp file"
    );
    Some(path)
}

/// T5: early offload an oversized tool result right at the execution site,
/// BEFORE the result enters the channel and receiver loop. This avoids
/// the full payload sitting in the channel/receiver memory — only the
/// small preview + path reference travels downstream.
///
/// When `max_bytes` is 0 or the content fits, returns the content
/// unchanged (no offload). When offloading succeeds, returns a preview +
/// path reference string. When offloading fails (e.g. disk full), returns
/// a truncated preview with a warning — better to give the model a
/// truncated result than to fail the entire turn.
async fn early_offload_tool_result(
    content: String,
    max_bytes: usize,
    blob_store: Option<&Arc<grodex_tools::ManagedBlobStore<grodex_tools::FileBlobStore>>>,
    session_id: &str,
    tool_name: &str,
    call_id: grodex_core::id::ToolCallId,
) -> String {
    if max_bytes == 0 || content.len() <= max_bytes {
        return content;
    }
    let orig_len = content.len();
    let path = if let Some(store) = blob_store {
        let (_blob_ref, hash) = store
            .store_owned(
                content.as_bytes().to_vec(),
                "text/plain".to_string(),
                grodex_tools::BlobOwnerKind::ToolResult,
                session_id.to_string(),
                grodex_tools::BlobRefKind::ToolOutputBody,
                None,
            )
            .await;
        let p = store.inner().path_of(&hash);
        tracing::info!(
            bytes = orig_len,
            path = %p.display(),
            "early-offloaded oversized tool result to blob store"
        );
        Some(p)
    } else {
        offload_large_result(&content, tool_name, call_id, session_id).await
    };
    match path {
        Some(path) => {
            let preview = truncate_utf8(&content, 2048);
            format!(
                "工具结果过大（{orig_len} 字节），完整内容已保存到临时文件：{}\n\
                 以下为前 2048 字节预览：\n{preview}\n\n\
                 [预览截断] 如需完整内容，请用 read_artifact 工具读取：path=\"{}\"",
                path.display(),
                path.display()
            )
        }
        None => {
            // Offload failed — truncate in-place as a fallback.
            let preview = truncate_utf8(&content, 2048);
            format!(
                "工具结果过大（{orig_len} 字节），offload 失败。以下为前 2048 字节预览：\n{preview}\n\n[预览截断]"
            )
        }
    }
}

/// Truncate a string at a UTF-8 char boundary (never splits a multibyte char).
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod schema_validation_tests {
    use super::{coerce_integer_args, validate_args_against_schema};
    use serde_json::json;

    fn read_file_like_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" },
                "ratio": { "type": "number" }
            },
            "required": ["path"]
        })
    }

    #[test]
    fn integer_field_accepts_integer_json_numbers() {
        // 回归：之前所有 Number 被判为 "number"，integer 字段一律报
        // "expected type integer, got number"（read_file limit=100 即此误报）。
        let schema = read_file_like_schema();
        assert!(validate_args_against_schema(&json!({"path": "a.md", "limit": 100}), &schema).is_ok());
        assert!(validate_args_against_schema(&json!({"path": "a.md", "limit": 0}), &schema).is_ok());
        // 模型偶尔以浮点形态输出整数（100.0）也应放行。
        assert!(validate_args_against_schema(&json!({"path": "a.md", "limit": 100.0}), &schema).is_ok());
    }

    #[test]
    fn integer_field_accepts_any_number_form() {
        // 真小数也放行（形态漂移由 coerce_integer_args 四舍五入归一），只拒非数字。
        let schema = read_file_like_schema();
        assert!(validate_args_against_schema(&json!({"path": "a.md", "limit": 10.5}), &schema).is_ok());
        assert!(validate_args_against_schema(&json!({"path": "a.md", "limit": "100"}), &schema).is_err());
    }

    #[test]
    fn coerce_integer_args_rounds_floats_to_integers() {
        let schema = read_file_like_schema();
        let mut args = json!({"path": "a.md", "limit": 0.5, "ratio": 0.3});
        coerce_integer_args(&mut args, &schema);
        // 0.5 四舍五入为 1；非整数字段（ratio）与字符串字段不动。
        assert_eq!(args["limit"], json!(1));
        assert_eq!(args["ratio"], json!(0.3));
        assert_eq!(args["path"], json!("a.md"));
    }

    #[test]
    fn number_field_accepts_any_number_and_required_enforced() {
        let schema = read_file_like_schema();
        assert!(validate_args_against_schema(&json!({"path": "a.md", "ratio": 0.3}), &schema).is_ok());
        assert!(validate_args_against_schema(&json!({"ratio": 0.3}), &schema).is_err()); // missing required "path"
    }
}
