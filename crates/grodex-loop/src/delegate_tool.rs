//! DelegateTool — spawns a sub-agent to handle a delegated task.
//!
//! Registered as a built-in tool so the model can spawn sub-agents.
//! When a `SamplingActor` is injected (via `with_sampling`), the tool
//! actually runs the sub-agent turn inline: it constructs a minimal
//! `CanonicalModelRequest` from the task description, samples the model,
//! and returns the sub-agent's response. Without an actor it falls back
//! to the legacy "spawned" placeholder.
//!
//! When a `RolloutWriter` is injected (via `with_writer`), the tool uses
//! `DurableSubAgentSupervisor` so spawn/complete are journaled and
//! restorable on crash.

use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata, Tool, ToolRuntime};
use grodex_subagent::context::ContextFork;
use grodex_subagent::supervisor::{SubAgentConfig, SubAgentSupervisor};
use grodex_subagent::task::TaskBudget;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::durable_subagent::DurableSubAgentSupervisor;
use crate::rollout_writer::RolloutWriter;
use crate::supervisor::ModelConfig;


/// Structured sub-agent lifecycle/progress event, forwarded to the
/// frontend so the TUI can render each sub-agent as a collapsible card
/// (instead of loose one-line logs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "phase")]
pub enum SubagentProgress {
    /// Sub-agent started running.
    Started {
        id: String,
        label: String,
        task_preview: String,
    },
    /// One internal execution step (tool call, sampling retry, …).
    Step { id: String, detail: String },
    /// Sub-agent finished (ok=true) or failed (ok=false).
    Finished {
        id: String,
        label: String,
        ok: bool,
        summary: String,
        /// 预算状态（used/max）——None 时前端不显示进度条。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget: Option<SubagentBudgetStatus>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateArgs {
    pub task: String,
    #[serde(default)]
    pub label: Option<String>,
    /// Optional per-call override of the sampling-turn budget
    /// (`[subagent] max_turns`). One turn = one model sample in the
    /// sub-agent loop. Must be within [`DelegateTool`]'s allowed range
    /// (1..=100); out-of-range values are rejected before spawning.
    #[serde(default)]
    pub max_turns: Option<u32>,
}

/// Lower bound for a sub-agent turn budget: a zero-turn loop can never
/// produce a result.
pub const SUBAGENT_MIN_TURNS: u32 = 1;
/// 剩余轮次达到该阈值时注入 synthesis 提示（软截止）：
/// "停止新的宽泛搜索，开始汇总"。
pub const SUBAGENT_SYNTHESIS_REMAINING: u32 = 3;
/// Upper bound for a sub-agent turn budget (config default or per-call).
/// Prevents a single sub-agent from running away over a long session;
/// raise here deliberately if deeper agentic searches are needed.
pub const SUBAGENT_MAX_TURNS: u32 = 100;

/// Resolve the effective sampling-turn budget for one sub-agent run.
///
/// - `configured` comes from `[subagent] max_turns` (clamped into range —
///   a bad config value must never disable the loop silently).
/// - `per_call` is the optional `delegate_task`/`followup_task` override;
///   `None` falls back to `configured`, while an explicit out-of-range
///   value is an error (the model/user supplied a bad argument and
///   should see why rather than getting a silently clamped run).
pub fn resolve_effective_max_turns(
    configured: u32,
    per_call: Option<u32>,
) -> Result<u32, String> {
    let configured = configured.clamp(SUBAGENT_MIN_TURNS, SUBAGENT_MAX_TURNS);
    match per_call {
        None => Ok(configured),
        Some(v) if (SUBAGENT_MIN_TURNS..=SUBAGENT_MAX_TURNS).contains(&v) => Ok(v),
        Some(v) => Err(format!(
            "max_turns={v} out of allowed range {SUBAGENT_MIN_TURNS}..={SUBAGENT_MAX_TURNS}"
        )),
    }
}

/// 预算状态——主 Agent 据此决定续派/缩范围/接管（Doc 12 预算管理）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SubagentBudgetStatus {
    pub max_turns: u32,
    pub used_turns: u32,
    pub remaining_turns: u32,
    /// true = 耗尽预算仍无完整报告（partial report 由 message 承载）。
    pub exhausted_without_full_report: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateOutput {
    pub agent_id: String,
    pub task_id: String,
    pub message: String,
    /// 预算状态。旧路径（无 actor）为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<SubagentBudgetStatus>,
}

/// Sub-agent runtime backing the delegate_task tool.
///
/// If `writer` is set, tasks are journaled via `DurableSubAgentSupervisor`;
/// otherwise a plain in-memory `SubAgentSupervisor` is used.
enum SubAgentRuntime {
    InMemory(Arc<Mutex<SubAgentSupervisor>>),
    Durable(Arc<Mutex<DurableSubAgentSupervisor>>),
}

pub struct DelegateTool {
    runtime: SubAgentRuntime,
    /// Injected to actually run sub-agent sampling turns. If None, the
    /// tool returns a "spawned" placeholder (legacy behavior).
    actor: Option<Arc<grodex_sampler::SamplingActor>>,
    /// Model config for constructing the sub-agent's ModelBinding.
    model_config: Option<ModelConfig>,
    /// Structured progress channel. When set, the tool emits
    /// Started/Step/Finished events so the TUI can render each
    /// sub-agent as a collapsible card instead of a silent block.
    progress_tx: Option<Arc<tokio::sync::mpsc::UnboundedSender<SubagentProgress>>>,
    /// Read-only tools available to sub-agents (name, runtime, schema,
    /// description) — the description is passed through to the sub-agent's
    /// ToolSpec (previously the model saw `description = name` only).
    /// Injected via `with_readonly_tools` — lets a sub-agent actually
    /// inspect files instead of answering blind (which was the main
    /// cause of "empty response" failures on analysis tasks).
    readonly_tools: Vec<(String, Arc<dyn ToolRuntime>, serde_json::Value, String)>,
    /// Number of sub-agents currently executing.
    running_count: Arc<AtomicUsize>,
    /// Total sub-agents spawned in this session (for the session cap).
    spawned_total: Arc<AtomicUsize>,
    /// Max sub-agents allowed to run concurrently.
    max_concurrent: usize,
    /// Max sub-agents allowed per session (guards against runaway
    /// re-spawn loops). Defaults to 4x the concurrent cap.
    max_total: usize,
    /// Protocol tree host — when set, delegate children are registered
    /// into the SAME tree the six collaboration tools operate on, with a
    /// cancellation token (interrupt_agent) and a mailbox drain (send_message
    /// delivery between steps). Fixes the "two disconnected agent trees" gap.
    protocol_host: Option<Arc<crate::protocol_tools::ProtocolToolHost>>,
    /// Shared permission gate. Sub-agent tool calls previously bypassed
    /// the permission pipeline entirely — deny rules had no effect on
    /// delegated work. When set, each sub-agent tool call is checked:
    /// Denied → refused, Ask → refused (a sub-agent cannot prompt for
    /// approval — fail-closed), Allowed → execute.
    permission: Option<Arc<Mutex<grodex_permission::PermissionManager>>>,
    /// Hard deadline for a single sub-agent turn. Configurable via
    /// `[subagent] turn_timeout_secs` in config.toml. On timeout the
    /// child is cancelled and a failed Finished is emitted so the UI
    /// never shows a permanently "running" node.
    turn_timeout: std::time::Duration,
    /// Default sampling-turn budget for one sub-agent run, from
    /// `[subagent] max_turns` via `SubAgentConfig.default_max_turns`.
    /// Previously the run loop hard-capped at 15 and ignored this.
    max_turns: u32,
}

/// Long sub-agent reports are written to a temp file and only a
/// preview + path is returned into the parent context — otherwise
/// aggregating several 16K-token reports overflows the parent window.
const SUBAGENT_INLINE_MAX_BYTES: usize = 8 * 1024;

impl DelegateTool {
    pub fn new(config: SubAgentConfig) -> Self {
        // Read the turn budget BEFORE moving `config` into the supervisor;
        // `default_max_turns` is Copy (u32) so this is cheap.
        let max_turns = config.default_max_turns
            .clamp(SUBAGENT_MIN_TURNS, SUBAGENT_MAX_TURNS);
        Self {
            runtime: SubAgentRuntime::InMemory(Arc::new(Mutex::new(
                SubAgentSupervisor::new(config),
            ))),
            actor: None,
            model_config: None,
            progress_tx: None,
            readonly_tools: Vec::new(),
            running_count: Arc::new(AtomicUsize::new(0)),
            spawned_total: Arc::new(AtomicUsize::new(0)),
            max_concurrent: 4,
            max_total: 16,
            permission: None,
            protocol_host: None,
            turn_timeout: std::time::Duration::from_secs(480),
            max_turns,
        }
    }

    /// Attach the protocol tree host for tree unification.
    pub fn with_protocol_host(mut self, host: Arc<crate::protocol_tools::ProtocolToolHost>) -> Self {
        self.protocol_host = Some(host);
        self
    }

    /// Handle to the durable sub-agent supervisor (when writer-backed),
    /// so the resume path can run `recover_from_journal`.
    pub fn durable_supervisor(&self) -> Option<Arc<Mutex<DurableSubAgentSupervisor>>> {
        match &self.runtime {
            SubAgentRuntime::Durable(sup) => Some(sup.clone()),
            SubAgentRuntime::InMemory(_) => None,
        }
    }

    /// Attach the shared PermissionManager so sub-agent tool calls are
    /// policy-checked (deny rules apply; Ask fails closed — sub-agents
    /// cannot drive the approval round-trip).
    pub fn with_permission(
        mut self,
        permission: Arc<Mutex<grodex_permission::PermissionManager>>,
    ) -> Self {
        self.permission = Some(permission);
        self
    }

    /// Inject a SamplingActor + ModelConfig so the tool can actually
    /// run sub-agent turns instead of just spawning.
    pub fn with_sampling(mut self, actor: Arc<grodex_sampler::SamplingActor>, cfg: ModelConfig) -> Self {
        self.actor = Some(actor);
        self.model_config = Some(cfg);
        self
    }

    /// Inject a RolloutWriter so sub-agent lifecycle is journaled
    /// (spawn/complete/fail events written to the rollout).
    pub fn with_writer(self, writer: RolloutWriter, config: SubAgentConfig) -> Self {
        Self {
            runtime: SubAgentRuntime::Durable(Arc::new(Mutex::new(
                DurableSubAgentSupervisor::new(writer, config),
            ))),
            ..self
        }
    }

    /// Inject a structured progress channel. The tool sends
    /// Started/Step/Finished events so the TUI can render each
    /// sub-agent as a collapsible card.
    pub fn with_progress_sender(
        mut self,
        tx: Arc<tokio::sync::mpsc::UnboundedSender<SubagentProgress>>,
    ) -> Self {
        self.progress_tx = Some(tx);
        self
    }

    /// Set sub-agent caps: max running concurrently and max total per
    /// session. `0` keeps the defaults (4 concurrent / 16 total).
    pub fn with_limits(mut self, max_concurrent: usize, max_total: usize) -> Self {
        if max_concurrent > 0 {
            self.max_concurrent = max_concurrent;
        }
        self.max_total = if max_total > 0 { max_total } else { self.max_concurrent * 4 };
        self
    }

    /// Override the hard deadline for a single sub-agent turn.
    /// Configurable via `[subagent] turn_timeout_secs`.
    pub fn with_turn_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.turn_timeout = timeout;
        self
    }

    /// Inject read-only tools (e.g. `read_file`) that sub-agents may use.
    /// Only tools that are safe to run without an approval round-trip
    /// should be passed here — they bypass the main permission pipeline.
    pub fn with_readonly_tools(
        mut self,
        tools: Vec<(String, Arc<dyn ToolRuntime>, serde_json::Value, String)>,
    ) -> Self {
        self.readonly_tools = tools;
        self
    }

    /// Send a structured progress event if the channel is wired.
    fn notify_progress(&self, ev: SubagentProgress) {
        if let Some(ref tx) = self.progress_tx {
            let _ = tx.send(ev);
        }
    }
}

impl Tool for DelegateTool {
    type Args = DelegateArgs;
    type Output = DelegateOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "delegate_task".into(),
            display_name: "Delegate Task".into(),
            description: "Spawn a sub-agent to handle a bounded task independently. You (the caller) are responsible for estimating task complexity BEFORE delegating and choosing a sufficient max_turns budget - do not use a fixed default mechanically. Before delegating: split broad investigations into independently completable tasks; define a narrow scope with explicit questions; prefer multiple focused sub-agents over one broad task. Write the task instruction with: scope and exclusions, required findings, priority order, and a stop rule requiring a partial report if full completion is impossible. The sub-agent must fit its work into max_turns: it reserves its final turns for synthesis and returns the best available result even if incomplete. Hitting max_turns without a report is a delegation failure - you remain responsible for continuing from any partial result.".into(),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Ask,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The task description for the sub-agent"},
                "label": {"type": "string", "description": "Human-readable label for the sub-agent"},
                "max_turns": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Sampling-turn budget for this sub-agent (one turn = one model sample). Estimate from task breadth, repo size, number of questions, and reporting cost; ALWAYS reserve 2-3 turns for the final report. Suggested: 6-8 symbol/config lookup; 10-15 focused module investigation; 15-22 cross-layer review; 20-35 code changes with tests. For broader work, split into multiple delegations rather than only raising this."}
            },
            "required": ["task"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"},
                "task_id": {"type": "string"},
                "message": {"type": "string"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for DelegateTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: DelegateArgs = serde_json::from_value(args)
            .map_err(|e| GrodexError::ToolExecution(format!("invalid delegate args: {e}")))?;

        // Resolve the sampling-turn budget: config default, optionally
        // overridden per call. Out-of-range per-call values are rejected
        // BEFORE any spawn so the model gets an explicit error instead of
        // a silently truncated run.
        let effective_max_turns = resolve_effective_max_turns(self.max_turns, args.max_turns)
            .map_err(GrodexError::ToolExecution)?;

        let label = args.label.unwrap_or_else(|| "sub-agent".into());

        // ── 0. Enforce sub-agent caps BEFORE spawning ────────────
        // Returning the refusal as tool output (not Err) lets the model
        // adapt: wait, split the work, or do it itself.
        let running_now = self.running_count.load(Ordering::Relaxed);
        if running_now >= self.max_concurrent {
            return Ok(serde_json::json!(DelegateOutput {
                agent_id: String::new(),
                task_id: String::new(),
                message: format!(
                    "[Subagent quota] {running_now} sub-agents are already running (cap {}). This one was NOT scheduled. Wait for a running sub-agent to finish, or do the subtask yourself.",
                    self.max_concurrent
                ),
                budget: None,
            }));
        }
        let spawned_so_far = self.spawned_total.load(Ordering::Relaxed);
        if spawned_so_far >= self.max_total {
            return Ok(serde_json::json!(DelegateOutput {
                agent_id: String::new(),
                task_id: String::new(),
                message: format!(
                    "[Subagent quota] This session already spawned {spawned_so_far} sub-agents (cap {}); no more will be scheduled. Complete the remaining work yourself.",
                    self.max_total
                ),
                budget: None,
            }));
        }

        // ── 1. Spawn the sub-agent task ──────────────────────────
        // Record the same effective budget on the TaskRun metadata so
        // supervisor/protocol views agree with the actual run loop.
        let task_budget = TaskBudget {
            max_turns: Some(effective_max_turns),
            max_duration_secs: Some(self.turn_timeout.as_secs()),
        };
        let (agent_id, task_id) = match &self.runtime {
            SubAgentRuntime::InMemory(sup) => {
                let mut sup = sup.lock().await;
                let root = sup.root_id();
                sup.spawn(root, &label, &args.task, ContextFork::None, Some(task_budget.clone()))
                    .map_err(|e| GrodexError::ToolExecution(format!("cannot spawn: {e}")))?
            }
            SubAgentRuntime::Durable(sup) => {
                let mut sup = sup.lock().await;
                let root = sup.root_id();
                sup.spawn(root, &label, &args.task, ContextFork::None, Some(task_budget.clone()))
                    .await
                    .map_err(|e| GrodexError::ToolExecution(format!("cannot spawn: {e}")))?
            }
        };

        // ── 2. If a SamplingActor is available, actually run the
        //    sub-agent turn. Otherwise return a placeholder.
        let mut budget_status: Option<SubagentBudgetStatus> = None;
        let message = if let (Some(actor), Some(cfg)) = (&self.actor, &self.model_config) {
            let task_id_str = task_id.to_string();
            self.running_count.fetch_add(1, Ordering::Relaxed);
            self.spawned_total.fetch_add(1, Ordering::Relaxed);
            self.notify_progress(SubagentProgress::Started {
                id: task_id_str.clone(),
                label: label.clone(),
                task_preview: truncate_task(&args.task, 80).to_string(),
            });
            // Unified tree: register the child in the protocol tree so
            // list/send/wait/interrupt operate on it, and give the child
            // loop its cancellation token + mailbox drain.
            let child_link = self
                .protocol_host
                .as_ref()
                .and_then(|h| h.attach_delegate_child(&label, &task_id_str).ok());
            let mut controls = child_link.as_ref().map(|link| {
                let host = self.protocol_host.clone().unwrap();
                let agent_id = link.agent_id;
                ChildControls {
                    cancel: link.cancel.clone(),
                    drain_messages: Box::new(move || host.drain_delegate_messages(&agent_id)),
                }
            });

            // Hard overall deadline for a sub-agent turn. Even if the provider
            // stalls or a lock wedges, the sub-agent (and thus the parent turn)
            // MUST terminate: on timeout we cancel the child and emit a failed
            // Finished so the frontend never sees a permanently "running" node.
            let turn_timeout = self.turn_timeout;
            let cancel_on_timeout = child_link.as_ref().map(|link| link.cancel.clone());
            let ran = tokio::time::timeout(
                turn_timeout,
                run_subagent_turn(
                    actor,
                    cfg,
                    &args.task,
                    effective_max_turns,
                    &self.readonly_tools,
                    self.permission.clone(),
                    controls.as_mut(),
                    |detail| {
                        self.notify_progress(SubagentProgress::Step {
                            id: task_id_str.clone(),
                            detail,
                        });
                    },
                ),
            )
            .await;
            self.running_count.fetch_sub(1, Ordering::Relaxed);
            let (response, used_turns): (Result<String, String>, u32) = match ran {
                Ok(pair) => pair,
                Err(_elapsed) => {
                    if let Some(c) = cancel_on_timeout {
                        c.cancel();
                    }
                    (
                        Err(format!(
                            "sub-agent exceeded {}s without producing a final result",
                            turn_timeout.as_secs()
                        )),
                        effective_max_turns,
                    )
                }
            };
            // 预算状态（Doc 12）：主 Agent 据此决定续派/接管/缩范围。
            let exhausted_without_full_report = response.is_err()
                || used_turns >= effective_max_turns
                || response
                    .as_ref()
                    .map(|t| t.starts_with("[partial report"))
                    .unwrap_or(false);
            budget_status = Some(SubagentBudgetStatus {
                max_turns: effective_max_turns,
                used_turns,
                remaining_turns: effective_max_turns.saturating_sub(used_turns),
                exhausted_without_full_report,
            });
            if let Some(link) = &child_link {
                let ok = response.is_ok();
                self.protocol_host
                    .as_ref()
                    .map(|h| h.finish_delegate_child(&link.agent_id, ok));
            }
            let response_text = match response {
                Ok(text) => {
                    self.notify_progress(SubagentProgress::Finished {
                        id: task_id_str,
                        label: label.clone(),
                        ok: true,
                        summary: truncate_task(&text, 100).to_string(),
                        budget: budget_status,
                    });
                    // Long reports go to a temp file so aggregating
                    // multiple sub-agent outputs doesn't blow up the
                    // parent's context window.
                    offload_if_large(text, &label, &task_id.to_string()).await
                }
                Err(e) => {
                    self.notify_progress(SubagentProgress::Finished {
                        id: task_id_str,
                        label: label.clone(),
                        ok: false,
                        summary: e.clone(),
                        budget: budget_status,
                    });
                    // Mark the task as failed in the supervisor.
                    match &self.runtime {
                        SubAgentRuntime::InMemory(sup) => {
                            sup.lock().await.fail_task(&task_id, &e);
                        }
                        SubAgentRuntime::Durable(sup) => {
                            sup.lock().await.fail_task(&task_id, &e).await;
                        }
                    }
                    format!("[Subagent '{label}'] 失败: {e}")
                }
            };

            // Complete the task with the result.
            let tokens: u64 = response_text.len() as u64 / 4; // rough estimate
            match &self.runtime {
                SubAgentRuntime::InMemory(sup) => {
                    sup.lock().await.complete_task(&task_id, response_text.clone(), tokens);
                }
                SubAgentRuntime::Durable(sup) => {
                    sup.lock().await.complete_task(&task_id, response_text.clone(), tokens).await;
                }
            }
            // Prefix subagent output so it doesn't mix with main conversation.
            format!("[Subagent '{label}'] {response_text}")
        } else {
            {
                format!("Sub-agent '{label}' spawned. It will work on: {}", args.task)
            }
        };

        let output = DelegateOutput {
            agent_id: agent_id.to_string(),
            task_id: task_id.to_string(),
            message,
            budget: budget_status,
        };

        serde_json::to_value(output)
            .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

/// Run a sub-agent as a multi-step agentic loop.
///
/// The sub-agent gets the task as a user message plus (optionally) a set
/// of read-only tools. It loops: sample → execute tool calls → feed
/// results back, until it produces a final text answer or hits the step
/// cap. Single-shot sampling (the old behavior) failed on analysis tasks:
/// with `max_output_tokens: 4096` and no tools, long answers got
/// truncated and reasoning-only models returned zero visible text
/// ("empty response").
/// Step-boundary controls handed to the sub-agent loop by DelegateTool:
/// cancellation (interrupt_agent) and parent mailbox delivery (send_message).
pub struct ChildControls {
    pub cancel: tokio_util::sync::CancellationToken,
    pub drain_messages: Box<dyn FnMut() -> Vec<String> + Send>,
}

async fn run_subagent_turn(
    actor: &grodex_sampler::SamplingActor,
    cfg: &ModelConfig,
    task: &str,
    max_turns: u32,
    readonly_tools: &[(String, Arc<dyn ToolRuntime>, serde_json::Value, String)],
    permission: Option<Arc<Mutex<grodex_permission::PermissionManager>>>,
    mut controls: Option<&mut ChildControls>,
    mut on_step: impl FnMut(String),
) -> (Result<String, String>, u32) {
    use grodex_core::context::ContextItem;
    use grodex_core::id::{SessionId, StepId, TurnId};
    use grodex_provider::binding::ModelBinding;
    use grodex_provider::canonical_request::{
        CanonicalModelRequest, InstructionBlock, InstructionRole, ToolChoice, ToolSpec,
    };
    use grodex_provider::canonical_event::CanonicalResponseItem;
    use grodex_provider::prompt_snapshot::PromptSnapshot;


    // The sampling-turn cap arrives via `max_turns` (config default plus
    // optional per-call override), resolved and range-checked by the caller.
    // One iteration below = one model sample = one turn.
    /// Long answers (analysis reports) need headroom; 4096 truncated them.
    const SUBAGENT_MAX_OUTPUT_TOKENS: u64 = 16384;

    let binding = ModelBinding::new(
        cfg.provider.clone(),
        1,
        cfg.model.clone(),
        1,
        cfg.wire_protocol,
    );

    let mut tool_specs: Vec<ToolSpec> = readonly_tools
        .iter()
        .map(|(name, _, schema, description)| ToolSpec {
            name: name.clone(),
            description: description.clone(),
            parameters: schema.clone(),
            required: vec![],
        })
        .collect();
    // Deterministic sort by name — same rationale as
    // TurnCapabilityOverlay::effective_specs: HashMap or caller-provided
    // order may vary, and an unstable tools array defeats provider-side
    // prompt caching.
    tool_specs.sort_by(|a, b| a.name.cmp(&b.name));

    let mut context: Vec<ContextItem> = vec![ContextItem::User {
        content: task.to_string(),
        message_id: None,
    }];
    let mut last_error: Option<String> = None;

    for turn in 0..max_turns as usize {
        // ── Turn-boundary controls (unified tree) ───────────────────
        if let Some(c) = controls.as_deref_mut() {
            if c.cancel.is_cancelled() {
                return (Err("interrupted by user (interrupt_agent)".into()), (turn + 1) as u32);
            }
            // Deliver parent messages queued via send_message: injected as
            // user-role items so the model sees them before the next sample.
            for msg in (c.drain_messages)() {
                if !msg.is_empty() {
                    context.push(ContextItem::User {
                        content: format!("[user message]: {msg}"),
                        message_id: None,
                    });
                }
            }
        }
        on_step(format!("采样轮次 {}/{}", turn + 1, max_turns));

        // ── 软截止 + 末轮硬禁用（Doc 12 预算管理） ──────────────────
        // remaining 含本轮：`remaining == 1` 即最后一轮。
        let remaining = max_turns as usize - turn;
        let final_turn = remaining == 1;
        if remaining as u32 == SUBAGENT_SYNTHESIS_REMAINING && !final_turn {
            context.push(ContextItem::User {
                content: "[System: You have 3 turns remaining. Enter synthesis mode now.                           Do not begin broad new investigations. Gather only evidence strictly                           necessary for the final report. You must return a final or partial                           report before the budget expires.]".into(),
                message_id: None,
            });
        } else if final_turn {
            context.push(ContextItem::User {
                content: "[System: This is your FINAL turn. Do not call tools — tool use is                           disabled for this turn. Return the best available report now. If                           incomplete, include confirmed findings, evidence, unresolved items,                           and continuation instructions.]".into(),
                message_id: None,
            });
        }

        let request = CanonicalModelRequest {
            request_id: format!("subagent-{}", StepId::new()),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            step_id: StepId::new(),
            model_binding_id: binding.binding_id.clone(),
            prompt_snapshot_hash: Some(PromptSnapshot::capture(&context, &tool_specs).content_hash),
            instructions: vec![InstructionBlock {
                role: InstructionRole::System,
                content: format!(
                    "You are a sub-agent with a budget of {max_turns} turns (one turn = one model sample). \
                     Spend early turns on discovery and middle turns on verification. \
                     When {synthesis_remaining} or fewer turns remain, enter synthesis mode: do NOT begin broad new investigations — gather only evidence strictly necessary for the final report. \
                     Your final deliverable must include: a conclusion, confirmed findings with file paths and line numbers, unresolved items, and continuation hints. \
                     Running out of turns without a report is a delegation failure — a partial report is ALWAYS better than nothing.",
                    synthesis_remaining = SUBAGENT_SYNTHESIS_REMAINING,
                ),
                priority: 0,
            }],
            context_items: context.clone(),
            // 最后一轮结构性禁用工具：模型没有任何工具可调，只能输出报告。
            tool_specs: if final_turn { Vec::new() } else { tool_specs.clone() },
            tool_choice: if final_turn || tool_specs.is_empty() {
                ToolChoice::None
            } else {
                ToolChoice::Auto
            },
            parallel_tool_calls: false,
            reasoning_request: Some(grodex_provider::canonical_request::ReasoningRequest {
                effort: None,
                summary: Some("auto".to_string()),
            }),
            response_format: None,
            max_output_tokens: Some(SUBAGENT_MAX_OUTPUT_TOKENS),
            provider_state_in: None,
        };

        // Sampling must be cancellable: when the parent turn is stopped, the
        // in-flight provider request is abandoned promptly instead of blocking
        // the delegate (and the whole turn) until it returns / times out.
        let outcome = if let Some(c) = controls.as_deref_mut() {
            if c.cancel.is_cancelled() {
                return (Err("interrupted by user (stop)".into()), (turn + 1) as u32);
            }
            let cancel = c.cancel.clone();
            tokio::select! {
                _ = cancel.cancelled() => {
                    return (Err("interrupted by user (stop)".into()), (turn + 1) as u32);
                }
                out = actor.sample(&binding, &request) => out,
            }
        } else {
            actor.sample(&binding, &request).await
        };
        let response = match outcome.response {
            Some(r) => r,
            None => {
                let err = outcome
                    .error
                    .map(|e| format!("{e}"))
                    .unwrap_or_else(|| "unknown sampling error".into());
                last_error = Some(err.clone());
                // Transient provider errors: nudge the loop to retry once
                // via a synthetic user item, then give up on the next round.
                context.push(ContextItem::User {
                    content: format!("[sub-agent runtime error, retry] {err}"),
                    message_id: None,
                });
                continue;
            }
        };

        // ── Tool calls → execute and loop ────────────────────────
        let calls: Vec<(grodex_core::id::ToolCallId, String, serde_json::Value)> = response
            .tool_calls()
            .iter()
            .filter_map(|item| match item {
                CanonicalResponseItem::ToolCall { call_id, name, arguments } => {
                    Some((*call_id, name.clone(), arguments.clone()))
                }
                _ => None,
            })
            .collect();

        if !calls.is_empty() {
            context.push(ContextItem::Assistant {
                content: response.assistant_text().unwrap_or("").to_string(),
            });
            for (call_id, name, arguments) in calls {
                on_step(format!("工具 {name} {}", truncate_task(&arguments.to_string(), 60)));
                context.push(ContextItem::ToolCall {
                    call_id,
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
                let runtime = readonly_tools.iter().find(|(n, ..)| *n == name);
                // ── Permission gate (fail-closed) ──────────────────
                // Deny rules from the live policy now apply to delegated
                // work too. Ask cannot be satisfied inside a sub-agent
                // (no approval round-trip) → refuse with a clear message
                // so the model stops asking the tool.
                let blocked: Option<String> = if let Some(ref perm) = permission {
                    let mut pm = perm.lock().await;
                    match pm.check(
                        call_id,
                        &name,
                        &arguments,
                        &format!("{name} {arguments}"),
                    ) {
                        grodex_permission::PermissionResult::Allowed => None,
                        grodex_permission::PermissionResult::Denied { reason } => {
                            Some(format!("permission denied: {reason}"))
                        }
                        grodex_permission::PermissionResult::ApprovalRequired { ticket_id, .. } => {
                            // The sub-agent only ever sees READ-ONLY analysis
                            // tools (read_file/grep/glob/read_artifact), which
                            // don't need interactive approval. Auto-approve the
                            // ticket (so no stray prompt reaches the frontend)
                            // and let the read proceed. Explicit policy Denies
                            // are still honoured above.
                            if runtime.is_some() {
                                pm.resolve(&ticket_id, grodex_core::policy::PolicyDecision::Allow, None);
                                None
                            } else {
                                pm.resolve(&ticket_id, grodex_core::policy::PolicyDecision::Deny, None);
                                Some("this tool requires interactive approval, which a sub-agent cannot perform — the call was refused; finish the task without it".into())
                            }
                        }
                    }
                } else {
                    None
                };
                let (content, is_error) = if let Some(reason) = blocked {
                    (format!("[permission denied] {reason}"), true)
                } else {
                    match runtime {
                        Some((_, rt, ..)) => {
                            let r = rt.execute(arguments, OperationId::new()).await;
                            match r {
                                Ok(v) => {
                                    let text = match v {
                                        serde_json::Value::String(s) => s,
                                        other => other.to_string(),
                                    };
                                    (text, false)
                                }
                                Err(e) => (format!("tool execution failed: {e}"), true),
                            }
                        }
                        None => (format!("unregistered tool: {name}"), true),
                    }
                };
                context.push(ContextItem::ToolResult {
                    call_id,
                    content,
                    is_error,
                    duration_ms: None,
                });
            }
            continue;
        }

        // ── Final answer ────────────────────────────────────────
        let text = response.assistant_text().unwrap_or_default().to_string();
        if !text.is_empty() {
            return (Ok(text), (turn + 1) as u32);
        }

        // Reasoning-only output (thinking models): salvage the reasoning
        // text instead of failing with "empty response".
        let reasoning: String = response
            .items
            .iter()
            .filter_map(|i| match i {
                CanonicalResponseItem::ReasoningSummary { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !reasoning.is_empty() {
            return (Ok(format!("[sub-agent reasoning-only output]\n{reasoning}")), (turn + 1) as u32);
        }

        // Truly empty — nudge once and retry.
        context.push(ContextItem::User {
            content: "[System: You produced no visible output. Respond with your final result text now.]".into(),
            message_id: None,
        });
    }

    // ── 预算耗尽：强制总结采样（最后一次免费机会） ─────────────────
    // 循环正常结束说明模型每一轮都在调工具（或输出为空）。给一次
    // tool_choice=None 的总结机会，把已收集的证据变成 partial report，
    // 而不是直接返回 "hit the max turn cap"。
    if !context.is_empty() {
        context.push(ContextItem::User {
            content: "[System: The turn budget is exhausted. Tool use is disabled.                       Return the best available partial report NOW: confirmed findings,                       evidence, unresolved items, and continuation instructions.]".into(),
            message_id: None,
        });
        let request = CanonicalModelRequest {
            request_id: format!("subagent-final-{}", StepId::new()),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            step_id: StepId::new(),
            model_binding_id: binding.binding_id.clone(),
            prompt_snapshot_hash: Some(PromptSnapshot::capture(&context, &[]).content_hash),
            instructions: vec![InstructionBlock {
                role: InstructionRole::System,
                content: "You are a sub-agent whose budget just ran out. Produce the best                           available partial report from the evidence already gathered. Never                           mention that you ran out of budget; just report findings, evidence,                           unresolved items, and continuation hints.".into(),
                priority: 0,
            }],
            context_items: context.clone(),
            tool_specs: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tool_calls: false,
            reasoning_request: Some(grodex_provider::canonical_request::ReasoningRequest {
                effort: None,
                summary: Some("auto".to_string()),
            }),
            response_format: None,
            max_output_tokens: Some(SUBAGENT_MAX_OUTPUT_TOKENS),
            provider_state_in: None,
        };
        let outcome = actor.sample(&binding, &request).await;
        if let Some(resp) = outcome.response {
            let text = resp.assistant_text().unwrap_or_default().to_string();
            if !text.trim().is_empty() {
                return (
                    Ok(format!(
                        "[partial report — turn budget exhausted ({max_turns} turns used)]\n{text}"
                    )),
                    max_turns,
                );
            }
        }
    }

    // 强制总结也失败：带上已观测的工具摘要，主 Agent 至少知道查过什么。
    (
        Err(last_error.unwrap_or_else(|| {
            format!(
                "sub-agent hit the max turn cap ({max_turns}) without producing a final result"
            )
        })),
        max_turns,
    )
}

/// Write an oversized sub-agent report to a temp file and return a
/// preview + path reference. The caller adds the `[Subagent]` prefix.
/// Mirrors the coordinator's large-tool-result offload. On write
/// failure the original text is kept (fail-open).
async fn offload_if_large(text: String, label: &str, task_id: &str) -> String {
    if text.len() <= SUBAGENT_INLINE_MAX_BYTES {
        return text;
    }
    let dir = std::env::temp_dir().join("grodex-subagent-results");
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return text;
    }
    let safe_label: String = label
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let path = dir.join(format!("{safe_label}_{task_id}.md"));
    if tokio::fs::write(&path, &text).await.is_err() {
        return text;
    }
    let orig_len = text.len();
    let preview = truncate_task(&text, 2048);
    format!(
        "Sub-agent report too large ({orig_len} bytes). Full report saved to: {}\n\
         First 2048 chars preview:\n{preview}\n\n\
         [preview truncated] To read the full report, call the read_file tool with path=\"{}\".",
        path.display(),
        path.display()
    )
}

/// Truncate a task description for progress display, avoiding overly
/// long lines in the TUI info log.
fn truncate_task(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Split at char boundary to avoid panicking on multi-byte chars.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_no_override_returns_config_clamped() {
        // In-range config passes through unchanged.
        assert_eq!(resolve_effective_max_turns(50, None).unwrap(), 50);
    }

    #[test]
    fn resolve_config_below_min_clamps_up() {
        // A bad config (0) must not disable the loop — clamp to 1.
        assert_eq!(resolve_effective_max_turns(0, None).unwrap(), SUBAGENT_MIN_TURNS);
    }

    #[test]
    fn resolve_config_above_max_clamps_down() {
        assert_eq!(resolve_effective_max_turns(200, None).unwrap(), SUBAGENT_MAX_TURNS);
    }

    #[test]
    fn resolve_valid_override_wins() {
        assert_eq!(resolve_effective_max_turns(50, Some(3)).unwrap(), 3);
    }

    #[test]
    fn resolve_override_zero_errors() {
        assert!(resolve_effective_max_turns(50, Some(0)).is_err());
    }

    #[test]
    fn resolve_override_above_max_errors() {
        assert!(resolve_effective_max_turns(50, Some(101)).is_err());
    }

    #[test]
    fn resolve_override_at_boundary_ok() {
        assert_eq!(resolve_effective_max_turns(50, Some(SUBAGENT_MAX_TURNS)).unwrap(), 100);
    }

    #[test]
    fn resolve_override_one_ok() {
        assert_eq!(resolve_effective_max_turns(50, Some(SUBAGENT_MIN_TURNS)).unwrap(), 1);
    }
}
