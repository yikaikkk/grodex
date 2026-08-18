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
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::durable_subagent::DurableSubAgentSupervisor;
use crate::rollout_writer::RolloutWriter;
use crate::supervisor::ModelConfig;

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
}

impl DelegateTool {
    pub fn new(config: SubAgentConfig) -> Self {
        Self {
            runtime: SubAgentRuntime::InMemory(Arc::new(Mutex::new(
                SubAgentSupervisor::new(config),
            ))),
            actor: None,
            model_config: None,
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
            let response = run_subagent_turn(actor, cfg, &args.task).await;
            let response_text = match response {
                Ok(text) => text,
                Err(e) => {
                    // Mark the task as failed in the supervisor.
                    match &self.runtime {
                        SubAgentRuntime::InMemory(sup) => {
                            sup.lock().await.fail_task(&task_id, &e);
                        }
                        SubAgentRuntime::Durable(sup) => {
                            sup.lock().await.fail_task(&task_id, &e).await;
                        }
                    }
                    format!("Sub-agent '{label}' failed: {e}")
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
            response_text
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

/// Run a single sub-agent sampling turn with the task as the user input.
///
/// Constructs a minimal `CanonicalModelRequest` with just the task
/// as a user message — no tools, no multi-turn history, no streaming.
async fn run_subagent_turn(
    actor: &grodex_sampler::SamplingActor,
    cfg: &ModelConfig,
    task: &str,
) -> Result<String, String> {
    use grodex_core::context::ContextItem;
    use grodex_core::id::{SessionId, StepId, TurnId};
    use grodex_provider::binding::ModelBinding;
    use grodex_provider::canonical_request::{
        CanonicalModelRequest, InstructionBlock, InstructionRole, ToolChoice,
    };

    let binding = ModelBinding::new(
        cfg.provider.clone(),
        1,
        cfg.model.clone(),
        1,
        cfg.wire_protocol,
    );

    let request = CanonicalModelRequest {
        request_id: format!("subagent-{}", SessionId::new()),
        session_id: SessionId::new(),
        turn_id: TurnId::new(),
        step_id: StepId::new(),
        model_binding_id: binding.binding_id.clone(),
        prompt_snapshot_hash: None,
        instructions: vec![InstructionBlock {
            role: InstructionRole::System,
            content: "You are a sub-agent. Complete the task concisely.".into(),
            priority: 0,
        }],
        context_items: vec![ContextItem::User {
            content: task.to_string(),
            message_id: None,
        }],
        tool_specs: vec![],
        tool_choice: ToolChoice::None,
        parallel_tool_calls: false,
        reasoning_request: None,
        response_format: None,
        max_output_tokens: Some(4096),
        provider_state_in: None,
    };

    let outcome = actor.sample(&binding, &request).await;
    match outcome.response {
        Some(resp) => {
            let text = resp.assistant_text().unwrap_or_default().to_string();
            if text.is_empty() {
                Err("sub-agent returned empty response".into())
            } else {
                Ok(text)
            }
        }
        None => {
            let err = outcome
                .error
                .map(|e| format!("{e}"))
                .unwrap_or_else(|| "unknown sampling error".into());
            Err(err)
        }
    }
}
