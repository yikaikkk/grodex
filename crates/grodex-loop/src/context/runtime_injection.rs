//! Runtime instruction injection into the Loop (Doc 19 §11.3/§11.4).
//!
//! Bridges [`RuntimeInstructionInjector`](grodex_prompt::RuntimeInstructionInjector)
//! events to `ContextItem::User` synthetic messages at the Step boundary:
//!
//! - the CURRENT Step's leading prefix is never mutated — callers invoke
//!   this only when assembling the NEXT Step;
//! - every drained discovery becomes ONE `User` item carrying a stable
//!   `message_id`, so compaction/resume cannot duplicate it (#6);
//! - invalidations are journal-only and never produce model-visible
//!   items.

use grodex_core::context::ContextItem;
use grodex_prompt::{DrainedInstruction, RuntimeInstructionInjector};

/// Drain the injector and convert discoveries into `author=runtime`
/// synthetic user items for the NEXT Step (Doc 19 §11.4).
///
/// Returns `(new_items, journal_events)`: append `new_items` to the
/// projection before sampling the next Step; persist `journal_events`
/// to the rollout as durable `InstructionDiscovered` records.
pub fn drain_runtime_instructions(
    injector: &mut RuntimeInstructionInjector,
) -> (Vec<ContextItem>, Vec<grodex_prompt::InstructionDiscoveredEvent>) {
    let drained = injector.drain_synthetic_messages();
    let mut items = Vec::new();
    let mut journal = Vec::with_capacity(drained.len());
    for DrainedInstruction {
        event,
        message_id,
        synthetic_message,
    } in drained
    {
        journal.push(event);
        if let Some(content) = synthetic_message {
            items.push(ContextItem::User {
                content,
                message_id: Some(message_id),
            });
        }
    }
    (items, journal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discoveries_become_user_items_with_stable_message_ids() {
        let mut inj = RuntimeInstructionInjector::new();
        inj.queue_discovery(true, "sub", "/repo/sub/AGENTS.md", "h1", "rule text");

        let (items, journal) = drain_runtime_instructions(&mut inj);
        assert_eq!(items.len(), 1);
        assert_eq!(journal.len(), 1);
        match &items[0] {
            ContextItem::User { content, message_id } => {
                assert!(content.contains("rule text"));
                assert!(message_id.as_deref() == Some("runtime-instruction:sub:h1"));
            }
            other => panic!("expected User item, got {other:?}"),
        }
    }

    #[test]
    fn invalidations_journal_only_and_no_duplicate_on_redrain() {
        let mut inj = RuntimeInstructionInjector::new();
        inj.queue_discovery(true, "sub", "/repo/sub/AGENTS.md", "h1", "rule");
        let (items1, _) = drain_runtime_instructions(&mut inj);
        assert_eq!(items1.len(), 1);

        // Same discovery re-queued (resume replay) → no second injection.
        inj.queue_discovery(true, "sub", "/repo/sub/AGENTS.md", "h1", "rule");
        let (items2, _) = drain_runtime_instructions(&mut inj);
        assert!(items2.is_empty(), "dedup via stable message_id");

        // Invalidation produces a journal event but no model item.
        inj.queue_invalidation("sub", "/repo/sub/AGENTS.md", "h1");
        let (items3, journal3) = drain_runtime_instructions(&mut inj);
        assert!(items3.is_empty());
        assert_eq!(journal3.len(), 1);
    }
}
