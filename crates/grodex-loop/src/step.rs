//! StepRunner — build request → sample → process response → execute tools → loop.

use grodex_core::context::ContextItem;
use grodex_core::id::{OperationId, StepId, ToolCallId};
use grodex_core::tool::ToolRuntime;
use grodex_permission::{PermissionManager, PermissionPolicy, PermissionResult};
use grodex_provider::canonical_event::CanonicalResponseItem;
use grodex_provider::canonical_request::{CanonicalModelRequest, ToolChoice};
use grodex_provider::usage::SettledUsage;
use grodex_sampler::SamplingActor;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::context::state_capsule::StateCapsule;
use crate::context::CompactionManager;

use crate::turn::{StepResult, TurnContext};

/// A parsed tool call ready for execution.
#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub call_id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Outcome of a complete Turn.
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub steps: Vec<StepResult>,
    pub final_text: String,
    pub usage: Option<SettledUsage>,
    /// True when the turn ended because the per-turn step budget was
    /// exhausted (a wrap-up summary was forced) instead of a natural
    /// model stop. Lets the supervisor surface a visible notice.
    pub steps_exhausted: bool,
}

/// Executes sampling steps with tool call handling.
pub struct StepRunner {
    sampler: SamplingActor,
    tool_runtimes: HashMap<String, Arc<dyn ToolRuntime>>,
    permission: Mutex<PermissionManager>,
    compaction: CompactionManager,
}

impl StepRunner {
    pub fn new(sampler: SamplingActor) -> Self {
        Self {
            sampler,
            tool_runtimes: HashMap::new(),
            permission: Mutex::new(PermissionManager::new(PermissionPolicy::new())),
            compaction: CompactionManager::new(128_000),
        }
    }

    /// Set the context window for auto-compaction detection.
    pub fn with_context_window(mut self, window: u64) -> Self {
        self.compaction.set_context_window(window);
        self
    }

    pub fn register_tool(&mut self, name: impl Into<String>, runtime: Arc<dyn ToolRuntime>) {
        self.tool_runtimes.insert(name.into(), runtime);
    }

    pub fn with_permission(self, mgr: PermissionManager) -> Self {
        Self {
            permission: Mutex::new(mgr),
            ..self
        }
    }

    /// Run a complete Turn with multi-step tool execution.
    pub async fn run_turn(&mut self, turn_ctx: &TurnContext, initial_context: &[ContextItem]) -> TurnOutcome {
        let mut context = initial_context.to_vec();
        let mut steps = Vec::new();
        let max_steps = 10;
        let mut finished = false;

        for _ in 0..max_steps {
            // ── Compaction check ──────────────────────────────────────
            let current_tokens: u64 = context.iter().map(|i| i.estimated_tokens() as u64).sum();
            if self.compaction.should_compact(current_tokens) || self.compaction.is_overflow(current_tokens) {
                if let Some(plan) = self.compaction.plan_compaction(&context) {
                    let (sys_prompt, user_prompt) = CompactionManager::build_compaction_prompt(&plan);
                    // Build a simple compaction request and sample it.
                    let compact_req = CanonicalModelRequest {
                        request_id: format!("compact_{}", StepId::new()),
                        session_id: turn_ctx.session_id,
                        turn_id: turn_ctx.turn_id,
                        step_id: StepId::new(),
                        model_binding_id: turn_ctx.model_binding.binding_id,
                        prompt_snapshot_hash: None,
                        instructions: vec![grodex_provider::canonical_request::InstructionBlock {
                            role: grodex_provider::canonical_request::InstructionRole::System,
                            content: sys_prompt,
                            priority: 0,
                        }],
                        context_items: vec![ContextItem::User {
                            content: user_prompt,
                            message_id: None,
                        }],
                        tool_specs: Vec::new(),
                        tool_choice: ToolChoice::None,
                        parallel_tool_calls: false,
                        reasoning_request: None,
                        response_format: None,
                        max_output_tokens: Some(4096),
                        provider_state_in: None,
                    };
                    let compact_outcome = self.sampler.sample(&turn_ctx.model_binding, &compact_req).await;
                    if let Some(ref response) = compact_outcome.response {
                        let summary_text = response.assistant_text().unwrap_or("");
                        let result = self.compaction.process_summary(summary_text, &plan);
                        if result.is_effective() {
                            let preserved: Vec<ContextItem> = context.iter()
                                .filter(|i| matches!(i, ContextItem::System { .. } | ContextItem::Developer { .. }))
                                .cloned()
                                .collect();
                            let capsule = StateCapsule::new();
                            context = CompactionManager::rebuild_context(
                                preserved, &result, &capsule, plan.items_to_keep,
                            );
                        } else {
                            self.compaction.suppress();
                        }
                    }
                }
            }

            let result = self.run_step(turn_ctx, &context).await;
            let is_terminal = result.tool_calls.is_empty() || result.error.is_some();
            steps.push(result.clone());

            if let Some(ref response) = result.response {
                if let Some(text) = response.assistant_text() {
                    if !text.is_empty() {
                        context.push(ContextItem::Assistant {
                            content: text.to_string(),
                        });
                    }
                }

                for tc in &result.tool_calls {
                    let tr = self.execute_tool(tc).await;
                    context.push(ContextItem::ToolCall {
                        call_id: tc.call_id,
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    });
                    context.push(tr);
                }
            }

            if is_terminal {
                finished = true;
                break;
            }
        }

        TurnOutcome {
            final_text: steps
                .last()
                .and_then(|s| s.response.as_ref())
                .and_then(|r| r.assistant_text())
                .unwrap_or("")
                .to_string(),
            usage: steps.last().and_then(|s| s.usage.clone()),
            steps,
            steps_exhausted: !finished,
        }
    }

    async fn execute_tool(&self, tc: &ToolCallResult) -> ContextItem {
        let cid = tc.call_id;
        let perm_result = {
            self.permission
                .lock()
                .await
                .check(cid, &tc.name, &tc.arguments, &format!("{} {}", tc.name, tc.arguments))
        };
        match perm_result {
            PermissionResult::Allowed => {}
            PermissionResult::Denied { reason } => {
                return ContextItem::ToolResult {
                    call_id: cid,
                    content: format!("Denied: {reason}"),
                    is_error: true,
                };
            }
            PermissionResult::ApprovalRequired { decision_rx, .. } => {
                // Wait up to 5 minutes for user approval. The previous
                // 5-second timeout was far too aggressive: the TUI needs
                // time to render the approval card, the user needs to
                // read the tool call details, and stdio transport adds
                // backpressure latency. 5 minutes matches the UX of
                // Codex / Claude Code where approvals never time out
                // during normal interactive use.
                match tokio::time::timeout(std::time::Duration::from_secs(300), decision_rx).await {
                    Ok(Ok(decision)) if decision.permits_execution() => {}
                    Ok(Ok(_decision)) => {
                        // User explicitly denied.
                        return ContextItem::ToolResult {
                            call_id: cid,
                            content: "Denied by user".into(),
                            is_error: true,
                        };
                    }
                    Ok(Err(_)) => {
                        // Channel dropped — permission manager shut down.
                        return ContextItem::ToolResult {
                            call_id: cid,
                            content: "Approval channel closed".into(),
                            is_error: true,
                        };
                    }
                    Err(_) => {
                        return ContextItem::ToolResult {
                            call_id: cid,
                            content: "Approval timeout (5 min)".into(),
                            is_error: true,
                        };
                    }
                }
            }
        }

        if let Some(rt) = self.tool_runtimes.get(&tc.name) {
            match rt.execute(tc.arguments.clone(), OperationId::new()).await {
                Ok(output) => ContextItem::ToolResult {
                    call_id: cid,
                    content: output.to_string(),
                    is_error: false,
                },
                Err(e) => ContextItem::ToolResult {
                    call_id: cid,
                    content: format!("Error: {e}"),
                    is_error: true,
                },
            }
        } else {
            ContextItem::ToolResult {
                call_id: cid,
                content: format!("Unknown tool: {}", tc.name),
                is_error: true,
            }
        }
    }

    /// Run one sampling step.
    pub async fn run_step(&self, turn_ctx: &TurnContext, context: &[ContextItem]) -> StepResult {
        let step_id = StepId::new();

        let request = CanonicalModelRequest {
            request_id: format!("req_{step_id}"),
            session_id: turn_ctx.session_id,
            turn_id: turn_ctx.turn_id,
            step_id,
            model_binding_id: turn_ctx.model_binding.binding_id,
            prompt_snapshot_hash: None,
            instructions: turn_ctx.instructions.clone(),
            context_items: context.to_vec(),
            tool_specs: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: true,
            reasoning_request: None,
            response_format: None,
            max_output_tokens: Some(4096),
            provider_state_in: None,
        };

        let outcome = self.sampler.sample(&turn_ctx.model_binding, &request).await;
        let elapsed_ms = outcome.elapsed.as_millis() as u64;

        match outcome.response {
            Some(response) => {
                let tool_calls: Vec<ToolCallResult> = response
                    .tool_calls()
                    .iter()
                    .filter_map(|item| match item {
                        CanonicalResponseItem::ToolCall {
                            call_id,
                            name,
                            arguments,
                        } => Some(ToolCallResult {
                            call_id: *call_id,
                            name: name.clone(),
                            arguments: arguments.clone(),
                        }),
                        _ => None,
                    })
                    .collect();

                StepResult {
                    step_id,
                    response: Some(response.clone()),
                    error: None,
                    usage: Some(response.usage.clone()),
                    elapsed_ms,
                    tool_calls,
                }
            }
            None => StepResult {
                step_id,
                response: None,
                error: outcome.error.clone(),
                usage: None,
                elapsed_ms,
                tool_calls: Vec::new(),
            },
        }
    }
}
