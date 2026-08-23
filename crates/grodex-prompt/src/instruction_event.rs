//! Runtime instruction discovery events + Loop injection (Doc 19 §11).
//!
//! When the agent enters a previously unscanned subdirectory it may find
//! narrower `AGENTS.md` rules. The protocol:
//!
//! 1. validate workspace trust, path ownership and the source hash;
//! 2. produce an [`InstructionDiscoveredEvent`] durable event;
//! 3. the CURRENT Step keeps its leading prefix untouched;
//! 4. the NEXT Step receives an `author=runtime` synthetic message with
//!    the rule summary + source (this module's injector);
//! 5. the NEXT Turn folds the node into the prompt baseline;
//! 6. removal/changes produce invalidation events.
//!
//! Everything here is fail-closed: untrusted sources never queue.

use serde::{Deserialize, Serialize};

use crate::manifest::Authority;

/// What happened to a runtime instruction (Doc 19 §11.6: removal/change
/// also produce invalidation events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstructionEventKind {
    /// A new rule was discovered and admitted.
    Discovered,
    /// A previously admitted rule was removed or its content changed —
    /// consumers must drop/replace the old node.
    Invalidated,
}

/// One durable runtime-instruction event. Serializable so it can be
/// journaled to the rollout alongside other durable events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionDiscoveredEvent {
    pub instruction_id: String,
    pub kind: InstructionEventKind,
    /// Where the rule came from (file path or URI).
    pub source_uri: String,
    /// SHA-256 of the source content (change detection / invalidation).
    pub source_hash: String,
    /// Short human-readable summary of the rule (what gets injected into
    /// the synthetic message; full content joins the prompt baseline at
    /// the next Turn instead).
    pub summary: String,
    /// Runtime discoveries never outrank managed/system instructions.
    pub authority: Authority,
}

/// Queue + dedup of runtime instruction events, bridging discovery (which
/// can happen mid-Step) and the Loop (which adopts them at Step/Turn
/// boundaries). Acceptance #4: entering a new directory must not change
/// the current Step prefix — events stay queued until drained.
#[derive(Debug, Default)]
pub struct RuntimeInstructionInjector {
    pending: Vec<InstructionDiscoveredEvent>,
    /// message_ids already injected (acceptance #6: no duplicates after
    /// compaction/resume re-injection).
    injected_ids: std::collections::BTreeSet<String>,
}

impl RuntimeInstructionInjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a discovered rule. `workspace_trusted` gates admission —
    /// untrusted repos never inject (acceptance #3). Returns false when
    /// rejected, so callers can surface a diagnostic instead of silently
    /// dropping.
    pub fn queue_discovery(
        &mut self,
        workspace_trusted: bool,
        instruction_id: impl Into<String>,
        source_uri: impl Into<String>,
        source_hash: impl Into<String>,
        summary: impl Into<String>,
    ) -> bool {
        if !workspace_trusted {
            return false;
        }
        self.pending.push(InstructionDiscoveredEvent {
            instruction_id: instruction_id.into(),
            kind: InstructionEventKind::Discovered,
            source_uri: source_uri.into(),
            source_hash: source_hash.into(),
            summary: summary.into(),
            authority: Authority::RUNTIME,
        });
        true
    }

    /// Queue an invalidation (rule removed or content changed). Unlike
    /// discoveries, invalidations are always queued — dropping them would
    /// keep a stale rule alive.
    pub fn queue_invalidation(
        &mut self,
        instruction_id: impl Into<String>,
        source_uri: impl Into<String>,
        source_hash: impl Into<String>,
    ) {
        self.pending.push(InstructionDiscoveredEvent {
            instruction_id: instruction_id.into(),
            kind: InstructionEventKind::Invalidated,
            source_uri: source_uri.into(),
            source_hash: source_hash.into(),
            summary: String::new(),
            authority: Authority::RUNTIME,
        });
    }

    /// Number of queued events not yet drained.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Stable synthetic-message id for one event (also the dedup key).
    pub fn message_id(event: &InstructionDiscoveredEvent) -> String {
        format!("runtime-instruction:{}:{}", event.instruction_id, event.source_hash)
    }

    /// Drain queued events as `author=runtime` synthetic user messages for
    /// the NEXT Step (Doc 19 §11.4). Returns `(message_id, content)` pairs;
    /// the caller wraps them into `ContextItem::User` with the message_id
    /// so compaction/resume cannot re-inject duplicates (#6).
    ///
    /// Invalidations produce NO model-visible message (the model never
    /// needs to be told a rule vanished — the baseline simply drops it),
    /// but they are returned with `content = None` so callers can journal
    /// the durable event.
    pub fn drain_synthetic_messages(&mut self) -> Vec<DrainedInstruction> {
        let events = std::mem::take(&mut self.pending);
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            let message_id = Self::message_id(&event);
            match event.kind {
                InstructionEventKind::Discovered => {
                    // Dedup across drains (resume replay safety).
                    if self.injected_ids.contains(&message_id) {
                        continue;
                    }
                    self.injected_ids.insert(message_id.clone());
                    out.push(DrainedInstruction {
                        event,
                        message_id,
                        synthetic_message: None, // filled below for borrow reasons
                    });
                }
                InstructionEventKind::Invalidated => {
                    // A changed hash re-discovery supersedes the old node;
                    // free the old dedup slot so the new hash can inject.
                    self.injected_ids
                        .retain(|id| !id.starts_with(&format!("runtime-instruction:{}:", event.instruction_id)));
                    out.push(DrainedInstruction {
                        event,
                        message_id,
                        synthetic_message: None,
                    });
                }
            }
        }
        // Render content after the set mutations (avoids borrow clashes).
        for d in out.iter_mut() {
            if d.event.kind == InstructionEventKind::Discovered {
                d.synthetic_message = Some(render_synthetic_message(&d.event));
            }
        }
        out
    }
}

/// One drained event: the durable record + optional model-visible text.
#[derive(Debug, Clone)]
pub struct DrainedInstruction {
    pub event: InstructionDiscoveredEvent,
    pub message_id: String,
    /// `None` for invalidations (journal-only, never shown to the model).
    pub synthetic_message: Option<String>,
}

/// Render the `author=runtime` message body: summary + provenance. The
/// full rule content is NOT inlined — it joins the baseline next Turn;
/// only a compact, attributable summary reaches the Step history.
fn render_synthetic_message(event: &InstructionDiscoveredEvent) -> String {
    format!(
        "<runtime-instruction author=\"runtime\" source=\"{}\" hash=\"{}\">\n{}\n</runtime-instruction>",
        event.source_uri, event.source_hash, event.summary
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_workspace_never_queues() {
        // Acceptance #3: untrusted repo rules are never injected.
        let mut inj = RuntimeInstructionInjector::new();
        let ok = inj.queue_discovery(
            false,
            "proj_subdir_agents",
            "/repo/sub/AGENTS.md",
            "hash1",
            "always run tests",
        );
        assert!(!ok, "untrusted discovery must be rejected");
        assert_eq!(inj.pending_len(), 0);
    }

    #[test]
    fn discovery_queues_and_drains_as_runtime_synthetic_message() {
        let mut inj = RuntimeInstructionInjector::new();
        assert!(inj.queue_discovery(
            true,
            "sub_agents",
            "/repo/sub/AGENTS.md",
            "h1",
            "use pytest in this subtree"
        ));
        assert_eq!(inj.pending_len(), 1, "queued, not applied to current Step");

        let drained = inj.drain_synthetic_messages();
        assert_eq!(drained.len(), 1);
        let text = drained[0].synthetic_message.as_ref().unwrap();
        assert!(text.contains("author=\"runtime\""));
        assert!(text.contains("/repo/sub/AGENTS.md"), "provenance required");
        assert!(text.contains("use pytest"), "summary required");
        assert!(drained[0].message_id.starts_with("runtime-instruction:sub_agents:"));
        assert_eq!(inj.pending_len(), 0);
    }

    #[test]
    fn same_discovery_never_injected_twice() {
        // Acceptance #6: compaction/resume re-drain must not duplicate.
        let mut inj = RuntimeInstructionInjector::new();
        inj.queue_discovery(true, "id1", "/a/AGENTS.md", "h1", "rule one");
        let first = inj.drain_synthetic_messages();
        assert_eq!(first.len(), 1);

        // Replay the same discovery (e.g. resume re-discovers the file).
        inj.queue_discovery(true, "id1", "/a/AGENTS.md", "h1", "rule one");
        let second = inj.drain_synthetic_messages();
        assert!(second.is_empty(), "identical hash already injected");
    }

    #[test]
    fn invalidation_supersedes_and_reinjection_allowed_on_change() {
        let mut inj = RuntimeInstructionInjector::new();
        inj.queue_discovery(true, "id1", "/a/AGENTS.md", "h1", "rule one");
        let _ = inj.drain_synthetic_messages();

        // Content changed → invalidation + new discovery with new hash.
        inj.queue_invalidation("id1", "/a/AGENTS.md", "h1");
        inj.queue_discovery(true, "id1", "/a/AGENTS.md", "h2", "rule one (updated)");
        let drained = inj.drain_synthetic_messages();

        // Invalidation is journal-only (no model message); the new hash
        // injects exactly once.
        let invalidated: Vec<_> = drained
            .iter()
            .filter(|d| d.synthetic_message.is_none())
            .collect();
        assert_eq!(invalidated.len(), 1, "invalidation must be journaled");
        let injected: Vec<_> = drained.iter().filter(|d| d.synthetic_message.is_some()).collect();
        assert_eq!(injected.len(), 1);
        assert!(injected[0].synthetic_message.as_ref().unwrap().contains("updated"));
    }

    #[test]
    fn event_roundtrips_through_serde_for_rollout_journaling() {
        let mut inj = RuntimeInstructionInjector::new();
        inj.queue_discovery(true, "id1", "/a/AGENTS.md", "h1", "rule");
        let drained = inj.drain_synthetic_messages();
        let json = serde_json::to_string(&drained[0].event).unwrap();
        let back: InstructionDiscoveredEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, drained[0].event);
        assert!(json.contains("discovered"));
    }
}
