//! End-to-end deferred promotion flow (Doc 10 §17.3, acceptance #6):
//! Tool Search hit → revision pin → Turn overlay → `CapabilityPromoted`
//! → next Step can use the capability; stale revision path included.

use std::collections::BTreeMap;

use grodex_capability::{
    CapabilityManager, CapabilityPromotedEvent, DeferredToolIndex, PromotionPlanner,
    SearchOutcome, TurnBaseInputs,
};
use grodex_capability::authority::Authority;
use grodex_capability::descriptor::StepCapabilitySnapshot;
use grodex_capability::exposure::ToolExposure;
use grodex_capability::id::{CapabilityId, CapabilityKind};
use grodex_core::id::{StepGeneration, StepSnapshotId};

fn deploy_id() -> CapabilityId {
    CapabilityId::new(Authority::Mcp, "srv", CapabilityKind::Tool, "mcp.deploy")
}

fn read_id() -> CapabilityId {
    CapabilityId::new(Authority::Core, "builtin", CapabilityKind::Tool, "read")
}

/// Two-step Turn where `mcp.deploy` is Deferred and `read` is Direct.
fn two_step_turn() -> (CapabilityManager, grodex_capability::descriptor::TurnCapabilityBase) {
    let manager = CapabilityManager::new().with_allow_deferred_promotion(true);
    let snaps: Vec<StepCapabilitySnapshot> = (0..2)
        .map(|i| {
            StepCapabilitySnapshot::builder(StepSnapshotId::new(), StepGeneration::new(i as u64))
                .build()
        })
        .collect();
    let inputs = TurnBaseInputs {
        turn_id: "turn-e2e",
        capability_generation: 9,
        requested_authority_ceiling: 60,
        promotable_ids: vec![deploy_id(), read_id()],
        step_snapshots: snaps,
        request_deferred_promotion_allowed: true,
    };
    let base = manager.capture_turn_base(inputs);
    (manager, base)
}

fn candidates() -> Vec<(CapabilityId, ToolExposure, u64, String, String)> {
    vec![
        (deploy_id(), ToolExposure::Deferred, 4, "deploy".into(), "Deploy the service".into()),
        (read_id(), ToolExposure::Direct, 1, "read".into(), "Read a file".into()),
    ]
}

#[test]
fn full_promotion_flow_reaches_next_step_overlay() {
    let (manager, base) = two_step_turn();

    // Step 0: Tool Search runs against the Turn's Deferred index.
    let index = DeferredToolIndex::build(&base, &candidates(), &[]);
    let hits = match index.search(&base, "deploy") {
        SearchOutcome::Hits(h) => h,
        other => panic!("expected hits, got {other:?}"),
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].pinned_revision, 4);

    // Hits pin revisions into the TurnContext planner.
    let mut planner = PromotionPlanner::new(&base.turn_id);
    planner.record_hits(&hits, 0);

    // Step 1 assembly: published revision unchanged → promotion valid.
    let mut current = BTreeMap::new();
    current.insert(deploy_id(), 4u64);
    let planned = planner.plan_overlay(1, &current);
    assert_eq!(planned.valid_ids, vec![deploy_id()]);
    assert_eq!(planned.events, vec![CapabilityPromotedEvent {
        turn_id: "turn-e2e".into(),
        query_hash: DeferredToolIndex::query_hash("deploy"),
        capability_id: deploy_id(),
        pinned_revision: 4,
        source_step_index: 0,
        target_step_index: 1,
    }]);

    // The valid ids flow into the manager's overlay for Step 1.
    let overlay = manager
        .suggest_overlay(&base, 1, Vec::new(), planned.valid_ids.clone(), Some("tool-search".into()))
        .expect("overlay must validate against the turn base");
    assert_eq!(overlay.deferred_promoted_ids, vec![deploy_id()]);

    let applied = manager.apply_overlay(&base, 1, &overlay).expect("overlay applies");
    assert!(applied.effective_promoted_ids.contains(&deploy_id()));

    // End-of-turn consistency passes with one overlay per step snapshot.
    let overlay0 = manager
        .suggest_overlay(&base, 0, vec![read_id()], Vec::new(), None)
        .expect("step 0 overlay");
    manager
        .validate_turn_consistency(&base, &[overlay0, overlay])
        .expect("turn consistent");
}

#[test]
fn stale_revision_never_reaches_the_overlay() {
    let (manager, base) = two_step_turn();
    let index = DeferredToolIndex::build(&base, &candidates(), &[]);
    let hits = match index.search(&base, "deploy") {
        SearchOutcome::Hits(h) => h,
        _ => panic!(),
    };
    let mut planner = PromotionPlanner::new(&base.turn_id);
    planner.record_hits(&hits, 0);

    // Published state moved on between search (rev 4) and assembly (rev 5).
    let mut current = BTreeMap::new();
    current.insert(deploy_id(), 5u64);
    let planned = planner.plan_overlay(1, &current);
    assert!(planned.valid_ids.is_empty());
    assert!(planned.events.is_empty());
    assert_eq!(planned.stale.len(), 1);
    assert_eq!(planned.stale[0].pinned_revision, 4);
    assert_eq!(planned.stale[0].current_revision, 5);

    // Nothing gets promoted — the next Step must re-search (controlled
    // resampling), never call the new definition under the old pin.
    let overlay = manager
        .suggest_overlay(&base, 1, Vec::new(), planned.valid_ids.clone(), None)
        .expect("empty overlay validates");
    assert!(overlay.deferred_promoted_ids.is_empty());
}

#[test]
fn session_gate_blocks_the_whole_flow_fail_closed() {
    // Session forbids deferred promotion → the Turn base inherits the gate
    // and search refuses to run at all.
    let manager = CapabilityManager::new(); // default: not allowed
    let snaps: Vec<StepCapabilitySnapshot> = (0..1)
        .map(|_| StepCapabilitySnapshot::builder(StepSnapshotId::new(), StepGeneration::new(0)).build())
        .collect();
    let base = manager.capture_turn_base(TurnBaseInputs {
        turn_id: "turn-locked",
        capability_generation: 1,
        requested_authority_ceiling: 60,
        promotable_ids: vec![deploy_id()],
        step_snapshots: snaps,
        request_deferred_promotion_allowed: true, // requested but denied
    });
    assert!(!base.deferred_promotion_allowed);
    let index = DeferredToolIndex::build(&base, &candidates(), &[]);
    assert_eq!(index.search(&base, "deploy"), SearchOutcome::PromotionNotAllowed);
}
