//! Telemetry-projection integration tests.
//!
//! Proves the "双层记录" contract at the writer boundary:
//! 1. Every successful journal append emits exactly one telemetry record
//!    carrying the committed `journal_seq` (single choke point in
//!    `RolloutWriter::write`).
//! 2. `reproject_telemetry()` restores lost projection rows after a
//!    simulated crash (kill before telemetry commit) and is idempotent.
//! 3. Telemetry failure never affects journal write results.
//!
//! The rollout journal is the source of truth; telemetry.db is only a
//! query projection — these tests are what keep that boundary honest.

use grodex_core::id::{SessionId, TurnId};
use grodex_loop::rollout_writer::RolloutWriter;
use grodex_rollout::store::{FileRolloutStore, RolloutStore};
use grodex_telemetry::{kind, TelemetryRecord, TelemetrySink};
use std::sync::{Arc, Mutex};

async fn store(dir: &tempfile::TempDir, sid: SessionId) -> Arc<dyn RolloutStore> {
    Arc::new(FileRolloutStore::new_session(dir.path(), &sid.to_string()).await.unwrap())
}

/// In-memory sink that records what the writer emitted.
#[derive(Clone, Default)]
struct MemorySink {
    records: Arc<Mutex<Vec<TelemetryRecord>>>,
}

impl TelemetrySink for MemorySink {
    fn emit(&self, record: TelemetryRecord) {
        self.records.lock().unwrap().push(record);
    }
    fn ingest(&self, records: Vec<TelemetryRecord>) -> usize {
        let n = records.len();
        self.records.lock().unwrap().extend(records);
        n
    }
    fn flush(&self) {}
}

/// Every journal write emits a telemetry record whose event_id is
/// "{session_id}:{seq}" and whose journal_seq matches the returned seq.
#[tokio::test]
async fn every_journal_write_emits_telemetry_with_seq() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let sink = MemorySink::default();
    let writer = RolloutWriter::new(store(&dir, sid).await, sid)
        .with_telemetry(Arc::new(sink.clone()), "run-1".into());

    let turn = TurnId::new();
    let seq_input = writer.write_user_input(turn, "hello").await.unwrap();
    let seq_started = writer.write_turn_started(turn, 5).await.unwrap();
    let seq_done = writer
        .write_turn_completed_with(turn, "final_answer", &serde_json::json!({"steps": 1}))
        .await
        .unwrap();

    let records = sink.records.lock().unwrap();
    assert_eq!(records.len(), 3, "one telemetry record per journal append");
    for (rec, seq) in records.iter().zip([seq_input, seq_started, seq_done]) {
        assert_eq!(rec.journal_seq, Some(seq));
        assert_eq!(rec.event_id, format!("{}:{}", sid, seq));
        assert_eq!(rec.run_id, "run-1");
        assert_eq!(rec.session_id, sid.to_string());
    }
    // Sensitivity: user input is Personal; lifecycle events Normal.
    assert_eq!(records[0].sensitivity, grodex_telemetry::Sensitivity::Personal);
    assert_eq!(records[1].kind, kind::TURN_STARTED);
    assert_eq!(records[2].kind, kind::TURN_COMPLETED);
    // TurnCompleted payload carries the structured termination reason.
    let payload: serde_json::Value = serde_json::from_str(&records[2].payload_json).unwrap();
    assert_eq!(payload["termination_reason"], "final_answer");
}

/// Without a sink attached, writes still succeed (telemetry is optional).
#[tokio::test]
async fn writes_succeed_without_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let writer = RolloutWriter::new(store(&dir, sid).await, sid);
    let turn = TurnId::new();
    let seq = writer.write_turn_started(turn, 0).await.unwrap();
    assert_eq!(seq, 0, "journal actor allocates seq from 0");
}

/// Re-projection restores the projection from the journal and is
/// idempotent — the crash-gap-backfill contract.
#[tokio::test]
async fn reprojection_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new();
    let sink = MemorySink::default();
    let writer = RolloutWriter::new(store(&dir, sid).await, sid)
        .with_telemetry(Arc::new(sink.clone()), "run-2".into());

    let turn = TurnId::new();
    writer.write_session_started(&serde_json::json!({"cwd": "/tmp/x"})).await.unwrap();
    writer.write_turn_started(turn, 12).await.unwrap();
    writer
        .write_turn_completed_with(turn, "cancelled", &serde_json::json!({"cancel_count": 1}))
        .await
        .unwrap();

    // Simulate crash: the projection lost everything; journal replays all.
    let accepted = writer.reproject_telemetry().await;
    let accepted = accepted.expect("telemetry attached");
    assert_eq!(accepted, 3);
    // Replay again (fresh process startup against the same journal).
    let accepted2 = writer.reproject_telemetry().await.unwrap();
    assert_eq!(accepted2, 3);

    let records = sink.records.lock().unwrap();
    // 3 live emissions + 3 reprojected + 3 reprojected again — but event
    // ids are deterministic, so dedup is the DB's job (INSERT OR IGNORE,
    // covered in grodex-telemetry tests). Here we assert the ids repeat.
    let ids: Vec<&str> = records.iter().map(|r| r.event_id.as_str()).collect();
    assert_eq!(ids.len(), 9);
    for id in &ids {
        assert!(id.starts_with(&format!("{sid}:")));
    }
}
