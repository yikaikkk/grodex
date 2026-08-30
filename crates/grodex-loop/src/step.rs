//! Step types — StepDisposition/classify_step (structured termination
//! judgment), StepResult and TurnOutcome. The live turn loop lives in
//! turn_coordinator.rs; the duplicate StepRunner loop was removed
//! (第三十二轮: it drifted from the coordinator and had no production callers).

use grodex_core::id::{StepId, ToolCallId};
use grodex_provider::canonical_event::{
    CanonicalModelResponse, CanonicalResponseItem, StopReason,
};
use grodex_provider::usage::SettledUsage;

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
    /// Structured reason the turn reached its terminal state:
    /// `final_answer` | `repair_exhausted` | `step_budget_exhausted` |
    /// `cancelled` | `sampling_error` | `tool_error` | `journal_failure`
    /// | `indeterminate_wait`. Journaled in TurnCompleted so the
    /// telemetry projection can answer "why did this turn end?".
    pub termination_reason: &'static str,
    /// Aggregate counters collected across the turn (journaled in
    /// TurnCompleted).
    pub metrics: TurnMetricsSummary,
}

/// Turn-level aggregate counters — the journaled form of the
/// coordinator's in-loop `TurnMetrics`.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct TurnMetricsSummary {
    pub steps: u64,
    pub model_calls: u64,
    pub tool_calls: u64,
    pub retries: u64,
    pub compactions: u64,
    pub cancels: u64,
    /// Repair 提示注入次数（与 retries 分列——repair 是「模型犹豫」，
    /// 不是失败重试）。
    pub repair_injections: u64,
    pub duration_ms: u64,
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
/// 6. stop_reason = Stop / None + 非空文本 + repair_budget > 0 + 本轮已有
///    工具调用 → Repair（mid-flight 兜底）
/// 7. stop_reason = Stop / None + 非空文本 + 其余情况 → FinalAnswer
///
/// R 修复：Repair 增加 `had_tool_work` 守卫。此前任何无工具自然 Stop 都会
/// 触发 repair——纯问答 turn 因此被双倍采样（模型答完后被强制再答一次），
/// 且用户看到重复输出。Repair 的原始目的是兜底「工具工作中途停下」，
/// 没有工具工作就没有 mid-flight 可言。
pub fn classify_step(
    response: &CanonicalModelResponse,
    repair_budget: u8,
    had_tool_work: bool,
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
            } else if repair_budget > 0 && had_tool_work {
                StepDisposition::Repair
            } else {
                StepDisposition::FinalAnswer
            }
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
            provider_request_id: None,
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
        assert_eq!(classify_step(&r, 1, true), StepDisposition::ContinueForTools);
    }

    #[test]
    fn classify_truncated() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "partial output...".into(),
            }],
            Some(StopReason::Length),
        );
        assert_eq!(classify_step(&r, 1, true), StepDisposition::Truncated);
    }

    #[test]
    fn classify_content_filter() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "[filtered]".into(),
            }],
            Some(StopReason::ContentFilter),
        );
        assert_eq!(classify_step(&r, 1, true), StepDisposition::Failed);
    }

    #[test]
    fn classify_empty_text_stop() {
        let r = response(vec![], Some(StopReason::Stop));
        assert_eq!(classify_step(&r, 1, true), StepDisposition::Failed);
    }

    #[test]
    fn classify_repair_budget_remaining() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "I will check the file next.".into(),
            }],
            Some(StopReason::Stop),
        );
        assert_eq!(classify_step(&r, 1, true), StepDisposition::Repair);
    }

    /// R 修复回归：纯问答（本轮还没有任何工具调用）必须直接 FinalAnswer，
    /// 不再触发 repair 双倍采样。
    #[test]
    fn classify_no_tool_work_never_repairs() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "42.".into(),
            }],
            Some(StopReason::Stop),
        );
        assert_eq!(classify_step(&r, 1, false), StepDisposition::FinalAnswer);
    }

    #[test]
    fn classify_final_answer_budget_exhausted() {
        let r = response(
            vec![CanonicalResponseItem::AssistantText {
                content: "Done, here's the result.".into(),
            }],
            Some(StopReason::Stop),
        );
        assert_eq!(classify_step(&r, 0, false), StepDisposition::FinalAnswer);
    }

    #[test]
    fn classify_refusal_item() {
        let r = response(
            vec![CanonicalResponseItem::Refusal {
                content: "I can't help with that.".into(),
            }],
            Some(StopReason::Stop),
        );
        assert_eq!(classify_step(&r, 1, true), StepDisposition::Failed);
    }
}
