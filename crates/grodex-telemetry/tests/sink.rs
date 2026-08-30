//! End-to-end tests for the SQLite telemetry sink: batching, flush,
//! idempotent re-projection, and queue-full shedding.

use std::sync::atomic::Ordering;

use grodex_telemetry::{
    kind, NoopTelemetrySink, Sensitivity, Severity, SqliteTelemetrySink, TelemetryRecord,
    TelemetrySink,
};

fn tmp_db(tag: &str) -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{tag}.db"));
    // Leak the tempdir: the guard joins the writer thread at drop, and we
    // sometimes assert after dropping the guard; the file must outlive it.
    std::mem::forget(dir);
    path
}

fn turn_started(session: &str, turn: &str, seq: u64) -> TelemetryRecord {
    let mut r = TelemetryRecord::from_journal(
        session,
        seq,
        "run-1",
        Some(turn),
        None,
        None,
        kind::TURN_STARTED,
        chrono::Utc::now(),
    );
    r.payload_json = r#"{"input_chars": 42}"#.into();
    r
}

fn turn_completed(session: &str, turn: &str, seq: u64) -> TelemetryRecord {
    let mut r = TelemetryRecord::from_journal(
        session,
        seq,
        "run-1",
        Some(turn),
        None,
        None,
        kind::TURN_COMPLETED,
        chrono::Utc::now(),
    );
    r.payload_json = serde_json::json!({
        "termination_reason": "final_answer",
        "steps": 3,
        "model_calls": 3,
        "tool_calls": 2,
        "retries": 1,
        "compactions": 0,
        "cancel_count": 0,
        "duration_ms": 4321,
    })
    .to_string();
    r
}

#[test]
fn session_and_turn_projection() {
    let path = tmp_db("proj");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");

    let mut s = TelemetryRecord::from_journal(
        "sess-a", 1, "run-1", None, None, None, kind::SESSION_STARTED, chrono::Utc::now(),
    );
    s.payload_json = serde_json::json!({"cwd": "/tmp/w", "model_provider": "openai", "model": "gpt-x"}).to_string();
    sink.emit(s);
    sink.emit(turn_started("sess-a", "t-1", 2));
    sink.emit(turn_completed("sess-a", "t-1", 3));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (status, reason, steps, duration): (String, String, i64, i64) = conn
        .query_row(
            "SELECT status, termination_reason, steps, duration_ms FROM turns WHERE turn_id='t-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(status, "completed");
    assert_eq!(reason, "final_answer");
    assert_eq!(steps, 3);
    assert_eq!(duration, 4321);

    let (run_id, provider): (String, String) = conn
        .query_row(
            "SELECT run_id, model_provider FROM sessions WHERE session_id='sess-a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_id, "run-1");
    assert_eq!(provider, "openai");

    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM telemetry_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events, 3);
    drop(guard);
}

#[test]
fn re_projection_is_idempotent() {
    let path = tmp_db("reproj");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");

    let journal = vec![
        turn_started("sess-b", "t-9", 1),
        turn_completed("sess-b", "t-9", 2),
    ];
    // Ingest the same journal twice (crash recovery replays from 0).
    sink.ingest(journal.clone());
    sink.ingest(journal);
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let events: i64 = conn
        .query_row("SELECT COUNT(*) FROM telemetry_events WHERE session_id='sess-b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events, 2, "duplicate journal replay must not duplicate rows");
    let turns: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns WHERE session_id='sess-b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(turns, 1);
    let cursor: i64 = conn
        .query_row(
            "SELECT last_journal_seq FROM projection_cursors WHERE session_id='sess-b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cursor, 2);
    drop(guard);
}

#[test]
fn flush_delivers_pending_records() {
    let path = tmp_db("flush");
    let (sink, _guard) = SqliteTelemetrySink::open(&path).expect("open");
    for i in 0..300u64 {
        sink.emit(turn_started("sess-c", &format!("t-{i}"), i + 1));
    }
    sink.flush();
    let conn = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM turns WHERE session_id='sess-c'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 300, "flush must commit everything queued so far");
}

#[test]
fn cancelled_turn_maps_status() {
    let path = tmp_db("cancel");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    sink.emit(turn_started("sess-d", "t-c", 1));
    let mut r = turn_completed("sess-d", "t-c", 2);
    r.payload_json = r#"{"termination_reason": "cancelled"}"#.into();
    sink.emit(r);
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let status: String = conn
        .query_row("SELECT status FROM turns WHERE turn_id='t-c'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "cancelled");
    drop(guard);
}

#[test]
fn noop_sink_compiles_and_counts() {
    let sink = NoopTelemetrySink;
    sink.emit(turn_started("s", "t", 1));
    assert_eq!(sink.ingest(vec![turn_started("s", "t", 2)]), 1);
    sink.flush();
}

#[test]
fn sensitivity_and_severity_roundtrip() {
    let s = Sensitivity::Personal;
    let v = Severity::Warn;
    assert_eq!(s.as_str(), "personal");
    assert_eq!(v.as_str(), "warn");
    // Ordering: debug is shed first (lowest severity dropped first).
    assert!(Severity::Debug < Severity::Info);
    // Atomic counter used by the sink — sanity check the API we rely on.
    let c = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    c.fetch_add(1, Ordering::Relaxed);
    assert_eq!(c.load(Ordering::Relaxed), 1);
}

// ── P1 diagnostic projections ───────────────────────────────────────

use std::time::Duration as StdDuration;

fn rec(session: &str, kind_str: &str, seq: u64, turn: Option<&str>, step: Option<&str>, call: Option<&str>, payload: serde_json::Value, at: chrono::DateTime<chrono::Utc>) -> TelemetryRecord {
    let mut r = TelemetryRecord::from_journal(session, seq, "run-1", turn, step, call, kind_str, at);
    r.payload_json = payload.to_string();
    r
}

#[test]
fn model_attempt_projection() {
    let path = tmp_db("model_attempt");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let t0 = chrono::Utc::now();
    sink.emit(rec("sess-m", kind::MODEL_ATTEMPT_STARTED, 1, Some("t-m"), Some("s-1"), None,
        serde_json::json!({"request_id": "req_1", "provider": "openai", "model": "gpt-x", "wire_protocol": "Responses"}), t0));
    sink.emit(rec("sess-m", kind::MODEL_ATTEMPT_FINISHED, 2, Some("t-m"), Some("s-1"), None,
        serde_json::json!({
            "request_id": "req_1", "attempts": 2, "duration_ms": 3500, "status": "error",
            "error_class": "rate_limited", "http_status": 429, "retry_after_secs": 8,
            "usage": {"input_tokens": 1000, "cached_input_tokens": 250, "cache_creation_tokens": 0,
                      "output_tokens": 100, "reasoning_tokens": 20, "total_tokens": 1120, "estimated": false}
        }), t0 + chrono::Duration::milliseconds(3500)));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (status, err_cls, http, attempts): (String, String, i64, i64) = conn
        .query_row(
            "SELECT status, error_class, http_status, attempts FROM model_attempts WHERE session_id='sess-m'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
    assert_eq!(status, "error");
    assert_eq!(err_cls, "rate_limited");
    assert_eq!(http, 429);
    assert_eq!(attempts, 2, "retry count recorded on the attempt row");
    let (input, cached): (i64, i64) = conn
        .query_row("SELECT input_tokens, cached_input_tokens FROM model_attempts", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((input, cached), (1000, 250));
    drop(guard);
}

#[test]
fn tool_lifecycle_projection_and_approval_wait() {
    let path = tmp_db("tool_lifecycle");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let t0 = chrono::Utc::now();

    sink.emit(rec("sess-t", kind::TOOL_PREPARED, 1, Some("t-t"), Some("s-1"), Some("call-1"),
        serde_json::json!({"call_id": "call-1", "name": "exec", "operation_id": "op-1"}), t0));
    // Approval round-trip takes 2s — approval_wait_ms must reflect it.
    sink.emit(rec("sess-t", kind::APPROVAL_REQUESTED, 2, Some("t-t"), Some("s-1"), Some("call-1"),
        serde_json::json!({"ticket_id": "tk-1", "tool_name": "exec", "call_id": "call-1", "operation_id": "op-1"}), t0));
    sink.emit(rec("sess-t", kind::APPROVAL_RESOLVED, 3, Some("t-t"), Some("s-1"), None,
        serde_json::json!({"ticket_id": "tk-1", "resolution": "approved", "call_id": "call-1"}), t0 + StdDuration::from_secs(2)));
    sink.emit(rec("sess-t", kind::TOOL_APPROVED, 4, Some("t-t"), Some("s-1"), Some("call-1"),
        serde_json::json!({"call_id": "call-1", "name": "exec"}), t0 + StdDuration::from_secs(2)));
    sink.emit(rec("sess-t", kind::TOOL_STARTED, 5, Some("t-t"), Some("s-1"), Some("call-1"),
        serde_json::json!({"call_id": "call-1", "name": "exec"}), t0 + StdDuration::from_secs(2)));
    sink.emit(rec("sess-t", kind::TOOL_FINISHED, 6, Some("t-t"), Some("s-1"), Some("call-1"),
        serde_json::json!({"call_id": "call-1", "is_error": false, "exit_code": 0, "duration_ms": 150}), t0 + StdDuration::from_secs(2) + chrono::Duration::milliseconds(150)));
    sink.emit(rec("sess-t", kind::TOOL_RESULT_COMMITTED, 7, Some("t-t"), Some("s-1"), Some("call-1"),
        serde_json::json!({"call_id": "call-1", "content": "ok", "is_error": false}), t0 + StdDuration::from_secs(2) + chrono::Duration::milliseconds(160)));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (status, dur, wait, exit): (String, i64, i64, i64) = conn
        .query_row(
            "SELECT status, duration_ms, approval_wait_ms, exit_code FROM tool_executions WHERE call_id='call-1'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
    assert_eq!(status, "committed");
    assert_eq!(dur, 150);
    assert_eq!(exit, 0);
    assert!(wait >= 1900 && wait <= 2200, "approval wait ≈ 2s, got {wait}");
}

#[test]
fn crashed_tool_shows_running_and_doctor_flags_it() {
    let path = tmp_db("doctor");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let t0 = chrono::Utc::now();
    sink.emit(rec("sess-d", kind::TURN_STARTED, 1, Some("t-d"), None, None,
        serde_json::json!({"input_chars": 10}), t0));
    sink.emit(rec("sess-d", kind::TOOL_PREPARED, 2, Some("t-d"), Some("s-1"), Some("call-x"),
        serde_json::json!({"call_id": "call-x", "name": "exec"}), t0));
    sink.emit(rec("sess-d", kind::TOOL_STARTED, 3, Some("t-d"), Some("s-1"), Some("call-x"),
        serde_json::json!({"call_id": "call-x", "name": "exec"}), t0));
    // Crash here — no TOOL_FINISHED / no TURN_COMPLETED.
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let report = grodex_telemetry::doctor(&conn).unwrap();
    assert_eq!(report.open_turns, 1, "started turn without finish");
    assert_eq!(report.running_tools, 1, "started tool without finish");
    assert_eq!(report.uncommitted_results, 0);
    drop(guard);
}

#[test]
fn security_decisions_projection() {
    let path = tmp_db("security");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let t0 = chrono::Utc::now();
    sink.emit(rec("sess-s", kind::APPROVAL_REQUESTED, 1, Some("t-s"), Some("s-1"), Some("c-1"),
        serde_json::json!({"ticket_id": "tk-9", "tool_name": "write_file", "call_id": "c-1"}), t0));
    sink.emit(rec("sess-s", kind::APPROVAL_RESOLVED, 2, Some("t-s"), Some("s-1"), Some("c-1"),
        serde_json::json!({"ticket_id": "tk-9", "resolution": "narrowed"}), t0));
    sink.emit(rec("sess-s", kind::LEASE_ISSUED, 3, Some("t-s"), Some("s-1"), Some("c-1"),
        serde_json::json!({"lease_id": "l-1", "ticket_id": "tk-9", "call_id": "c-1"}), t0));
    sink.emit(rec("sess-s", kind::CAPABILITY_REJECTED_STALE, 4, Some("t-s"), Some("s-1"), None,
        serde_json::json!({"capability_id": "cap-1", "reason": "stale_or_evicted"}), t0));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM security_decisions WHERE session_id='sess-s'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4);
    let narrowed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM security_decisions WHERE decision_type='approval_resolved' AND decision='narrowed'",
            [], |r| r.get(0),
        ).unwrap();
    assert_eq!(narrowed, 1);
    drop(guard);
}

#[test]
fn prompt_and_compaction_projection() {
    let path = tmp_db("prompt_compaction");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let t0 = chrono::Utc::now();

    sink.emit(rec("sess-p", kind::PROMPT_SNAPSHOT, 1, Some("t-p"), Some("s-1"), None,
        serde_json::json!({"prompt_snapshot_hash": "abc123", "context_item_count": 42, "estimated_input_tokens": 9000}), t0));
    sink.emit(rec("sess-p", kind::COMPACTION_STARTED, 2, Some("t-p"), None, None,
        serde_json::json!({"trigger": "token_budget", "pre_compaction_item_count": 40}), t0));
    sink.emit(rec("sess-p", kind::COMPACTION_CANDIDATE, 3, Some("t-p"), None, None,
        serde_json::json!({"candidate_item_count": 12}), t0));
    sink.emit(rec("sess-p", kind::COMPACTION_COMMITTED, 4, Some("t-p"), None, None,
        serde_json::json!({}), t0));
    // A second compaction that failed.
    sink.emit(rec("sess-p", kind::COMPACTION_STARTED, 5, Some("t-p"), None, None,
        serde_json::json!({"trigger": "token_budget", "pre_compaction_item_count": 30}), t0));
    sink.emit(rec("sess-p", kind::COMPACTION_FAILED, 6, Some("t-p"), None, None,
        serde_json::json!({"reason": "model error"}), t0));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (hash, est): (String, i64) = conn
        .query_row("SELECT prompt_snapshot_hash, estimated_input_tokens FROM prompt_builds", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(hash, "abc123");
    assert_eq!(est, 9000);
    let (committed, failed): (i64, i64) = conn
        .query_row(
            "SELECT SUM(status='committed'), SUM(status='failed') FROM compactions",
            [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
    assert_eq!((committed, failed), (1, 1));
    let reason: String = conn
        .query_row("SELECT failure_reason FROM compactions WHERE status='failed'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(reason, "model error");
}

#[test]
fn views_exist_and_cache_stats_works() {
    let path = tmp_db("views");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let t0 = chrono::Utc::now();
    sink.emit(rec("sess-v", kind::SESSION_STARTED, 1, None, None, None,
        serde_json::json!({"cwd": "/w", "model_provider": "openai", "model": "m1"}), t0));
    sink.emit(rec("sess-v", kind::MODEL_ATTEMPT_STARTED, 2, Some("t-v"), Some("s-1"), None,
        serde_json::json!({"request_id": "r1", "provider": "openai", "model": "m1", "wire_protocol": "Responses"}), t0));
    sink.emit(rec("sess-v", kind::MODEL_ATTEMPT_FINISHED, 3, Some("t-v"), Some("s-1"), None,
        serde_json::json!({"request_id": "r1", "attempts": 1, "duration_ms": 1000, "status": "ok",
            "first_token_ms": 300,
            "usage": {"input_tokens": 800, "cached_input_tokens": 200, "cache_creation_tokens": 0,
                      "output_tokens": 50, "reasoning_tokens": 0, "total_tokens": 850, "estimated": false}}), t0));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    // All six views are queryable.
    for view in ["v_session_timeline", "v_turn_summary", "v_tool_lifecycle",
                 "v_model_usage", "v_cache_stats", "v_recovery_anomalies"] {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {view}"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("view {view} failed: {e}"));
        let _ = n;
    }
    let stats = grodex_telemetry::cache_stats(&conn).unwrap();
    assert_eq!(stats.len(), 1);
    let s = &stats[0];
    assert_eq!(s.model, "m1");
    assert!((s.cache_hit_rate.unwrap() - 0.25).abs() < 1e-9);
    // TTFT recorded on the attempt row.
    let ttft: i64 = conn
        .query_row("SELECT first_token_ms FROM model_attempts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ttft, 300);
    drop(guard);
}

#[test]
fn peripheral_projections() {
    let path = tmp_db("peripheral");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let t0 = chrono::Utc::now();

    // Sub-agent lifecycle.
    sink.emit(rec("sess-x", kind::SUBAGENT_STARTED, 1, Some("t-x"), None, None,
        serde_json::json!({"task_id": "task-1", "agent_id": "a-1", "parent_id": "root", "label": "research"}), t0));
    sink.emit(rec("sess-x", kind::SUBAGENT_FINISHED, 2, Some("t-x"), None, None,
        serde_json::json!({"task_id": "task-1", "status": "completed", "tokens": 4321}), t0));
    // Skill snapshot (two skills in one event).
    sink.emit(rec("sess-x", kind::SKILL_SNAPSHOT, 3, Some("t-x"), None, None,
        serde_json::json!({"skill_generation": 3, "skills": [
            {"name": "rust-review", "source": "Project", "path": "/w/.agent/skills/a.md", "content_hash": "h1"},
            {"name": "commit-msg", "source": "User", "path": "~/.grodex/skills/b.md", "content_hash": "h2"}
        ]}), t0));
    // Out-of-band memory retrieval.
    sink.emit(rec("sess-x", kind::MEMORY_RETRIEVAL, 0, Some("t-x"), None, None,
        serde_json::json!({"query_chars": 120, "selected_count": 3, "duration_ms": 45, "router_kind": "hybrid_rrf"}), t0));
    // Out-of-band MCP lifecycle.
    sink.emit(rec("sess-x", kind::MCP_LIFECYCLE, 0, None, None, None,
        serde_json::json!({"server_name": "fs", "phase": "spawn", "transport": "stdio",
                            "tool_count": 0, "status": "failed", "error_class": "timeout", "duration_ms": 5000}), t0));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (status, tokens): (String, i64) = conn
        .query_row("SELECT status, tokens FROM subagent_runs WHERE task_id='task-1'", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((status.as_str(), tokens), ("completed", 4321));
    let skills: i64 = conn
        .query_row("SELECT COUNT(*) FROM skill_activations WHERE session_id='sess-x'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(skills, 2);
    let (sel, dur): (i64, i64) = conn
        .query_row("SELECT selected_count, duration_ms FROM memory_retrievals", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((sel, dur), (3, 45));
    let (mcp_status, err): (String, String) = conn
        .query_row("SELECT status, error_class FROM mcp_lifecycle WHERE server_name='fs'", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((mcp_status.as_str(), err.as_str()), ("failed", "timeout"));
    drop(guard);
}

#[test]
fn retention_removes_old_events_only() {
    let path = tmp_db("retention");
    let (sink, guard) = SqliteTelemetrySink::open(&path).expect("open");
    let old = chrono::Utc::now() - chrono::Duration::days(60);
    let mut r_old = rec("sess-r", kind::TURN_STARTED, 1, Some("t-old"), None, None,
        serde_json::json!({"input_chars": 1}), old);
    r_old.occurred_at = old;
    r_old.payload_json = "{}".into();
    // from_journal stamps occurred_at with the passed timestamp; patch it.
    r_old.occurred_at = old;
    sink.emit(r_old);
    sink.emit(rec("sess-r", kind::TURN_STARTED, 2, Some("t-new"), None, None,
        serde_json::json!({"input_chars": 1}), chrono::Utc::now()));
    sink.flush();

    let conn = rusqlite::Connection::open(&path).unwrap();
    grodex_telemetry::retain(&conn, 30);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM telemetry_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1, "60-day-old event removed, fresh event kept");
    drop(guard);
}

