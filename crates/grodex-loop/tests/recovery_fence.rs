//! Integration tests for the Phase-1 "断链" fixes.
//!
//! Topics covered:
//! 1. RolloutWriter is the single seq source — no `seq: 0` collisions.
//! 2. generation is written into events and the reducer's
//!    GenerationRegression check can actually fire (it could not before,
//!    when generation was always `None`).
//! 3. Crash recovery: write a journal through RolloutWriter, then replay
//!    and reduce it and assert the rebuilt context matches, with the
//!    writer resuming from the correct next seq.
//! 4. Commit fence: a failing store surfaces the error instead of being
//!    silently dropped.

use grodex_core::id::SessionId;
use grodex_loop::reducer::SessionReducer;
use grodex_loop::rollout_writer::RolloutWriter;
use grodex_rollout::store::{FileRolloutStore, RolloutStore};


/// The writer assigns a gap-free, monotonic seq to every event it writes.
#[tokio::test]
async fn writer_assigns_monotonic_gap_free_seq() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let store: std::sync::Arc<dyn RolloutStore> =
        std::sync::Arc::new(FileRolloutStore::new_session(dir.path(), &sid.to_string()).await.unwrap());
    let writer = RolloutWriter::new(store, sid);

    let s0 = writer.write_state("idle").await.unwrap();
    let s1 = writer.write_user_input(grodex_core::id::TurnId::new(), "hello").await.unwrap();
    let s2 = writer.write_turn_completed(grodex_core::id::TurnId::new()).await.unwrap();

    assert_eq!([s0, s1, s2], [0, 1, 2], "seq must be 0,1,2 with no gaps");
}

/// After recovering from a journal, the writer must continue from the next
/// seq so a newly written event does not collide with a replayed one.
#[tokio::test]
async fn writer_resume_from_does_not_collide_with_replayed_events() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let store: std::sync::Arc<dyn RolloutStore> =
        std::sync::Arc::new(FileRolloutStore::new_session(dir.path(), &sid.to_string()).await.unwrap());
    let writer = RolloutWriter::new(store.clone(), sid);

    // Write a small journal.
    for i in 0..5u64 {
        writer.write_state(&format!("state-{i}")).await.unwrap();
    }

    // Simulate crash + restart: a new writer over the same store.
    let writer2 = RolloutWriter::new(store.clone(), sid);
    let replayed = writer2.store().replay_from(0).await.unwrap();
    assert_eq!(replayed.len(), 5);
    writer2.resume_from(replayed.len() as u64);

    // The next event must be seq 5, not seq 0.
    let next = writer2.write_state("recovered").await.unwrap();
    assert_eq!(next, 5, "resumed writer must not reuse a replayed seq");

    // And the full journal is still gap-free 0..6.
    let all = writer2.store().replay_from(0).await.unwrap();
    for (i, e) in all.iter().enumerate() {
        assert_eq!(e.seq, i as u64, "seq {} mismatch", i);
    }
}

/// Crash recovery end-to-end: write a full tool round-trip journal,
/// rebuild via the reducer, and assert the transcript matches exactly.
#[tokio::test]
async fn crash_recovery_rebuilds_transcript_from_journal() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let store: std::sync::Arc<dyn RolloutStore> =
        std::sync::Arc::new(FileRolloutStore::new_session(dir.path(), &sid.to_string()).await.unwrap());
    let writer = RolloutWriter::new(store.clone(), sid);

    let turn = grodex_core::id::TurnId::new();
    let step = grodex_core::id::StepId::new();
    let cap_gen = grodex_core::id::StepGeneration::new(1);
    let call_id = grodex_core::id::ToolCallId::new();
    writer.write_user_input(turn, "please read the file").await.unwrap();
    writer
        .write_model_output(
            turn, step, cap_gen,
            "",
            &[serde_json::json!({"call_id": call_id.to_string(), "name": "read_file", "arguments": {"path": "/tmp/x"}})],
            None,
        )
        .await
        .unwrap();
    // Tool result MUST precede TurnCompleted or the reducer flags it as
    // an orphaned tool call (invariant #9). Use the writer's fenced helper.
    writer
        .write_tool_finished(turn, step, cap_gen, &call_id.to_string(), None, "file contents", false)
        .await
        .unwrap();
    writer.write_turn_completed(turn).await.unwrap();

    // On restart, replay and reduce.
    let events = store.replay_from(0).await.unwrap();
    let mut reducer = SessionReducer::new(sid);
    reducer.apply_all(&events).unwrap();
    let ctx = reducer.into_context();

    // User + ToolCall + ToolResult.
    assert_eq!(ctx.len(), 3, "expected User + ToolCall + ToolResult, got {ctx:?}");
    assert!(matches!(ctx[0], grodex_core::context::ContextItem::User { .. }));
    assert!(matches!(ctx[1], grodex_core::context::ContextItem::ToolCall { .. }));
    assert!(matches!(ctx[2], grodex_core::context::ContextItem::ToolResult { .. }));
}

/// The reducer's GenerationRegression check can now actually fire, because
/// the writer stamps a real generation onto events (it could not when
/// generation was always `None`). To exercise the regression path we use a
/// writer that writes a high generation, then a writer call that stamps a
/// strictly-lower generation on the next event — something the live loop
/// would never do, but a corrupted/reordered journal could.
#[tokio::test]
async fn reducer_detects_generation_regression_with_real_generation() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let store: std::sync::Arc<dyn RolloutStore> =
        std::sync::Arc::new(FileRolloutStore::new_session(dir.path(), &sid.to_string()).await.unwrap());
    let writer = RolloutWriter::new(store.clone(), sid);

    let turn = grodex_core::id::TurnId::new();
    // Event 0: cap_gen 5.
    writer.write_user_input(turn, "hello").await.unwrap();
    // Event 1: cap_gen 5.
    writer
        .write_model_output(
            turn,
            grodex_core::id::StepId::new(),
            grodex_core::id::StepGeneration::new(5),
            "hi",
            &[],
            None,
        )
        .await
        .unwrap();
    // Event 2: cap_gen 3 (< 5) ⇒ regression. Stamped via the same writer so seq
    // stays continuous (3 == expected next seq after 0,1,2... here 2).
    writer
        .write_model_output(
            turn,
            grodex_core::id::StepId::new(),
            grodex_core::id::StepGeneration::new(3),
            "regressed",
            &[],
            None,
        )
        .await
        .unwrap();

    let events = store.replay_from(0).await.unwrap();
    let mut reducer = SessionReducer::new(sid);
    let err = reducer.apply_all(&events).unwrap_err();
    assert!(
        matches!(err, grodex_loop::reducer::ReducerError::GenerationRegression { .. }),
        "expected GenerationRegression, got {err:?}"
    );
}

/// Commit fence behaviour: a store that returns `Err` propagates the error
/// out of the writer (it is NOT silently dropped as `let _ = ...` did).
#[tokio::test]
async fn failing_store_surfaces_error_not_silently_dropped() {
    use async_trait::async_trait;

    struct BadStore;
    #[async_trait]
    impl RolloutStore for BadStore {
        async fn append_event(
            &self,
            _event: grodex_rollout::event::RolloutEvent,
        ) -> Result<u64, grodex_core::error::GrodexError> {
            Err(grodex_core::error::GrodexError::Internal(anyhow::anyhow!("disk full")))
        }
        async fn write_blob_async(
            &self,
            _content: &[u8],
            _mime_type: &str,
        ) -> Result<grodex_rollout::store::BlobRef, grodex_core::error::GrodexError> {
            unreachable!()
        }
        async fn read_blob_async(
            &self,
            _blob_id: &grodex_rollout::store::BlobId,
        ) -> Result<Vec<u8>, grodex_core::error::GrodexError> {
            unreachable!()
        }
        async fn replay_from(
            &self,
            _seq: u64,
        ) -> Result<Vec<grodex_rollout::event::RolloutEvent>, grodex_core::error::GrodexError> {
            Ok(Vec::new())
        }
    }

    let sid = SessionId::new();
    let store: std::sync::Arc<dyn RolloutStore> = std::sync::Arc::new(BadStore);
    let writer = RolloutWriter::new(store, sid);
    let result = writer.write_state("idle").await;
    assert!(
        result.is_err(),
        "writer must surface the store error, not swallow it; got {result:?}"
    );
}
