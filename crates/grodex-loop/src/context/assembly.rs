//! CompactionAssembly — rebuild the context projection after compaction.
//!
//! Following Grok's `build_compacted_history` pattern: assemble the
//! compacted context in canonical order:
//!   [Zone A: system + developer, ..., CompactionSummary, state_capsule?, recent_items]

use crate::context::state_capsule::StateCapsule;
use grodex_core::context::ContextItem;

/// Assembles the compacted context projection.
///
/// Output order (oldest first):
///   1. Preserved items (Zone A: system instructions, developer rules)
///   2. CompactionSummary (the generated summary)
///   3. State capsule (if present, as a User item with <system-reminder>)
///   4. Recent items (kept verbatim)
pub struct CompactionAssembly;

impl CompactionAssembly {
    /// Rebuild the context projection after compaction.
    ///
    /// `preserved`: Zone A items (system + developer) that survive unchanged.
    /// `summary`: The generated compaction summary text.
    /// `state_capsule`: Optional structured state capsule.
    /// `recent`: Items kept verbatim (most recent conversation turns).
    pub fn assemble(
        preserved: Vec<ContextItem>,
        summary: String,
        state_capsule: &StateCapsule,
        recent: Vec<ContextItem>,
    ) -> Vec<ContextItem> {
        let mut result = Vec::new();

        // 1. Zone A: system + developer instructions (preserved verbatim).
        for item in preserved {
            if matches!(
                item,
                ContextItem::System { .. } | ContextItem::Developer { .. }
            ) {
                result.push(item);
            }
        }

        // 2. Compaction summary.
        if !summary.is_empty() {
            // Check for existing summaries and increment window number.
            let window_number = result
                .iter()
                .filter_map(|i| match i {
                    ContextItem::CompactionSummary {
                        window_number: wn, ..
                    } => Some(*wn),
                    _ => None,
                })
                .max()
                .unwrap_or(0)
                + 1;

            result.push(ContextItem::CompactionSummary {
                summary,
                window_number,
            });
        }

        // 3. State capsule (as a synthetic user item so the model sees it).
        let capsule_text = state_capsule.render();
        if !capsule_text.is_empty() {
            result.push(ContextItem::User {
                content: capsule_text,
                message_id: Some("compaction-state-capsule".into()),
            });
        }

        // 4. Recent items (verbatim).
        result.extend(recent);

        result
    }

    /// Validate the assembled projection.
    ///
    /// Check for orphaned ToolResults (ToolResult without preceding ToolCall).
    /// Returns a list of issues found. An empty list = valid.
    pub fn validate(items: &[ContextItem]) -> Vec<String> {
        let mut issues = Vec::new();
        let mut pending_tool_calls = 0i64;

        for item in items {
            match item {
                ContextItem::ToolCall { .. } => pending_tool_calls += 1,
                ContextItem::ToolResult { .. } => {
                    if pending_tool_calls <= 0 {
                        issues.push("orphaned ToolResult found in compacted history".into());
                    }
                    pending_tool_calls -= 1;
                }
                _ => {}
            }
        }

        if pending_tool_calls > 0 {
            issues.push(format!(
                "{pending_tool_calls} ToolCall(s) without ToolResult in compacted history"
            ));
        }

        issues
    }

    /// Estimate tokens for the assembled projection.
    pub fn estimate_tokens(items: &[ContextItem]) -> u64 {
        items.iter().map(|i| i.estimated_tokens() as u64).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_in_correct_order() {
        let preserved = vec![ContextItem::System {
            content: "system prompt".into(),
        }];
        let summary = "summary text".into();
        let capsule = StateCapsule::new();
        let recent = vec![ContextItem::User {
            content: "hello".into(),
            message_id: None,
        }];

        let result = CompactionAssembly::assemble(preserved, summary, &capsule, recent);
        assert_eq!(result.len(), 3); // system + summary + user
        assert!(matches!(result[0], ContextItem::System { .. }));
        assert!(matches!(
            result[1],
            ContextItem::CompactionSummary { .. }
        ));
        assert!(matches!(result[2], ContextItem::User { .. }));
    }

    #[test]
    fn validate_detects_orphaned_tool_results() {
        let items = vec![ContextItem::ToolResult {
            call_id: Default::default(),
            content: "result".into(),
            is_error: false,
        }];
        let issues = CompactionAssembly::validate(&items);
        assert!(!issues.is_empty());
    }

    #[test]
    fn validate_passes_on_valid_history() {
        let items = vec![
            ContextItem::User {
                content: "q".into(),
                message_id: None,
            },
            ContextItem::Assistant {
                content: "a".into(),
            },
        ];
        let issues = CompactionAssembly::validate(&items);
        assert!(issues.is_empty());
    }
}
