//! CapabilityPublisher — publishes generation numbers when capabilities change.
//!
//! Each capability source (tools, skills, MCP) has its own generation counter.
//! The publisher bumps these when registrations change, and the StepContext
//! captures a snapshot of the current generation for each sampling step.
//!
//! Following the design doc §10 CapabilityManager pattern:
//!   - Generation is bumped on tool register/unregister, skill discovery, MCP refresh
//!   - StepContext captures the generation at step time
//!   - A stale generation comparison rejects calls after a mid-step refresh
//!
//! ## Unification with CapabilityManager (audit Phase-3 fix)
//! Historically `CapabilityPublisher` (an `AtomicU64` counter set) and
//! `CapabilityManager` (the ring-buffered tool/generation store) were two
//! independent systems with no cross-reference — bumping one did not bump the
//! other, so a Step could capture a publisher snapshot that disagreed with the
//! manager's generation. They now share state: `CapabilityManager` owns a
//! `CapabilityPublisher` and bumps it atomically inside `register_tool` /
//! `unregister_tool`, so the two generation views stay coherent by construction.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Monotonic capability generation tracker.
#[derive(Debug)]
pub struct CapabilityPublisher {
    /// Bumped when built-in or MCP tools are registered/unregistered.
    tool_generation: AtomicU64,
    /// Bumped when skills are discovered or reloaded.
    skill_generation: AtomicU64,
    /// Bumped on any capability change (umbrella counter).
    root_generation: AtomicU64,
}

/// Snapshot of capability generations at a point in time.
#[derive(Debug, Clone, Copy)]
pub struct CapabilitySnapshot {
    pub tool_generation: u64,
    pub skill_generation: u64,
    pub root_generation: u64,
}

impl Default for CapabilityPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl CapabilityPublisher {
    pub fn new() -> Self {
        Self {
            tool_generation: AtomicU64::new(1),
            skill_generation: AtomicU64::new(1),
            root_generation: AtomicU64::new(1),
        }
    }

    /// Bump tool generation (called when tools are registered/unregistered).
    pub fn bump_tools(&self) -> u64 {
        self.root_generation.fetch_add(1, Ordering::Release);
        self.tool_generation.fetch_add(1, Ordering::Release) + 1
    }

    /// Bump skill generation (called when skills are discovered/reloaded).
    pub fn bump_skills(&self) -> u64 {
        self.root_generation.fetch_add(1, Ordering::Release);
        self.skill_generation.fetch_add(1, Ordering::Release) + 1
    }

    /// Capture the current generation snapshot.
    pub fn snapshot(&self) -> CapabilitySnapshot {
        CapabilitySnapshot {
            tool_generation: self.tool_generation.load(Ordering::Acquire),
            skill_generation: self.skill_generation.load(Ordering::Acquire),
            root_generation: self.root_generation.load(Ordering::Acquire),
        }
    }

    /// Check if a snapshot is stale (superseded by a later bump).
    pub fn is_stale(&self, snapshot: &CapabilitySnapshot) -> bool {
        self.root_generation.load(Ordering::Acquire) > snapshot.root_generation
    }
}

/// A `CapabilityPublisher` shared between the manager and any external observer
/// (the ACP event stream, the StepContext, MCP refresh) so a single bump is
/// visible everywhere. The manager wraps its publisher in this handle and hands
/// out clones; this is the integration point that previously did not exist.
#[derive(Debug, Clone)]
pub struct SharedPublisher {
    inner: Arc<CapabilityPublisher>,
}

impl SharedPublisher {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CapabilityPublisher::new()),
        }
    }

    /// Borrow the underlying publisher (for bump/snapshot/is_stale).
    pub fn publisher(&self) -> &CapabilityPublisher {
        &self.inner
    }

    /// Convenience: bump tools through the shared handle.
    pub fn bump_tools(&self) -> u64 {
        self.inner.bump_tools()
    }

    /// Convenience: bump skills through the shared handle.
    pub fn bump_skills(&self) -> u64 {
        self.inner.bump_skills()
    }

    /// Convenience: capture a snapshot.
    pub fn snapshot(&self) -> CapabilitySnapshot {
        self.inner.snapshot()
    }
}

impl Default for SharedPublisher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_manager::CapabilityManager;
    use grodex_core::tool::ToolRuntime;
    use std::sync::Arc;

    #[test]
    fn bump_and_snapshot() {
        let pubr = CapabilityPublisher::new();
        let s1 = pubr.snapshot();
        assert_eq!(s1.tool_generation, 1);

        pubr.bump_tools();
        let s2 = pubr.snapshot();
        assert_eq!(s2.tool_generation, 2);
        assert!(pubr.is_stale(&s1));
        assert!(!pubr.is_stale(&s2));
    }

    #[test]
    fn bump_skills_increments_root() {
        let pubr = CapabilityPublisher::new();
        let s1 = pubr.snapshot();
        pubr.bump_skills();
        let s2 = pubr.snapshot();
        assert!(s2.root_generation > s1.root_generation);
        assert_eq!(s2.skill_generation, 2);
    }

    /// Unification: a SharedPublisher handed to the CapabilityManager must
    /// advance its tool generation when the manager registers a tool, so an
    /// observer snapshotting the publisher before/after sees the bump. This
    /// is the integration the audit flagged as missing ("两套独立系统,无互相
    /// 引用"). The actual plumbing lives in `CapabilityManager::with_publisher`.
    #[test]
    fn shared_publisher_observes_manager_bumps() {
        // Use a zero-runtime mock — we only care about generation coherence.
        struct NoopTool;
        #[async_trait::async_trait]
        impl ToolRuntime for NoopTool {
            async fn execute(
                &self,
                _args: serde_json::Value,
                _id: grodex_core::id::OperationId,
            ) -> Result<serde_json::Value, grodex_core::error::GrodexError> {
                Ok(serde_json::json!({}))
            }
        }

        let shared = SharedPublisher::new();
        let before = shared.snapshot();
        let mut mgr = CapabilityManager::with_publisher(10, shared.clone());
        mgr.register_tool(
            "t".into(),
            Arc::new(NoopTool) as Arc<dyn ToolRuntime>,
            grodex_provider::canonical_request::ToolSpec {
                name: "t".into(),
                description: "t".into(),
                parameters: serde_json::json!({}),
                required: vec![],
            },
        );
        let after = shared.snapshot();
        assert!(after.root_generation > before.root_generation, "manager bump must advance the shared publisher's root_generation");
        assert!(after.tool_generation > before.tool_generation, "manager bump must advance the shared publisher's tool_generation");
    }
}

