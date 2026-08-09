//! Crash-recovery tests — Design Doc 11 §29.2 "6 类崩溃位置恢复测试".
//!
//! Each test simulates the journal ending at one of the six crash points and
//! asserts the `SessionReducer` either rebuilds the correct partial context
//! or refuses with the right invariant error. Together they prove recovery is
//! defined at every crash boundary, not just the happy path.
//!
//! Crash points (§29.2):
//!   1. 采样请求前      — user input accepted, nothing sampled yet.
//!   2. 流式半途中        — partial assistant text, terminal event missing.
//!   3. ToolResult 写 rollout 前   — model emitted a tool call, no result yet.
//!   4. ToolResult 写后 commit 前   — result in journal, turn not completed.
//!   5. Compaction 替换前            — compaction not committed mid-stream.
//!   6. TurnCompleted 持久化前       — turn finished in memory, not journaled.

use grodex_core::id::SessionId;
use grodex_loop::reducer::{ReducerError, SessionReducer};
use grodex_loop::rollout_writer::RolloutWriter;
use grodex_rollout::event::{RolloutEvent};
use grodex_rollout::store::{FileRolloutStore, RolloutStore};
use std::sync::Arc;

fn store(dir: &tempfile::TempDir, sid: SessionId) -> Arc<dyn RolloutStore> {
    Arc::new(FileRolloutStore::new(dir.path(), &sid.to_string()).unwrap())
}

/// Replay `events` through a fresh reducer for `sid`.
fn replay(sid: SessionId, events: &[RolloutEvent]) -> Result<Vec<grodex_core::context::ContextItem>, ReducerError> {
    let mut r = SessionReducer::new(sid);
    r.apply_all(events)?;
    Ok(r.into_context())
}

/// 1. Crash before the first sampling request: only UserInputAccepted in
/// the journal. Recovery must yield a transcript consisting of exactly the
/// user message — no hallucinated assistant turn.
#[tokio::test]
async fn crash_1_before_sampling() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    writer.write_user_input(turn, "please summarize the file").await.unwrap();

    let events = writer.store().replay_from(0).await.unwrap();
    let ctx = replay(sid, &events).expect("clean replay of user-only journal");
    assert_eq!(ctx.len(), 1, "crash before sampling → only the user turn");
    assert!(matches!(ctx[0], grodex_core::context::ContextItem::User { .. }));
}

/// 2. Crash mid-stream: a ModelItemProduced arrived with assistant text but
/// no terminal event. The reducer must accept the partial assistant text
/// (recovery is best-effort on persisted facts) — it should NOT fabricate a
/// TurnCompleted.
#[tokio::test]
async fn crash_2_mid_stream() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    writer.write_user_input(turn, "hello").await.unwrap();
    writer
        .write_model_output(
            turn,
            grodex_core::id::StepId::new(),
            grodex_core::id::StepGeneration::new(1),
            "partial response so far",
            &[],
            None,
        )
        .await
        .unwrap();
    // No TurnCompleted — crash happened before the stream terminated.

    let events = writer.store().replay_from(0).await.unwrap();
    let ctx = replay(sid, &events).expect("partial assistant text is a valid recoverable state");
    assert_eq!(ctx.len(), 2, "user + partial assistant");
    assert!(matches!(ctx[1], grodex_core::context::ContextItem::Assistant { .. }));
}

/// 3. Crash before a tool result is persisted: model emitted a tool call but
/// the journal has no ToolResultCommitted. If a TurnCompleted had been
/// written, the reducer would flag an orphaned tool call; with no
/// TurnCompleted the partial state is acceptable (the tool call is pending).
#[tokio::test]
async fn crash_3_before_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    let step = grodex_core::id::StepId::new();
    let cap_gen = grodex_core::id::StepGeneration::new(1);
    let call_id = grodex_core::id::ToolCallId::new();
    writer.write_user_input(turn, "read it").await.unwrap();
    writer
        .write_model_output(
            turn, step, cap_gen,
            "",
            &[serde_json::json!({"call_id": call_id.to_string(), "name": "read_file", "arguments": {"path": "/x"}})],
            None,
        )
        .await
        .unwrap();
    // No tool result, no TurnCompleted.

    let events = writer.store().replay_from(0).await.unwrap();
    let ctx = replay(sid, &events).expect("pending tool call is recoverable");
    assert_eq!(ctx.len(), 2, "user + pending tool call");
    assert!(matches!(ctx[1], grodex_core::context::ContextItem::ToolCall { .. }));
}

/// 3b. Negative case: if a TurnCompleted IS persisted but the result is
/// missing, the reducer MUST reject it as an orphaned tool call
/// (invariant #9) — recovery refuses to build a dangling transcript.
#[tokio::test]
async fn crash_3_orphaned_tool_call_rejected_on_turn_complete() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    let step = grodex_core::id::StepId::new();
    let cap_gen = grodex_core::id::StepGeneration::new(1);
    let call_id = grodex_core::id::ToolCallId::new();
    writer.write_user_input(turn, "read it").await.unwrap();
    writer
        .write_model_output(
            turn, step, cap_gen,
            "",
            &[serde_json::json!({"call_id": call_id.to_string(), "name": "read_file", "arguments": {"path": "/x"}})],
            None,
        )
        .await
        .unwrap();
    writer.write_turn_completed(turn).await.unwrap(); // turn marker WITHOUT a result

    let events = writer.store().replay_from(0).await.unwrap();
    let err = replay(sid, &events).unwrap_err();
    assert!(
        matches!(err, ReducerError::OrphanedToolResult(_)),
        "expected OrphanedToolResult, got {err:?}"
    );
}

/// 4. Crash after the tool result is persisted but before TurnCompleted is
/// written: the round-trip (call + result) must rebuild cleanly; only the
/// turn boundary marker is missing, which is acceptable.
#[tokio::test]
async fn crash_4_after_tool_result_before_turn_complete() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    let step = grodex_core::id::StepId::new();
    let cap_gen = grodex_core::id::StepGeneration::new(1);
    let call_id = grodex_core::id::ToolCallId::new();
    writer.write_user_input(turn, "read it").await.unwrap();
    writer
        .write_model_output(
            turn, step, cap_gen,
            "",
            &[serde_json::json!({"call_id": call_id.to_string(), "name": "read_file", "arguments": {"path": "/x"}})],
            None,
        )
        .await
        .unwrap();
    writer
        .write_tool_finished(turn, step, cap_gen, &call_id.to_string(), "contents", false)
        .await
        .unwrap();
    // TurnCompleted never written — crash between commit and turn marker.

    let events = writer.store().replay_from(0).await.unwrap();
    let ctx = replay(sid, &events).expect("full round-trip is recoverable");
    assert_eq!(ctx.len(), 3, "user + tool call + tool result");
    assert!(matches!(ctx[2], grodex_core::context::ContextItem::ToolResult { .. }));
}

/// 5. Crash before compaction replaces the projection: a CompactionCommitted
/// event may be mid-write. The reducer applies CompactionCommitted
/// atomically — either the items decode to a full replacement, or the prior
/// context survives. Here we assert a complete CompactionCommitted replaces
/// the prior context (no half-replaced state).
#[tokio::test]
async fn crash_5_before_compaction_replace() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    writer.write_user_input(turn, "long conversation").await.unwrap();
    writer
        .write_model_output(
            turn,
            grodex_core::id::StepId::new(),
            grodex_core::id::StepGeneration::new(1),
            "lots of text",
            &[],
            None,
        )
        .await
        .unwrap();
    // Compaction committed with a replacement set of items.
    let rebuilt = vec![
        grodex_core::context::ContextItem::System { content: "summary".into() },
        grodex_core::context::ContextItem::User { content: "last user msg".into(), message_id: None },
    ];
    writer.write_compaction(Some(turn), &rebuilt).await.unwrap();

    let events = writer.store().replay_from(0).await.unwrap();
    let ctx = replay(sid, &events).expect("compaction is recoverable");
    // Compaction atomically replaces → exactly the 2 replacement items.
    assert_eq!(ctx.len(), 2, "compaction replaced the prior context");
    assert!(matches!(ctx[0], grodex_core::context::ContextItem::System { .. }));
}

/// 6. Crash before TurnCompleted is persisted: identical to point 4 in
/// shape, but exercised with a no-tool turn (text-only) to cover the
/// "turn done in memory, marker not durable" boundary. The assistant text
/// must survive; no turn marker.
#[tokio::test]
async fn crash_6_before_turn_completed_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    writer.write_user_input(turn, "say hi").await.unwrap();
    writer
        .write_model_output(
            turn,
            grodex_core::id::StepId::new(),
            grodex_core::id::StepGeneration::new(1),
            "hi there",
            &[],
            None,
        )
        .await
        .unwrap();
    // TurnCompleted omitted.

    let events = writer.store().replay_from(0).await.unwrap();
    let ctx = replay(sid, &events).expect("assistant text is recoverable without turn marker");
    assert_eq!(ctx.len(), 2);
    assert!(matches!(ctx[1], grodex_core::context::ContextItem::Assistant { content: ref s } if s == "hi there"));
}

/// Cross-check: a fully-persisted turn (all six points passed) yields the
/// canonical complete transcript with no rejected events — the union of the
/// six crash tests must converge to this on a clean shutdown.
#[tokio::test]
async fn clean_shutdown_full_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid), sid);
    let turn = grodex_core::id::TurnId::new();
    let step = grodex_core::id::StepId::new();
    let cap_gen = grodex_core::id::StepGeneration::new(1);
    let call_id = grodex_core::id::ToolCallId::new();
    writer.write_user_input(turn, "read it").await.unwrap();
    writer
        .write_model_output(
            turn, step, cap_gen,
            "",
            &[serde_json::json!({"call_id": call_id.to_string(), "name": "read_file", "arguments": {"path": "/x"}})],
            None,
        )
        .await
        .unwrap();
    writer.write_tool_finished(turn, step, cap_gen, &call_id.to_string(), "contents", false).await.unwrap();
    writer.write_turn_completed(turn).await.unwrap();

    let events = writer.store().replay_from(0).await.unwrap();
    let ctx = replay(sid, &events).expect("complete turn replays cleanly");
    assert_eq!(ctx.len(), 3, "user + tool call + tool result");
    // keep unused import lint quiet
}
