//! DelegateTool — spawns a sub-agent to handle a delegated task.
//!
//! Registered as a built-in tool so the model can spawn sub-agents.
//! Uses SubAgentSupervisor for lifecycle management.

use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata, Tool, ToolRuntime};
use grodex_subagent::context::ContextFork;
use grodex_subagent::supervisor::{SubAgentConfig, SubAgentSupervisor};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

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

pub struct DelegateTool {
    supervisor: Arc<Mutex<SubAgentSupervisor>>,
}

impl DelegateTool {
    pub fn new(config: SubAgentConfig) -> Self {
        Self {
            supervisor: Arc::new(Mutex::new(SubAgentSupervisor::new(config))),
        }
    }

    pub fn supervisor(&self) -> Arc<Mutex<SubAgentSupervisor>> {
        self.supervisor.clone()
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
        let root_id = {
            let sup = self.supervisor.lock().await;
            sup.root_id()
        };

        let (agent_id, task_id) = {
            let mut sup = self.supervisor.lock().await;
            sup.spawn(
                root_id,
                &label,
                &args.task,
                ContextFork::None,
                None,
            )
            .map_err(|e| GrodexError::ToolExecution(format!("cannot spawn sub-agent: {e}")))?
        };

        let output = DelegateOutput {
            agent_id: agent_id.to_string(),
            task_id: task_id.to_string(),
            message: format!("Sub-agent '{label}' spawned. It will work on: {}", args.task),
        };

        serde_json::to_value(output)
            .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}
