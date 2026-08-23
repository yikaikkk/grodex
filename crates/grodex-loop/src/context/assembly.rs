//! CompactionAssembly — rebuild the context projection after compaction.
//!
//! Following Grok's `build_compacted_history` pattern: assemble the
//! compacted context in canonical order:
//!   [Zone A: system + developer, ..., CompactionSummary, recent_items, state_capsule?]
//!
//! The state capsule is placed LAST because its contents (edited files,
//! active processes, etc.) change every turn. Placing it after the
//! stable prefix (Zone A + summary + recent) maximises prompt-cache
//! hit rate: the static prefix remains identical across steps.

use crate::context::state_capsule::StateCapsule;
use grodex_core::context::ContextItem;

/// Assembles the compacted context projection.
///
/// Output order (oldest first):
///   1. Preserved items (Zone A: system instructions, developer rules)
///   2. CompactionSummary (the generated summary)
///   3. Recent items (kept verbatim)
///   4. State capsule (if present, as a User item with <system-reminder>)
///      — placed last because its dynamic content changes every turn,
///        which would invalidate the cache for everything after it.
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

        // 3. Recent items (verbatim) — placed before capsule so the
        //    stable prefix (Zone A + summary + recent) is cacheable.
        result.extend(recent);

        // 4. State capsule LAST (as a synthetic user item so the model sees it).
        //    Capsule contents (edited_files, active_processes, etc.) change
        //    every turn; placing it at the tail prevents it from invalidating
        //    the cache for the recent conversation items.
        let capsule_text = state_capsule.render();
        if !capsule_text.is_empty() {
            result.push(ContextItem::User {
                content: capsule_text,
                message_id: Some("compaction-state-capsule".into()),
            });
        }

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
        assert_eq!(result.len(), 3); // system + summary + user(recent)
        assert!(matches!(result[0], ContextItem::System { .. }));
        assert!(matches!(
            result[1],
            ContextItem::CompactionSummary { .. }
        ));
        assert!(matches!(result[2], ContextItem::User { .. }));
    }

    #[test]
    fn capsule_placed_after_recent_for_cache_stability() {
        // Capsule must be LAST so its dynamic content doesn't invalidate
        // the cache for the stable prefix (Zone A + summary + recent).
        let mut capsule = StateCapsule::new();
        capsule.add_section("State", "active_processes=3");

        let result = CompactionAssembly::assemble(
            vec![ContextItem::System { content: "sys".into() }],
            "summary".into(),
            &capsule,
            vec![ContextItem::User { content: "recent-msg".into(), message_id: None }],
        );

        // Expected: System → CompactionSummary → User(recent) → User(capsule)
        assert_eq!(result.len(), 4);
        assert!(matches!(result[0], ContextItem::System { .. }));
        assert!(matches!(result[1], ContextItem::CompactionSummary { .. }));
        // result[2] = recent item (no message_id → not capsule)
        match &result[2] {
            ContextItem::User { content, message_id } => {
                assert_eq!(*content, "recent-msg");
                assert!(message_id.is_none(), "recent item must not have capsule message_id");
            }
            other => panic!("expected User(recent), got {other:?}"),
        }
        // result[3] = capsule (has the stable message_id)
        match &result[3] {
            ContextItem::User { content, message_id } => {
                assert!(content.contains("active_processes"));
                assert_eq!(message_id.as_deref(), Some("compaction-state-capsule"));
            }
            other => panic!("expected User(capsule), got {other:?}"),
        }
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
