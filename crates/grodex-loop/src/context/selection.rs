//! Selection — choose which items to compact vs keep.
//!
//! Following Grok's `select_turns_to_compact()`: walk backward from
//! the most recent items, accumulating "keep" tokens up to the target.
//! Then snap forward past any tool-result runs so the model never
//! sees orphaned tool calls without their results.

use crate::context::types::CompactionPlan;
use grodex_core::context::ContextItem;

/// Select items for compaction.
///
/// Strategy: walk backward from the end, keeping items until the
/// `target_keep_tokens` budget is exhausted. Then snap forward to
/// ensure tool-call/tool-result pairs stay together (the model API
/// rejects orphaned tool results).
///
/// Returns None if there aren't enough compactable tokens to be worthwhile.
pub fn select_items_to_compact(
    items: &[ContextItem],
    target_keep_tokens: u64,
    min_compactable_tokens: u64,
) -> Option<CompactionPlan> {
    if items.is_empty() {
        return None;
    }

    let total_tokens: u64 = items.iter().map(|i| i.estimated_tokens() as u64).sum();

    if total_tokens <= target_keep_tokens + min_compactable_tokens {
        return None; // nothing worth compacting
    }

    let mut keep_count = 0usize;
    let mut keep_tokens = 0u64;

    // Walk backward, counting items to keep.
    for item in items.iter().rev() {
        let item_tokens = item.estimated_tokens() as u64;
        if keep_tokens + item_tokens > target_keep_tokens {
            break;
        }
        keep_tokens += item_tokens;
        keep_count += 1;
    }

    let split_idx = items.len().saturating_sub(keep_count);

    // Snap forward: if split would orphan a ToolResult from its ToolCall,
    // move the split point forward to include the paired items.
    let split_idx = snap_forward_past_tool_runs(items, split_idx);

    if split_idx == 0 {
        return None; // can't compact anything
    }

    let items_to_compact = items[..split_idx].to_vec();
    let items_to_keep = items[split_idx..].to_vec();
    let compact_tokens: u64 = items_to_compact
        .iter()
        .map(|i| i.estimated_tokens() as u64)
        .sum();

    Some(CompactionPlan {
        items_to_compact,
        items_to_keep,
        estimated_tokens_before: total_tokens,
        compact_tokens,
        keep_tokens,
    })
}

/// Snap the split point forward past any tool-result runs.
///
/// If the split would separate a ToolCall from its subsequent ToolResult(s),
/// move the split after the complete tool-call → tool-results block.
/// This prevents the model from seeing orphaned tool results on the next
/// request, which would cause a 400 error.
///
/// Also snaps backward for ReasoningSummary: DeepSeek/Qwen thinking-mode
/// requires the `reasoning_content` to be echoed back alongside its paired
/// assistant message. If the split would leave a ReasoningSummary in the
/// compact region while its Assistant message stays in the keep region,
/// the compactor would discard the reasoning and the next request would
/// trigger a 400 "reasoning_content must be passed back". We move the
/// split point back by one so the ReasoningSummary stays with its Assistant.
fn snap_forward_past_tool_runs(items: &[ContextItem], mut idx: usize) -> usize {
    // If we're splitting right after a ToolCall, include the following
    // ToolResults so they stay paired.
    while idx < items.len() {
        let current = &items[idx];
        if is_tool_result(current) {
            while idx < items.len() && is_tool_result(&items[idx]) {
                idx += 1;
            }
        } else {
            break;
        }
    }

    // Also check if the last item in the compact region is a ToolCall
    // without its results in the keep region.
    if idx > 0 && idx < items.len() {
        let last_compacted = &items[idx - 1];
        let first_kept = &items[idx];
        if is_tool_call(last_compacted) && is_tool_result(first_kept) {
            while idx < items.len() && is_tool_result(&items[idx]) {
                idx += 1;
            }
        }
    }

    // Snap backward: if the last item in the compact region is a
    // ReasoningSummary whose paired Assistant is the first kept item,
    // pull the ReasoningSummary into the keep region to preserve the
    // reasoning_content ↔ assistant message pairing.
    if idx > 0 && idx < items.len() {
        let last_compacted = &items[idx - 1];
        let first_kept = &items[idx];
        if is_reasoning_summary(last_compacted) && is_assistant(first_kept) {
            idx -= 1;
        }
    }

    idx.min(items.len())
}

// Helper functions for tool call/result detection.
fn is_tool_call(item: &ContextItem) -> bool {
    matches!(item, ContextItem::ToolCall { .. })
}

fn is_tool_result(item: &ContextItem) -> bool {
    matches!(item, ContextItem::ToolResult { .. })
}

fn is_reasoning_summary(item: &ContextItem) -> bool {
    matches!(item, ContextItem::ReasoningSummary { .. })
}

fn is_assistant(item: &ContextItem) -> bool {
    matches!(item, ContextItem::Assistant { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::id::ToolCallId;

    fn make_user(text: &str) -> ContextItem {
        ContextItem::User {
            content: text.into(),
            message_id: None,
        }
    }

    fn make_assistant(text: &str) -> ContextItem {
        ContextItem::Assistant {
            content: text.into(),
        }
    }

    fn make_tool_call(name: &str) -> ContextItem {
        ContextItem::ToolCall {
            call_id: ToolCallId::new(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    fn make_tool_result(text: &str) -> ContextItem {
        ContextItem::ToolResult {
            call_id: ToolCallId::new(),
            content: text.into(),
            is_error: false,
        }
    }

    #[test]
    fn selects_old_items_for_compaction() {
        let items: Vec<ContextItem> = (0..50)
            .map(|i| make_user(&format!("this is a much longer message number {i} with extra text padding for tokens")))
            .collect();

        let plan = select_items_to_compact(&items, 50, 20).unwrap();
        assert!(plan.items_to_compact.len() > 0);
        assert!(plan.items_to_keep.len() > 0);
        assert_eq!(
            plan.items_to_compact.len() + plan.items_to_keep.len(),
            items.len()
        );
    }

    #[test]
    fn nothing_to_compact_when_below_minimum() {
        let items = vec![make_user("short")];
        let plan = select_items_to_compact(&items, 1000, 50);
        assert!(plan.is_none());
    }

    #[test]
    fn snap_forward_preserves_tool_runs() {
        let items = vec![
            make_user("q1"),
            make_assistant("a1"),
            make_tool_call("read"),
            make_tool_result("file content"),
            make_assistant("done"),
        ];

        // Very small target_keep to force most items into compact, triggering snap
        let plan = select_items_to_compact(&items, 5, 1).unwrap();
        // The tool_result should NOT be split from its tool_call
        // Either both are in compact, or both are in keep.
        let compact_has_tc = plan
            .items_to_compact
            .iter()
            .any(|i| is_tool_call(i));
        let keep_has_tr = plan.items_to_keep.iter().any(|i| is_tool_result(i));
        // They should not be separated.
        assert!(!(compact_has_tc && keep_has_tr));
    }
}
