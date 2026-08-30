//! `SqliteTelemetrySink` — single-writer actor: business threads push
//! records into a bounded channel; one dedicated thread batches them
//! into SQLite transactions. Write failures are logged and swallowed —
//! telemetry must never affect the Agent Loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use sha2::Digest;

use crate::record::{kind, TelemetryRecord, TelemetrySink};
use crate::schema;

/// Queue capacity. Beyond this, records are dropped — the sink sheds
/// load instead of ever blocking the caller.
const CHANNEL_CAPACITY: usize = 4096;
/// Commit a batch after this many records...
const BATCH_MAX_RECORDS: usize = 64;
/// ...or this much time, whichever comes first.
const BATCH_MAX_INTERVAL: Duration = Duration::from_millis(100);
/// Upper bound for [`TelemetrySink::flush`].
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded retry window for [`TelemetrySink::ingest`] (startup path).
const INGEST_TOTAL_WAIT: Duration = Duration::from_secs(5);

enum Msg {
    Rec(Box<TelemetryRecord>),
    /// Commit everything queued, then ack on the embedded sender.
    Flush(SyncSender<()>),
    /// Commit and exit the writer thread (sent by the guard on drop).
    Shutdown,
}

/// Owns the writer thread. Hold until process exit; Drop performs a
/// final flush and joins the writer.
pub struct TelemetryGuard {
    tx: SyncSender<Msg>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // 1) Flush while the channel is still connected.
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(FLUSH_TIMEOUT);
        }
        // 2) Tell the writer to commit + exit (do NOT rely on channel
        //    disconnect — the sink may outlive this guard).
        let _ = self.tx.send(Msg::Shutdown);
        // 3) Join. The writer exits on Shutdown regardless of senders.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

pub struct SqliteTelemetrySink {
    tx: SyncSender<Msg>,
    dropped: Arc<AtomicU64>,
}

impl SqliteTelemetrySink {
    /// Open (or create) the telemetry DB and start the writer thread.
    /// Fails only if the DB cannot be opened — callers fall back to
    /// [`crate::NoopTelemetrySink`].
    pub fn open(path: &std::path::Path) -> Result<(Self, TelemetryGuard), rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        configure(&conn)?;
        schema::migrate(&conn)?;
        schema::create_views(&conn)?;
        restrict_permissions(path);
        retain(&conn, retention_days());

        let dropped = Arc::new(AtomicU64::new(0));
        let (tx, rx) = sync_channel::<Msg>(CHANNEL_CAPACITY);
        let dropped_writer = dropped.clone();
        let handle = std::thread::Builder::new()
            .name("grodex-telemetry".into())
            .spawn(move || writer_loop(conn, rx, dropped_writer))
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;

        Ok((
            Self { tx: tx.clone(), dropped },
            TelemetryGuard { tx, handle: Some(handle) },
        ))
    }

    /// Records shed due to a full queue (visible via `telemetry doctor`).
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl TelemetrySink for SqliteTelemetrySink {
    fn emit(&self, record: TelemetryRecord) {
        match self.tx.try_send(Msg::Rec(Box::new(record))) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                // Writer thread gone (only during shutdown).
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn ingest(&self, records: Vec<TelemetryRecord>) -> usize {
        let mut accepted = 0usize;
        let deadline = Instant::now() + INGEST_TOTAL_WAIT;
        for r in records {
            let mut delivered = false;
            // Startup path: bounded retry so re-projection is reliable
            // even if the queue is momentarily full.
            while Instant::now() < deadline {
                match self.tx.try_send(Msg::Rec(Box::new(r.clone()))) {
                    Ok(()) => {
                        delivered = true;
                        break;
                    }
                    Err(TrySendError::Full(_)) => std::thread::sleep(Duration::from_millis(5)),
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            if delivered {
                accepted += 1;
            } else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        accepted
    }

    fn flush(&self) {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        if self.tx.send(Msg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(FLUSH_TIMEOUT);
        }
    }
}

fn configure(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn restrict_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn writer_loop(
    conn: Connection,
    rx: std::sync::mpsc::Receiver<Msg>,
    dropped: Arc<AtomicU64>,
) {
    let mut batch: Vec<TelemetryRecord> = Vec::with_capacity(BATCH_MAX_RECORDS);
    let mut last_commit = Instant::now();
    loop {
        if batch.len() >= BATCH_MAX_RECORDS
            || (!batch.is_empty() && last_commit.elapsed() >= BATCH_MAX_INTERVAL)
        {
            commit(&conn, &mut batch);
            last_commit = Instant::now();
        }
        match rx.recv_timeout(BATCH_MAX_INTERVAL) {
            Ok(Msg::Rec(r)) => batch.push(*r),
            Ok(Msg::Flush(ack)) => {
                commit(&conn, &mut batch);
                last_commit = Instant::now();
                let _ = ack.send(());
            }
            Ok(Msg::Shutdown) => {
                commit(&conn, &mut batch);
                break;
            }
            Err(RecvTimeoutError::Timeout) => { /* loop condition handles commit */ }
            Err(RecvTimeoutError::Disconnected) => {
                commit(&conn, &mut batch);
                break;
            }
        }
    }
    let shed = dropped.load(Ordering::Relaxed);
    if shed > 0 {
        tracing::warn!(
            target: "grodex_telemetry",
            dropped = shed,
            "telemetry records dropped (queue full / shutdown)"
        );
    }
}

/// Insert a batch in one transaction + advance the projection tables.
fn commit(conn: &Connection, batch: &mut Vec<TelemetryRecord>) {
    if batch.is_empty() {
        return;
    }
    if let Err(e) = commit_inner(conn, batch) {
        // Telemetry write failure: log and drop — never propagate.
        tracing::warn!(
            target: "grodex_telemetry",
            error = %e,
            count = batch.len(),
            "telemetry commit failed; batch dropped"
        );
    }
    batch.clear();
}

fn commit_inner(conn: &Connection, batch: &[TelemetryRecord]) -> Result<(), rusqlite::Error> {
    let tx = conn.unchecked_transaction()?;
    for r in batch {
        tx.execute(
            r#"INSERT OR IGNORE INTO telemetry_events
               (event_id, run_id, session_id, turn_id, step_id, call_id,
                journal_seq, kind, status, severity, occurred_at, duration_ms,
                payload_json, sensitivity)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
            rusqlite::params![
                r.event_id,
                r.run_id,
                r.session_id,
                r.turn_id,
                r.step_id,
                r.call_id,
                r.journal_seq.map(|s| s as i64),
                r.kind,
                r.status,
                r.severity.as_str(),
                r.occurred_at.to_rfc3339(),
                r.duration_ms.map(|d| d as i64),
                r.payload_json,
                r.sensitivity.as_str(),
            ],
        )?;
        project(&tx, r)?;
    }
    tx.commit()
}

/// Maintain the sessions / turns / projection_cursors projections.
fn project(tx: &Connection, r: &TelemetryRecord) -> Result<(), rusqlite::Error> {
    let payload: serde_json::Value =
        serde_json::from_str(&r.payload_json).unwrap_or(serde_json::Value::Null);
    let ts = r.occurred_at.to_rfc3339();

    match r.kind.as_str() {
        kind::SESSION_STARTED => {
            let cwd_hash = payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| format!("{:x}", sha2::Sha256::digest(s.as_bytes())));
            tx.execute(
                r#"INSERT INTO sessions
                   (session_id, run_id, started_at, cwd_hash, model_provider, model)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                   ON CONFLICT(session_id) DO UPDATE SET
                     run_id = excluded.run_id,
                     model_provider = COALESCE(excluded.model_provider, model_provider),
                     model = COALESCE(excluded.model, model)"#,
                rusqlite::params![
                    r.session_id,
                    r.run_id,
                    ts,
                    cwd_hash,
                    payload.get("model_provider").and_then(|v| v.as_str()),
                    payload.get("model").and_then(|v| v.as_str()),
                ],
            )?;
        }
        kind::TURN_STARTED => {
            tx.execute(
                r#"INSERT INTO turns
                   (turn_id, session_id, run_id, started_at, status, input_chars)
                   VALUES (?1, ?2, ?3, ?4, 'running', ?5)
                   ON CONFLICT(turn_id) DO NOTHING"#,
                rusqlite::params![
                    r.turn_id,
                    r.session_id,
                    r.run_id,
                    ts,
                    payload.get("input_chars").and_then(|v| v.as_i64()),
                ],
            )?;
        }
        kind::TURN_COMPLETED => {
            // Ensure the row exists even if TurnStarted was lost.
            tx.execute(
                r#"INSERT INTO turns
                   (turn_id, session_id, run_id, finished_at, status)
                   VALUES (?1, ?2, ?3, ?4, 'completed')
                   ON CONFLICT(turn_id) DO NOTHING"#,
                rusqlite::params![r.turn_id, r.session_id, r.run_id, ts],
            )?;
            let reason = payload.get("termination_reason").and_then(|v| v.as_str());
            let status = match reason {
                Some("cancelled") => "cancelled",
                _ => "completed",
            };
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            tx.execute(
                r#"UPDATE turns SET
                     finished_at   = COALESCE(finished_at, ?2),
                     status        = ?3,
                     termination_reason = COALESCE(?4, termination_reason),
                     steps         = COALESCE(?5, steps),
                     model_calls   = COALESCE(?6, model_calls),
                     tool_calls    = COALESCE(?7, tool_calls),
                     retries       = COALESCE(?8, retries),
                     compactions   = COALESCE(?9, compactions),
                     cancel_count  = COALESCE(?10, cancel_count),
                     duration_ms   = COALESCE(?11, duration_ms)
                   WHERE turn_id = ?1"#,
                rusqlite::params![
                    r.turn_id,
                    ts,
                    status,
                    reason,
                    get_i("steps"),
                    get_i("model_calls"),
                    get_i("tool_calls"),
                    get_i("retries"),
                    get_i("compactions"),
                    get_i("cancel_count"),
                    get_i("duration_ms"),
                ],
            )?;
        }
        kind::MODEL_ATTEMPT_STARTED => {
            // attempt_id = event_id of the Started record; Finished updates
            // the row matched by (session_id, step_id).
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"INSERT INTO model_attempts
                   (attempt_id, session_id, run_id, turn_id, step_id, request_id,
                    provider, model, wire_protocol, started_at, status)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'running')
                   ON CONFLICT(attempt_id) DO NOTHING"#,
                rusqlite::params![
                    r.event_id,
                    r.session_id,
                    r.run_id,
                    r.turn_id,
                    r.step_id,
                    get("request_id"),
                    get("provider"),
                    get("model"),
                    get("wire_protocol"),
                    ts,
                ],
            )?;
        }
        kind::MODEL_ATTEMPT_FINISHED => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            let usage = payload.get("usage");
            let u_i = |key: &str| -> Option<i64> {
                usage.and_then(|u| u.get(key)).and_then(|v| v.as_i64())
            };
            // Update the running attempt for this step; if the Started
            // record was lost (projection gap), insert a finished-only row.
            let updated = tx.execute(
                r#"UPDATE model_attempts SET
                     finished_at          = ?3,
                     duration_ms          = COALESCE(?4, duration_ms),
                     status               = ?5,
                     error_class          = ?6,
                     http_status          = ?7,
                     retry_after_secs     = ?8,
                     provider_request_id  = ?9,
                     first_token_ms       = COALESCE(?18, first_token_ms),
                     attempts             = COALESCE(?10, attempts),
                     input_tokens         = ?11,
                     cached_input_tokens  = ?12,
                     cache_creation_tokens = ?13,
                     output_tokens        = ?14,
                     reasoning_tokens     = ?15,
                     total_tokens         = ?16,
                     estimated            = ?17
                   WHERE session_id = ?1 AND step_id = ?2 AND status = 'running'"#,
                rusqlite::params![
                    r.session_id,
                    r.step_id,
                    ts,
                    get_i("duration_ms"),
                    get("status"),
                    get("error_class"),
                    get_i("http_status"),
                    get_i("retry_after_secs"),
                    get("provider_request_id"),
                    get_i("attempts"),
                    u_i("input_tokens"),
                    u_i("cached_input_tokens"),
                    u_i("cache_creation_tokens"),
                    u_i("output_tokens"),
                    u_i("reasoning_tokens"),
                    u_i("total_tokens"),
                    usage.and_then(|u| u.get("estimated")).and_then(|v| v.as_i64()),
                    get_i("first_token_ms"),
                ],
            )?;
            if updated == 0 {
                tx.execute(
                    r#"INSERT INTO model_attempts
                       (attempt_id, session_id, run_id, turn_id, step_id, request_id,
                        attempts, started_at, finished_at, duration_ms, status,
                        error_class, http_status, retry_after_secs, provider_request_id,
                        first_token_ms,
                        input_tokens, cached_input_tokens, cache_creation_tokens,
                        output_tokens, reasoning_tokens, total_tokens, estimated)
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10,
                               ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
                       ON CONFLICT(attempt_id) DO NOTHING"#,
                    rusqlite::params![
                        r.event_id,
                        r.session_id,
                        r.run_id,
                        r.turn_id,
                        r.step_id,
                        get("request_id"),
                        get_i("attempts"),
                        ts,
                        get_i("duration_ms"),
                        get("status"),
                        get("error_class"),
                        get_i("http_status"),
                        get_i("retry_after_secs"),
                        get("provider_request_id"),
                        get_i("first_token_ms"),
                        u_i("input_tokens"),
                        u_i("cached_input_tokens"),
                        u_i("cache_creation_tokens"),
                        u_i("output_tokens"),
                        u_i("reasoning_tokens"),
                        u_i("total_tokens"),
                        usage.and_then(|u| u.get("estimated")).and_then(|v| v.as_i64()),
                    ],
                )?;
            }
        }
        kind::TOOL_PREPARED => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"INSERT INTO tool_executions
                   (session_id, call_id, run_id, turn_id, step_id, tool_name,
                    operation_id, prepared_at, status)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'prepared')
                   ON CONFLICT(session_id, call_id) DO UPDATE SET
                     tool_name    = COALESCE(excluded.tool_name, tool_name),
                     operation_id = COALESCE(excluded.operation_id, operation_id),
                     turn_id      = COALESCE(excluded.turn_id, turn_id),
                     step_id      = COALESCE(excluded.step_id, step_id)"#,
                rusqlite::params![
                    r.session_id,
                    r.call_id,
                    r.run_id,
                    r.turn_id,
                    r.step_id,
                    get("name"),
                    get("operation_id"),
                    ts,
                ],
            )?;
        }
        kind::TOOL_APPROVED => {
            tx.execute(
                r#"UPDATE tool_executions SET approved_at = ?3, status = 'approved'
                   WHERE session_id = ?1 AND call_id = ?2 AND approved_at IS NULL"#,
                rusqlite::params![r.session_id, r.call_id, ts],
            )?;
        }
        kind::TOOL_STARTED => {
            tx.execute(
                r#"UPDATE tool_executions SET started_at = ?3, status = 'running'
                   WHERE session_id = ?1 AND call_id = ?2 AND started_at IS NULL"#,
                rusqlite::params![r.session_id, r.call_id, ts],
            )?;
        }
        kind::TOOL_FINISHED => {
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            tx.execute(
                r#"UPDATE tool_executions SET
                     finished_at      = COALESCE(finished_at, ?3),
                     duration_ms      = COALESCE(?4, duration_ms),
                     exit_code        = COALESCE(?5, exit_code),
                     is_error         = COALESCE(?6, is_error),
                     output_truncated = COALESCE(?7, output_truncated),
                     status           = 'finished'
                   WHERE session_id = ?1 AND call_id = ?2"#,
                rusqlite::params![
                    r.session_id,
                    r.call_id,
                    ts,
                    get_i("duration_ms"),
                    get_i("exit_code"),
                    payload.get("is_error").and_then(|v| v.as_bool()).map(|b| b as i64),
                    payload.get("output_truncated").and_then(|v| v.as_bool()).map(|b| b as i64),
                ],
            )?;
        }
        kind::TOOL_RESULT_COMMITTED => {
            tx.execute(
                r#"UPDATE tool_executions SET committed_at = ?3, status = 'committed'
                   WHERE session_id = ?1 AND call_id = ?2 AND committed_at IS NULL"#,
                rusqlite::params![r.session_id, r.call_id, ts],
            )?;
        }
        kind::TOOL_INDETERMINATE => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            // Indeterminate events may lack turn/step (post-crash classification).
            tx.execute(
                r#"INSERT INTO tool_executions
                   (session_id, call_id, run_id, tool_name, operation_id, prepared_at, status)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'indeterminate')
                   ON CONFLICT(session_id, call_id) DO UPDATE SET status = 'indeterminate'"#,
                rusqlite::params![
                    r.session_id,
                    r.call_id,
                    r.run_id,
                    get("name"),
                    get("operation_id"),
                    ts,
                ],
            )?;
        }
        kind::TOOL_RESOLVED => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"UPDATE tool_executions SET status = ?3
                   WHERE session_id = ?1 AND call_id = ?2 AND status = 'indeterminate'"#,
                rusqlite::params![r.session_id, r.call_id, get("resolution")],
            )?;
        }
        kind::PROMPT_SNAPSHOT => {
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"INSERT OR IGNORE INTO prompt_builds
                   (prompt_id, session_id, run_id, turn_id, step_id,
                    prompt_snapshot_hash, context_item_count, estimated_input_tokens, built_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
                rusqlite::params![
                    r.event_id,
                    r.session_id,
                    r.run_id,
                    r.turn_id,
                    r.step_id,
                    get("prompt_snapshot_hash"),
                    get_i("context_item_count"),
                    get_i("estimated_input_tokens"),
                    ts,
                ],
            )?;
        }
        kind::COMPACTION_STARTED => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            tx.execute(
                r#"INSERT INTO compactions
                   (compaction_id, session_id, run_id, turn_id, trigger,
                    started_at, pre_item_count, status)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'started')
                   ON CONFLICT(compaction_id) DO NOTHING"#,
                rusqlite::params![
                    r.event_id,
                    r.session_id,
                    r.run_id,
                    r.turn_id,
                    get("trigger"),
                    ts,
                    get_i("pre_compaction_item_count"),
                ],
            )?;
        }
        kind::COMPACTION_CANDIDATE => {
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            tx.execute(
                r#"UPDATE compactions SET candidate_item_count = ?3
                   WHERE session_id = ?1 AND status = 'started'
                     AND compaction_id = (
                       SELECT compaction_id FROM compactions
                       WHERE session_id = ?1 AND turn_id IS ?2 AND status = 'started'
                       ORDER BY started_at DESC LIMIT 1)"#,
                rusqlite::params![r.session_id, r.turn_id, get_i("candidate_item_count")],
            )?;
        }
        kind::COMPACTION_COMMITTED => {
            tx.execute(
                r#"UPDATE compactions SET
                     finished_at = COALESCE(finished_at, ?3),
                     committed_at = ?3, status = 'committed'
                   WHERE session_id = ?1 AND status = 'started'
                     AND compaction_id = (
                       SELECT compaction_id FROM compactions
                       WHERE session_id = ?1 AND turn_id IS ?2 AND status = 'started'
                       ORDER BY started_at DESC LIMIT 1)"#,
                rusqlite::params![r.session_id, r.turn_id, ts],
            )?;
        }
        kind::COMPACTION_FAILED => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"UPDATE compactions SET
                     finished_at = COALESCE(finished_at, ?3),
                     status = 'failed', failure_reason = ?4
                   WHERE session_id = ?1 AND status = 'started'
                     AND compaction_id = (
                       SELECT compaction_id FROM compactions
                       WHERE session_id = ?1 AND turn_id IS ?2 AND status = 'started'
                       ORDER BY started_at DESC LIMIT 1)"#,
                rusqlite::params![r.session_id, r.turn_id, ts, get("reason")],
            )?;
        }
        kind::SUBAGENT_STARTED => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"INSERT INTO subagent_runs
                   (session_id, task_id, run_id, agent_id, parent_id, label, started_at, status)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'running')
                   ON CONFLICT(session_id, task_id) DO NOTHING"#,
                rusqlite::params![
                    r.session_id,
                    get("task_id"),
                    r.run_id,
                    get("agent_id"),
                    get("parent_id"),
                    get("label"),
                    ts,
                ],
            )?;
        }
        kind::SUBAGENT_FINISHED => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            let tokens = payload.get("tokens").and_then(|v| v.as_i64());
            tx.execute(
                r#"UPDATE subagent_runs SET
                     finished_at = COALESCE(finished_at, ?3),
                     status      = COALESCE(?4, status),
                     tokens      = COALESCE(?5, tokens),
                     error       = COALESCE(?6, error)
                   WHERE session_id = ?1 AND task_id = ?2"#,
                rusqlite::params![
                    r.session_id,
                    get("task_id"),
                    ts,
                    get("status"),
                    tokens,
                    get("error"),
                ],
            )?;
        }
        kind::SKILL_SNAPSHOT => {
            let generation = payload.get("skill_generation").and_then(|v| v.as_i64());
            let Some(skills) = payload.get("skills").and_then(|v| v.as_array()) else {
                return Ok(());
            };
            for sk in skills {
                let name = sk.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let get = |key: &str| -> Option<String> {
                    sk.get(key).and_then(|v| v.as_str()).map(str::to_string)
                };
                let activation_id = format!("{}:{}", r.event_id, name);
                tx.execute(
                    r#"INSERT OR IGNORE INTO skill_activations
                       (activation_id, session_id, run_id, turn_id, skill_name,
                        source, path, content_hash, skill_generation, loaded_at)
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
                    rusqlite::params![
                        activation_id,
                        r.session_id,
                        r.run_id,
                        r.turn_id,
                        name,
                        get("source"),
                        get("path"),
                        get("content_hash"),
                        generation,
                        ts,
                    ],
                )?;
            }
        }
        kind::MEMORY_RETRIEVAL => {
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"INSERT OR IGNORE INTO memory_retrievals
                   (retrieval_id, session_id, run_id, turn_id, query_chars,
                    selected_count, duration_ms, router_kind, occurred_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
                rusqlite::params![
                    r.event_id,
                    r.session_id,
                    r.run_id,
                    r.turn_id,
                    get_i("query_chars"),
                    get_i("selected_count"),
                    get_i("duration_ms"),
                    get("router_kind"),
                    ts,
                ],
            )?;
        }
        kind::MCP_LIFECYCLE => {
            let get_i = |key: &str| -> Option<i64> { payload.get(key).and_then(|v| v.as_i64()) };
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            tx.execute(
                r#"INSERT OR IGNORE INTO mcp_lifecycle
                   (event_id, session_id, run_id, server_name, phase, transport,
                    tool_count, status, error_class, duration_ms, occurred_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
                rusqlite::params![
                    r.event_id,
                    r.session_id,
                    r.run_id,
                    get("server_name"),
                    get("phase"),
                    get("transport"),
                    get_i("tool_count"),
                    get("status"),
                    get("error_class"),
                    get_i("duration_ms"),
                    ts,
                ],
            )?;
        }
        kind::APPROVAL_REQUESTED | kind::APPROVAL_RESOLVED | kind::LEASE_ISSUED
        | kind::LEASE_CONSUMED | kind::LEASE_EXPIRED | kind::CAPABILITY_REJECTED_STALE => {
            let get = |key: &str| -> Option<String> {
                payload.get(key).and_then(|v| v.as_str()).map(str::to_string)
            };
            let decision_type = match r.kind.as_str() {
                kind::APPROVAL_REQUESTED => "approval_requested",
                kind::APPROVAL_RESOLVED => "approval_resolved",
                kind::LEASE_ISSUED => "lease_issued",
                kind::LEASE_CONSUMED => "lease_consumed",
                kind::LEASE_EXPIRED => "lease_expired",
                _ => "capability_stale",
            };
            tx.execute(
                r#"INSERT OR IGNORE INTO security_decisions
                   (decision_id, session_id, run_id, turn_id, step_id, call_id,
                    operation_id, tool_name, ticket_id, lease_id,
                    decision_type, decision, reason, occurred_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"#,
                rusqlite::params![
                    r.event_id,
                    r.session_id,
                    r.run_id,
                    r.turn_id,
                    r.step_id,
                    r.call_id,
                    get("operation_id"),
                    get("tool_name"),
                    get("ticket_id"),
                    get("lease_id"),
                    decision_type,
                    get("resolution"),
                    get("reason"),
                    ts,
                ],
            )?;
            // Approval round-trip latency: resolved_at − requested_at,
            // stamped onto the tool's lifecycle row.
            if decision_type == "approval_resolved" {
                tx.execute(
                    r#"UPDATE tool_executions SET
                         approval_wait_ms = CAST((
                           julianday(?3) - julianday(
                             (SELECT occurred_at FROM security_decisions
                              WHERE session_id = ?1 AND ticket_id = ?2
                                AND decision_type = 'approval_requested'
                              ORDER BY occurred_at DESC LIMIT 1))
                         ) * 86400000 AS INTEGER)
                       WHERE session_id = ?1 AND call_id = (
                         SELECT call_id FROM security_decisions
                         WHERE session_id = ?1 AND ticket_id = ?2
                           AND decision_type = 'approval_requested'
                         ORDER BY occurred_at DESC LIMIT 1)"#,
                    rusqlite::params![r.session_id, get("ticket_id"), ts],
                )?;
            }
        }
        _ => {}
    }

    if let Some(seq) = r.journal_seq {
        tx.execute(
            r#"INSERT INTO projection_cursors (source, session_id, last_journal_seq, updated_at)
               VALUES ('journal', ?1, ?2, ?3)
               ON CONFLICT(source, session_id) DO UPDATE SET
                 last_journal_seq = MAX(last_journal_seq, excluded.last_journal_seq),
                 updated_at = excluded.updated_at"#,
            rusqlite::params![r.session_id, seq as i64, ts],
        )?;
    }
    Ok(())
}

/// Retention window in days — `GRODEX_TELEMETRY_RETENTION_DAYS` overrides
/// the 30-day default. `0` disables retention entirely.
fn retention_days() -> u32 {
    std::env::var("GRODEX_TELEMETRY_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30)
}

/// Best-effort startup retention: drop raw events older than the window.
/// Derived projection rows are left in place — they are tiny compared to
/// the event log and keep session/turn history readable.
pub fn retain(conn: &Connection, days: u32) {
    if days == 0 {
        return;
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
    let cutoff = cutoff.to_rfc3339();
    match conn.execute(
        "DELETE FROM telemetry_events WHERE occurred_at < ?1",
        rusqlite::params![cutoff],
    ) {
        Ok(n) if n > 0 => {
            tracing::info!(target: "grodex_telemetry", removed = n, days, "telemetry retention applied");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(target: "grodex_telemetry", error = %e, "telemetry retention failed (ignored)");
        }
    }
}
