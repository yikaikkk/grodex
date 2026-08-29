//! StepRunner — build request → sample → process response → execute tools → loop.

use grodex_core::context::ContextItem;
use grodex_core::id::{OperationId, StepId, ToolCallId};
use grodex_core::tool::ToolRuntime;
use grodex_permission::{PermissionManager, PermissionPolicy, PermissionResult};
use grodex_provider::canonical_event::{
    CanonicalModelResponse, CanonicalResponseItem, StopReason,
};
use grodex_provider::canonical_request::{CanonicalModelRequest, ToolChoice};
use grodex_provider::prompt_snapshot::PromptSnapshot;
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

// ── Step disposition — 结构化终止判断 ─────────────────────────────
//
// 把 turn_coordinator 里散落的 if/else 收拢成一个分类函数，终止协议
// 可读、可单测。分类依据：模型 response 的 stop_reason + items 内容 +
// repair 预算。
//
// 注意：这个 enum 不含 Codex 的 ContinueRequested（end_turn=false）和
// Commentary 两支——因为 DeepSeek 的 finish_reason 给不出这两个信号，
// 硬塞进去是死代码。Repair 就是「协议无法区分 phase 时的有界兜底」。

/// 结构化的单步终止判断结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepDisposition {
    /// 有 Tool Call → 走 dispatch。
    ContinueForTools,
    /// Length → 注入 continuation prompt 继续采样。
    Truncated,
    /// 无工具 Stop + 非空文本 + 预算未耗尽 → 注入 repair prompt 再采样。
    Repair,
    /// 无工具 Stop + 预算耗尽 → 结束 turn。
    FinalAnswer,
    /// ContentFilter / 空文本 / Refusal → 报错结束。
    Failed,
}

/// 把一个完成的模型 response 分类为 StepDisposition。
///
/// 分类逻辑：
/// 1. items 里有 Refusal → Failed（不依赖 stop_reason，模型拒绝即失败）
/// 2. 有 tool call → ContinueForTools（优先级最高，即使被截断也先 dispatch）
/// 3. stop_reason = Length → Truncated
/// 4. stop_reason = ContentFilter → Failed
/// 5. stop_reason = Stop / None + 空文本 → Failed
/// 6. stop_reason = Stop / None + 非空文本 + repair_budget > 0 → Repair
/// 7. stop_reason = Stop / None + 非空文本 + repair_budget = 0 → FinalAnswer
pub fn classify_step(
    response: &CanonicalModelResponse,
    repair_budget: u8,
) -> StepDisposition {
    // 1. 检查 items 里有没有 Refusal（不依赖 stop_reason）
    let has_refusal = response
        .items
        .iter()
        .any(|i| matches!(i, CanonicalResponseItem::Refusal { .. }));
    if has_refusal {
        return StepDisposition::Failed;
    }

    // 2. 有 tool call → dispatch
    if !response.tool_calls().is_empty() {
        return StepDisposition::ContinueForTools;
    }

    // 3-7. 按 stop_reason 分类
    match response.stop_reason {
        Some(StopReason::Length) => StepDisposition::Truncated,
        Some(StopReason::ContentFilter) => StepDisposition::Failed,
        Some(StopReason::ToolCalls) => StepDisposition::ContinueForTools, // 防御
        _ => {
            // Stop / None
            if response
                .assistant_text()
                .map_or(true, |t| t.trim().is_empty())
            {
                StepDisposition::Failed // 空响应
            } else if repair_budget > 0 {
                StepDisposition::Repair
            } else {
                StepDisposition::FinalAnswer
            }
        }
    }
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
                        prompt_snapshot_hash: Some(PromptSnapshot::capture(&[ContextItem::User { content: user_prompt.clone(), message_id: None }], &[]).content_hash),
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
            prompt_snapshot_hash: Some(PromptSnapshot::capture(context, &[]).content_hash),
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

// ── Unit tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::id::ToolCallId;
    use grodex_provider::canonical_event::{
        CanonicalModelResponse, CanonicalResponseItem, StopReason,
    };
    use grodex_provider::usage::SettledUsage;

    fn dummy_usage() -> SettledUsage {
        SettledUsage {
            estimated: false,
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 0,
            cost_micro_units: None,
            currency: None,
        }
    }

    fn response(
        items: Vec<CanonicalResponseItem>,
        stop_reason: Option<StopReason>,
    ) -> CanonicalModelResponse {
        CanonicalModelResponse {
            request_id: "test_req".into(),
            items,
            stop_reason,
            usage: dummy_usage(),
        }
    }

    #[test]
    fn classify_has_tool_call() {
        let r = response(
            vec![
                CanonicalResponseItem::AssistantText { content: "I'll check".into() },
                CanonicalResponseItem::ToolCall {
                    call_id: ToolCallId::new(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "/tmp"}),
                },
            ],
            Some(StopReason::ToolCalls),
        );
        assert_eq!(classify_step(&r, 1), StepDisposition::ContinueForTools);
    }

    #[test]
    fn classify_truncated() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "partial output...".into(),
            }],
            Some(StopReason::Length),
        );
        assert_eq!(classify_step(&r, 1), StepDisposition::Truncated);
    }

    #[test]
    fn classify_content_filter() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "[filtered]".into(),
            }],
            Some(StopReason::ContentFilter),
        );
        assert_eq!(classify_step(&r, 1), StepDisposition::Failed);
    }

    #[test]
    fn classify_empty_text_stop() {
        let r = response(vec![], Some(StopReason::Stop));
        assert_eq!(classify_step(&r, 1), StepDisposition::Failed);
    }

    #[test]
    fn classify_repair_budget_remaining() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "I will check the file next.".into(),
            }],
            Some(StopReason::Stop),
        );
        assert_eq!(classify_step(&r, 1), StepDisposition::Repair);
    }

    #[test]
    fn classify_final_answer_budget_exhausted() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "Done, here's the result.".into(),
            }],
            Some(StopReason::Stop),
        );
        assert_eq!(classify_step(&r, 0), StepDisposition::FinalAnswer);
    }

    #[test]
    fn classify_refusal_item() {
        let r = response(
            vec![CanonicalResponseItem::Refusal {
                content: "I can't help with that.".into(),
            }],
            Some(StopReason::Stop),
        );
        assert_eq!(classify_step(&r, 1), StepDisposition::Failed);
    }
}
