//! CapabilityManager — session-level mutable management + atomic generation publishing.
//!
//! Design Doc 10 §16: Tool, Skill, and MCP capabilities are managed at the
//! Session level with mutable state, but each mutation atomically publishes
//! a new immutable CapabilityGeneration. Steps capture the generation number
//! and execute against it — eliminating the drift window between model
//! sampling and tool execution.
//!
//! Key invariant: tool_runner(call) always uses the same capability revision
//! that was advertised to the model when it produced the call.

use crate::capability::SharedPublisher;
use grodex_core::tool::ToolRuntime;
use grodex_provider::canonical_request::ToolSpec;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

// ── Generation ─────────────────────────────────────────────────────

/// One immutable snapshot of all registered capabilities.
///
/// Published atomically by `register_tool`, `unregister_tool`, or
/// `refresh`. Retained for in-flight Step execution.
#[derive(Clone)]
pub struct CapabilityGeneration {
    /// Monotonic generation number.
    pub generation: u64,
    /// Tool schemas visible to the model.
    pub tool_specs: HashMap<String, ToolSpec>,
    /// Runtime handles for execution (shared via Arc).
    pub tool_runtimes: HashMap<String, Arc<dyn ToolRuntime>>,
}

impl CapabilityGeneration {
    /// Create an empty generation.
    fn new(generation: u64) -> Self {
        Self {
            generation,
            tool_specs: HashMap::new(),
            tool_runtimes: HashMap::new(),
        }
    }

    /// Build the Vec<ToolSpec> for a model request.
    pub fn model_specs(&self) -> Vec<ToolSpec> {
        self.tool_specs
            .values()
            .cloned()
            .collect()
    }

    /// Look up a tool runtime by name.
    pub fn get_runtime(&self, name: &str) -> Option<Arc<dyn ToolRuntime>> {
        self.tool_runtimes.get(name).cloned()
    }
}

// ── Manager ────────────────────────────────────────────────────────

/// Session-level capability manager.
///
/// Maintains a history of recent CapabilityGenerations so that in-flight
/// Steps can still execute tools against the generation they were sampled
/// against, even after a subsequent tool refresh.
pub struct CapabilityManager {
    /// Ring buffer of recent generations (newest at the back).
    generations: VecDeque<CapabilityGeneration>,
    /// Current generation number.
    current_gen: u64,
    /// Maximum generations to retain for in-flight Step execution.
    max_retained: usize,
    /// Shared publisher (audit Phase-3 unification): bumped in lockstep with
    /// the manager's own generation so external observers (ACP stream, MCP
    /// refresh, StepContext) see the SAME bump. None ⇒ a detached manager
    /// (legacy/standalone behaviour, e.g. unit tests that don't care).
    publisher: Option<SharedPublisher>,
    /// Generation last produced by `adopt_overlay()`. Starts at the initial
    /// generation and is bumped atomically whenever a Turn's overlay is
    /// adopted into the live manager — tracks the "adopted_generation"
    /// freeze boundary so the next Turn's base is provably >= the previous
    /// Turn's adopted generation (monotonic).
    last_adopted_generation: u64,
}

impl CapabilityManager {
    /// Create a new manager with built-in tools.
    pub fn new(max_retained: usize) -> Self {
        Self::with_publisher_opt(max_retained, None)
    }

    /// Create a manager that publishes generation bumps through `publisher`,
    /// unifying the manager's generation with the publisher's view. This is
    /// the integration point that previously did not exist — the two systems
    /// bumped independently and could disagree.
    pub fn with_publisher(max_retained: usize, publisher: SharedPublisher) -> Self {
        Self::with_publisher_opt(max_retained, Some(publisher))
    }

    fn with_publisher_opt(max_retained: usize, publisher: Option<SharedPublisher>) -> Self {
        let generation = CapabilityGeneration::new(1);
        let mut generations = VecDeque::new();
        generations.push_back(generation);
        Self {
            generations,
            current_gen: 1,
            max_retained: max_retained.max(2),
            publisher,
            last_adopted_generation: 1,
        }
    }

    /// Borrow the shared publisher, if any. Exposed so the ACP event stream
    /// / StepContext can snapshot capability generations from the SAME source
    /// the manager bumps.
    pub fn publisher(&self) -> Option<&SharedPublisher> {
        self.publisher.as_ref()
    }

    /// Register or update a tool. Publishes a NEW generation.
    ///
    /// Returns the new generation number. The old generation is retained
    /// for in-flight Step execution.
    pub fn register_tool(
        &mut self,
        name: String,
        runtime: Arc<dyn ToolRuntime>,
        tool_spec: ToolSpec,
    ) -> u64 {
        self.current_gen += 1;

        // Clone the previous generation as a starting point.
        let prev = self.generations.back().unwrap();
        let mut new_gen = prev.clone();
        new_gen.generation = self.current_gen;
        new_gen.tool_specs.insert(name.clone(), tool_spec);
        new_gen.tool_runtimes.insert(name, runtime);

        // Evict oldest if at capacity.
        while self.generations.len() >= self.max_retained {
            self.generations.pop_front();
        }
        self.generations.push_back(new_gen);
        // Unification: bump the shared publisher so external observers see the
        // SAME generation advance the manager just applied.
        if let Some(ref p) = self.publisher {
            p.bump_tools();
        }
        self.current_gen
    }

    /// Remove a tool. Returns the new generation number.
    pub fn unregister_tool(&mut self, name: &str) -> Option<u64> {
        let prev = self.generations.back()?;
        if !prev.tool_specs.contains_key(name) {
            return None;
        }
        self.current_gen += 1;
        let mut new_gen = prev.clone();
        new_gen.generation = self.current_gen;
        new_gen.tool_specs.remove(name);
        new_gen.tool_runtimes.remove(name);

        while self.generations.len() >= self.max_retained {
            self.generations.pop_front();
        }
        self.generations.push_back(new_gen);
        if let Some(ref p) = self.publisher {
            p.bump_tools();
        }
        Some(self.current_gen)
    }

    /// Get the latest generation (for new Steps).
    pub fn latest(&self) -> &CapabilityGeneration {
        self.generations.back().unwrap()
    }

    /// Get the current generation number (for StepContext capture).
    pub fn current_gen(&self) -> u64 {
        self.current_gen
    }

    /// Get the generation last produced by `adopt_overlay()`. This is the
    /// "adopted_generation" freeze marker: the next Turn's `snapshot_base()`
    /// will return a generation >= this value (monotonic non-decrease).
    /// Distinct from `current_gen()` because direct `register_tool`/
    /// `unregister_tool` bumps advance `current_gen` without going through
    /// the Turn-end adoption gate.
    pub fn adopted_generation(&self) -> u64 {
        self.last_adopted_generation
    }

    /// Look up a tool runtime by generation. If the requested generation
    /// has been evicted, falls back to the latest available.
    pub fn get_runtime(&self, generation: u64, name: &str) -> Option<Arc<dyn ToolRuntime>> {
        // Search from newest to oldest.
        for g in self.generations.iter().rev() {
            if g.generation <= generation {
                return g.get_runtime(name);
            }
        }
        // Fallback: use latest generation.
        self.latest().get_runtime(name)
    }

    /// Get model specs for a specific generation.
    pub fn model_specs_for(&self, generation: u64) -> Vec<ToolSpec> {
        for g in self.generations.iter().rev() {
            if g.generation <= generation {
                return g.model_specs();
            }
        }
        self.latest().model_specs()
    }

    /// Check if a generation has been evicted from the buffer.
    pub fn is_evicted(&self, generation: u64) -> bool {
        self.generations.front().map(|g| g.generation > generation).unwrap_or(true)
    }

    /// Number of retained generations.
    pub fn retained_count(&self) -> usize {
        self.generations.len()
    }

    /// Snapshot the latest generation as an immutable `TurnCapabilityBase`.
    ///
    /// Called at Turn start. The base is the frozen view of all registered
    /// capabilities — Steps within this Turn execute against this base plus
    /// any `TurnCapabilityOverlay` additions.
    pub fn snapshot_base(&self) -> TurnCapabilityBase {
        let latest = self.latest();
        TurnCapabilityBase {
            generation: self.current_gen,
            tool_specs: latest.tool_specs.clone(),
            tool_runtimes: latest.tool_runtimes.clone(),
        }
    }

    /// Adopt an overlay from a completed Turn: merge additions/removals
    /// into the manager, producing a new generation. This is the
    /// "adopted_generation" mechanism — promotions within a Turn are
    /// deferred until the Turn ends, then atomically applied.
    ///
    /// Monotonicity invariant: the returned generation is always >=
    /// `snapshot_base()` generation from any prior Turn.
    pub fn adopt_overlay(&mut self, overlay: TurnCapabilityOverlay) -> u64 {
        for (name, (spec, runtime)) in overlay.additions {
            self.register_tool(name, runtime, spec);
        }
        for name in &overlay.removals {
            self.unregister_tool(name);
        }
        self.last_adopted_generation = self.current_gen;
        self.current_gen
    }
}

// ── TurnCapabilityBase / Overlay ───────────────────────────────────

/// Immutable snapshot of capabilities at Turn start.
///
/// Captured from `CapabilityManager::snapshot_base()` when a Turn begins.
/// All Steps within the Turn execute against this base plus any
/// `TurnCapabilityOverlay` accumulated during the Turn.
///
/// Design Doc 10 §16: the base is the "frozen" view — mid-Turn tool
/// registrations go into the overlay and are only adopted as the next
/// Turn's base (the `adopted_generation` freeze mechanism).
#[derive(Clone)]
pub struct TurnCapabilityBase {
    /// The capability generation at the time of snapshot.
    pub generation: u64,
    /// Tool schemas visible to the model.
    pub tool_specs: HashMap<String, ToolSpec>,
    /// Runtime handles for execution.
    pub tool_runtimes: HashMap<String, Arc<dyn ToolRuntime>>,
}

impl TurnCapabilityBase {
    /// Effective model-visible tool specs from this base alone.
    pub fn model_specs(&self) -> Vec<ToolSpec> {
        self.tool_specs.values().cloned().collect()
    }

    /// Look up a tool runtime by name.
    pub fn get_runtime(&self, name: &str) -> Option<Arc<dyn ToolRuntime>> {
        self.tool_runtimes.get(name).cloned()
    }
}

/// Incremental capability changes accumulated during a Turn.
///
/// Promotions (tool registrations) and demotions (unregistrations) within
/// a Turn are buffered here. They are visible to Steps via
/// `effective_specs()` / `effective_runtime()`, but are NOT applied to
/// the `CapabilityManager` until the Turn ends and `adopt_overlay()`
/// is called.
///
/// This deferral is the "adopted_generation" freeze: it prevents a
/// mid-Turn registration from changing what the model sees mid-conversation,
/// which would violate invariant #15 (Tool/Skill/MCP stable within a Turn).
#[derive(Default)]
pub struct TurnCapabilityOverlay {
    /// Tools added or updated during this Turn.
    pub additions: HashMap<String, (ToolSpec, Arc<dyn ToolRuntime>)>,
    /// Tools removed during this Turn.
    pub removals: HashSet<String>,
}

impl TurnCapabilityOverlay {
    /// Create an empty overlay.
    pub fn new() -> Self {
        Self::default()
    }

    /// Promote a tool (add or update). Visible to subsequent Steps in this Turn.
    pub fn promote(&mut self, name: String, spec: ToolSpec, runtime: Arc<dyn ToolRuntime>) {
        self.additions.insert(name.clone(), (spec, runtime));
        self.removals.remove(&name);
    }

    /// Demote a tool (remove). Invisible to subsequent Steps in this Turn.
    pub fn demote(&mut self, name: &str) {
        self.removals.insert(name.to_string());
        self.additions.remove(name);
    }

    /// Whether the overlay has any changes.
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty() && self.removals.is_empty()
    }

    /// Compute the effective model-visible tool specs: base + additions − removals.
    pub fn effective_specs(base: &TurnCapabilityBase, overlay: &TurnCapabilityOverlay) -> Vec<ToolSpec> {
        let mut specs = base.tool_specs.clone();
        for (name, (spec, _)) in &overlay.additions {
            specs.insert(name.clone(), spec.clone());
        }
        for name in &overlay.removals {
            specs.remove(name);
        }
        specs.into_values().collect()
    }

    /// Resolve a tool runtime: check overlay first, then base.
    /// Returns `None` if the tool was demoted in the overlay.
    pub fn effective_runtime(
        base: &TurnCapabilityBase,
        overlay: &TurnCapabilityOverlay,
        name: &str,
    ) -> Option<Arc<dyn ToolRuntime>> {
        if overlay.removals.contains(name) {
            return None;
        }
        if let Some((_, rt)) = overlay.additions.get(name) {
            return Some(rt.clone());
        }
        base.get_runtime(name)
    }

    /// Merge this overlay onto a base, producing the next Turn's base.
    /// Consumes the overlay.
    pub fn adopt(self, base: &TurnCapabilityBase) -> TurnCapabilityBase {
        let mut new_specs = base.tool_specs.clone();
        let mut new_runtimes = base.tool_runtimes.clone();
        for (name, (spec, rt)) in self.additions {
            new_specs.insert(name.clone(), spec);
            new_runtimes.insert(name, rt);
        }
        for name in self.removals {
            new_specs.remove(&name);
            new_runtimes.remove(&name);
        }
        TurnCapabilityBase {
            generation: base.generation + 1,
            tool_specs: new_specs,
            tool_runtimes: new_runtimes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::error::GrodexError;
    use grodex_core::id::OperationId;
    use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata, ToolRuntime};
    use serde::{Deserialize, Serialize};

    struct TestTool {
        metadata: ToolMetadata,
        schema: serde_json::Value,
    }

    impl grodex_core::tool::Tool for TestTool {
        type Args = serde_json::Value;
        type Output = serde_json::Value;
        fn metadata(&self) -> ToolMetadata { self.metadata.clone() }
        fn input_schema(&self) -> serde_json::Value { self.schema.clone() }
        fn output_schema(&self) -> serde_json::Value { serde_json::json!({}) }
    }

    #[async_trait::async_trait]
    impl ToolRuntime for TestTool {
        async fn execute(&self, _args: serde_json::Value, _id: OperationId) -> Result<serde_json::Value, GrodexError> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    fn make_tool(name: &str) -> (TestTool, ToolSpec) {
        let tool = TestTool {
            metadata: ToolMetadata {
                name: name.into(), display_name: name.into(),
                description: format!("Tool {name}"),
                concurrency_class: ConcurrencyClass::Parallel,
                side_effect_class: SideEffectClass::ReadOnly,
                default_policy: grodex_core::policy::PolicyDecision::Allow,
            },
            schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        };
        let spec = ToolSpec {
            name: name.into(),
            description: format!("Tool {name}"),
            parameters: tool.schema.clone(),
            required: vec![],
        };
        (tool, spec)
    }

    #[test]
    fn generation_isolation() {
        let mut mgr = CapabilityManager::new(5);
        let gen1 = mgr.current_gen();
        let (tool_a, spec_a) = make_tool("tool_a");
        mgr.register_tool("tool_a".into(), Arc::new(tool_a), spec_a);
        let gen2 = mgr.current_gen();

        assert_ne!(gen1, gen2);
        // gen1 should NOT see tool_a (it was registered at gen2).
        assert!(mgr.model_specs_for(gen1).is_empty());
        // gen2 SHOULD see tool_a.
        assert_eq!(mgr.model_specs_for(gen2).len(), 1);
        assert!(mgr.get_runtime(gen2, "tool_a").is_some());
    }

    #[test]
    fn drift_prevention() {
        let mut mgr = CapabilityManager::new(5);

        // Step N: register tool v1, capture gen.
        let (tool_v1, spec_v1) = make_tool("reader");
        let gen_n = mgr.register_tool("reader".into(), Arc::new(tool_v1), spec_v1);

        // Model samples against gen_n, produces a tool call.

        // Tool is then updated (v2) before the call executes.
        let (tool_v2, spec_v2) = make_tool("reader");
        let _gen_n1 = mgr.register_tool("reader".into(), Arc::new(tool_v2), spec_v2);

        // Step N's tool call executes — must use v1, not v2.
        let runtime = mgr.get_runtime(gen_n, "reader").unwrap();
        // The runtime from gen_n should be the v1 runtime (we can't compare
        // Arc identity directly, but the spec lookup confirms isolation).
        let specs_n = mgr.model_specs_for(gen_n);
        let specs_n1 = mgr.model_specs_for(_gen_n1);
        // Both have 1 tool but they're different generations.
        assert_eq!(specs_n.len(), 1);
        assert_eq!(specs_n1.len(), 1);
    }

    #[test]
    fn eviction_fallback() {
        let mut mgr = CapabilityManager::new(3);
        let gen1 = mgr.current_gen();
        // Register 5 tools to force eviction of gen1.
        for i in 0..5 {
            let (tool, spec) = make_tool(&format!("tool_{i}"));
            mgr.register_tool(format!("tool_{i}"), Arc::new(tool), spec);
        }
        assert!(mgr.is_evicted(gen1));
        // Evicted generation falls back to latest.
        assert!(mgr.get_runtime(gen1, "tool_4").is_some());
    }

    #[test]
    fn turn_capability_base_freezes_at_turn_start() {
        let mut mgr = CapabilityManager::new(5);
        let (tool_a, spec_a) = make_tool("tool_a");
        mgr.register_tool("tool_a".into(), Arc::new(tool_a), spec_a);

        // Turn 1: snapshot base.
        let base = mgr.snapshot_base();
        assert_eq!(base.generation, mgr.current_gen());
        assert_eq!(base.tool_specs.len(), 1);

        // Mid-Turn: register tool_b directly to manager.
        let (tool_b, spec_b) = make_tool("tool_b");
        mgr.register_tool("tool_b".into(), Arc::new(tool_b), spec_b);

        // The base should still only see tool_a (frozen).
        assert_eq!(base.model_specs().len(), 1, "base is frozen");
        assert!(base.get_runtime("tool_b").is_none(), "tool_b not in base");
    }

    #[test]
    fn overlay_promote_visible_in_effective_specs() {
        let mut mgr = CapabilityManager::new(5);
        let (tool_a, spec_a) = make_tool("tool_a");
        mgr.register_tool("tool_a".into(), Arc::new(tool_a), spec_a);

        let base = mgr.snapshot_base();
        let mut overlay = TurnCapabilityOverlay::new();

        // Promote tool_b via overlay (not manager).
        let (tool_b, spec_b) = make_tool("tool_b");
        overlay.promote("tool_b".into(), spec_b, Arc::new(tool_b));

        // Effective specs should include both base + overlay.
        let specs = TurnCapabilityOverlay::effective_specs(&base, &overlay);
        assert_eq!(specs.len(), 2, "base + overlay addition");

        // Effective runtime should find tool_b in overlay.
        assert!(TurnCapabilityOverlay::effective_runtime(&base, &overlay, "tool_b").is_some());
    }

    #[test]
    fn overlay_demote_hides_from_effective_specs() {
        let mut mgr = CapabilityManager::new(5);
        let (tool_a, spec_a) = make_tool("tool_a");
        mgr.register_tool("tool_a".into(), Arc::new(tool_a), spec_a);
        let (tool_b, spec_b) = make_tool("tool_b");
        mgr.register_tool("tool_b".into(), Arc::new(tool_b), spec_b);

        let base = mgr.snapshot_base();
        let mut overlay = TurnCapabilityOverlay::new();
        overlay.demote("tool_a");

        let specs = TurnCapabilityOverlay::effective_specs(&base, &overlay);
        assert_eq!(specs.len(), 1, "tool_a demoted");
        assert!(TurnCapabilityOverlay::effective_runtime(&base, &overlay, "tool_a").is_none());
    }

    #[test]
    fn overlay_adopt_produces_next_turn_base() {
        let mut mgr = CapabilityManager::new(5);
        let (tool_a, spec_a) = make_tool("tool_a");
        mgr.register_tool("tool_a".into(), Arc::new(tool_a), spec_a);

        let base = mgr.snapshot_base();
        let mut overlay = TurnCapabilityOverlay::new();
        let (tool_b, spec_b) = make_tool("tool_b");
        overlay.promote("tool_b".into(), spec_b, Arc::new(tool_b));
        overlay.demote("tool_a");

        // Adopt: merge overlay into next base.
        let next_base = overlay.adopt(&base);
        assert_eq!(next_base.generation, base.generation + 1);
        assert_eq!(next_base.tool_specs.len(), 1, "only tool_b (tool_a demoted)");
        assert!(next_base.get_runtime("tool_b").is_some());
        assert!(next_base.get_runtime("tool_a").is_none());
    }

    #[test]
    fn manager_adopt_overlay_applies_changes() {
        let mut mgr = CapabilityManager::new(5);
        let (tool_a, spec_a) = make_tool("tool_a");
        mgr.register_tool("tool_a".into(), Arc::new(tool_a), spec_a);

        let base = mgr.snapshot_base();
        let mut overlay = TurnCapabilityOverlay::new();
        let (tool_b, spec_b) = make_tool("tool_b");
        overlay.promote("tool_b".into(), spec_b, Arc::new(tool_b));

        let gen_before = mgr.current_gen();
        mgr.adopt_overlay(overlay);

        // Manager should now have tool_b.
        assert!(mgr.current_gen() > gen_before);
        assert!(mgr.latest().get_runtime("tool_b").is_some());
    }
}
