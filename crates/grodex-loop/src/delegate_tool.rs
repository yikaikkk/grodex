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
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateArgs {
    pub task: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateOutput {
    pub agent_id: String,
    pub task_id: String,
    pub message: String,
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
    /// Read-only tools available to sub-agents (name, runtime, schema).
    /// Injected via `with_readonly_tools` — lets a sub-agent actually
    /// inspect files instead of answering blind (which was the main
    /// cause of "empty response" failures on analysis tasks).
    readonly_tools: Vec<(String, Arc<dyn ToolRuntime>, serde_json::Value)>,
    /// Number of sub-agents currently executing.
    running_count: Arc<AtomicUsize>,
    /// Total sub-agents spawned in this session (for the session cap).
    spawned_total: Arc<AtomicUsize>,
    /// Max sub-agents allowed to run concurrently.
    max_concurrent: usize,
    /// Max sub-agents allowed per session (guards against runaway
    /// re-spawn loops). Defaults to 4x the concurrent cap.
    max_total: usize,
}

/// Long sub-agent reports are written to a temp file and only a
/// preview + path is returned into the parent context — otherwise
/// aggregating several 16K-token reports overflows the parent window.
const SUBAGENT_INLINE_MAX_BYTES: usize = 8 * 1024;

impl DelegateTool {
    pub fn new(config: SubAgentConfig) -> Self {
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
        }
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

    /// Inject read-only tools (e.g. `read_file`) that sub-agents may use.
    /// Only tools that are safe to run without an approval round-trip
    /// should be passed here — they bypass the main permission pipeline.
    pub fn with_readonly_tools(
        mut self,
        tools: Vec<(String, Arc<dyn ToolRuntime>, serde_json::Value)>,
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
            description: "Spawn a sub-agent to handle a task independently. The sub-agent runs with a fresh context and returns results when complete.".into(),
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
                "label": {"type": "string", "description": "Human-readable label for the sub-agent"}
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
                    "[Subagent 限额] 当前已有 {running_now} 个 subagent 在运行（上限 {}），本次未被调度。请等待已有 subagent 完成后再派发，或自己直接完成该子任务。",
                    self.max_concurrent
                ),
            }));
        }
        let spawned_so_far = self.spawned_total.load(Ordering::Relaxed);
        if spawned_so_far >= self.max_total {
            return Ok(serde_json::json!(DelegateOutput {
                agent_id: String::new(),
                task_id: String::new(),
                message: format!(
                    "[Subagent 限额] 本会话已累计派发 {spawned_so_far} 个 subagent（上限 {}），不再接受新的派发。请自己直接完成剩余工作。",
                    self.max_total
                ),
            }));
        }

        // ── 1. Spawn the sub-agent task ──────────────────────────
        let (agent_id, task_id) = match &self.runtime {
            SubAgentRuntime::InMemory(sup) => {
                let mut sup = sup.lock().await;
                let root = sup.root_id();
                sup.spawn(root, &label, &args.task, ContextFork::None, None)
                    .map_err(|e| GrodexError::ToolExecution(format!("cannot spawn: {e}")))?
            }
            SubAgentRuntime::Durable(sup) => {
                let mut sup = sup.lock().await;
                let root = sup.root_id();
                sup.spawn(root, &label, &args.task, ContextFork::None, None)
                    .await
                    .map_err(|e| GrodexError::ToolExecution(format!("cannot spawn: {e}")))?
            }
        };

        // ── 2. If a SamplingActor is available, actually run the
        //    sub-agent turn. Otherwise return a placeholder.
        let message = if let (Some(actor), Some(cfg)) = (&self.actor, &self.model_config) {
            let task_id_str = task_id.to_string();
            self.running_count.fetch_add(1, Ordering::Relaxed);
            self.spawned_total.fetch_add(1, Ordering::Relaxed);
            self.notify_progress(SubagentProgress::Started {
                id: task_id_str.clone(),
                label: label.clone(),
                task_preview: truncate_task(&args.task, 80).to_string(),
            });
            let response =
                run_subagent_turn(actor, cfg, &args.task, &self.readonly_tools, |detail| {
                    self.notify_progress(SubagentProgress::Step {
                        id: task_id_str.clone(),
                        detail,
                    });
                })
                .await;
            self.running_count.fetch_sub(1, Ordering::Relaxed);
            let response_text = match response {
                Ok(text) => {
                    self.notify_progress(SubagentProgress::Finished {
                        id: task_id_str,
                        label: label.clone(),
                        ok: true,
                        summary: truncate_task(&text, 100).to_string(),
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
            format!("Sub-agent '{label}' spawned. It will work on: {}", args.task)
        };

        let output = DelegateOutput {
            agent_id: agent_id.to_string(),
            task_id: task_id.to_string(),
            message,
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
async fn run_subagent_turn(
    actor: &grodex_sampler::SamplingActor,
    cfg: &ModelConfig,
    task: &str,
    readonly_tools: &[(String, Arc<dyn ToolRuntime>, serde_json::Value)],
    mut on_step: impl FnMut(String),
) -> Result<String, String> {
    use grodex_core::context::ContextItem;
    use grodex_core::id::{SessionId, StepId, TurnId};
    use grodex_provider::binding::ModelBinding;
    use grodex_provider::canonical_request::{
        CanonicalModelRequest, InstructionBlock, InstructionRole, ToolChoice, ToolSpec,
    };
    use grodex_provider::canonical_event::CanonicalResponseItem;
    use grodex_provider::prompt_snapshot::PromptSnapshot;

    /// Hard cap on sub-agent steps — keeps a runaway sub-agent bounded.
    const MAX_SUBAGENT_STEPS: usize = 15;
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
        .map(|(name, _, schema)| ToolSpec {
            name: name.clone(),
            description: name.clone(),
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

    for step in 0..MAX_SUBAGENT_STEPS {
        on_step(format!("采样步骤 {}", step + 1));
        let request = CanonicalModelRequest {
            request_id: format!("subagent-{}", StepId::new()),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            step_id: StepId::new(),
            model_binding_id: binding.binding_id.clone(),
            prompt_snapshot_hash: Some(PromptSnapshot::capture(&context, &tool_specs).content_hash),
            instructions: vec![InstructionBlock {
                role: InstructionRole::System,
                content: "You are a sub-agent. Complete the task thoroughly. \
                          You may use the provided tools to inspect resources. \
                          ALWAYS end with a final textual deliverable — never stop \
                          with only tool calls or silent reasoning.".into(),
                priority: 0,
            }],
            context_items: context.clone(),
            tool_specs: tool_specs.clone(),
            tool_choice: if tool_specs.is_empty() { ToolChoice::None } else { ToolChoice::Auto },
            parallel_tool_calls: false,
            reasoning_request: None,
            response_format: None,
            max_output_tokens: Some(SUBAGENT_MAX_OUTPUT_TOKENS),
            provider_state_in: None,
        };

        let outcome = actor.sample(&binding, &request).await;
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
                    content: format!("[sub-agent 运行时错误，请重试] {err}"),
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
                let runtime = readonly_tools.iter().find(|(n, _, _)| *n == name);
                let (content, is_error) = match runtime {
                    Some((_, rt, _)) => match rt.execute(arguments, OperationId::new()).await {
                        Ok(v) => {
                            let text = match v {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            };
                            (text, false)
                        }
                        Err(e) => (format!("工具执行失败: {e}"), true),
                    },
                    None => (format!("未注册的工具: {name}"), true),
                };
                context.push(ContextItem::ToolResult {
                    call_id,
                    content,
                    is_error,
                });
            }
            continue;
        }

        // ── Final answer ────────────────────────────────────────
        let text = response.assistant_text().unwrap_or_default().to_string();
        if !text.is_empty() {
            return Ok(text);
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
            return Ok(format!("[sub-agent 思考过程输出（无正文）]\n{reasoning}"));
        }

        // Truly empty — nudge once and retry.
        context.push(ContextItem::User {
            content: "你刚才没有输出任何内容。请直接给出最终结果文本。".into(),
            message_id: None,
        });
    }

    Err(last_error.unwrap_or_else(|| {
        format!("sub-agent 达到最大步数 ({MAX_SUBAGENT_STEPS}) 仍未产出最终结果")
    }))
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
        "结果过长（{orig_len} 字节），完整报告已保存到：{}\n\
         以下为前 2048 字符预览：\n{preview}\n\n\
         [预览截断] 如需完整内容，请用 read_file 读取上述文件。",
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
