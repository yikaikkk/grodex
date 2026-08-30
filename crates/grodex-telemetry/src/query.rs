//! Read-only query helpers over the telemetry projection — the data
//! source behind `grodex telemetry {sessions,turn,errors,slow-tools,
//! slow-models,doctor}`. SQL lives here; the CLI stays a formatter.

use rusqlite::Connection;

// ── Row types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub run_id: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub model_provider: Option<String>,
    pub model: Option<String>,
    pub turn_count: i64,
}

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub turn_id: String,
    pub session_id: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub status: String,
    pub termination_reason: Option<String>,
    pub steps: Option<i64>,
    pub model_calls: Option<i64>,
    pub tool_calls: Option<i64>,
    pub retries: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ToolAgg {
    pub tool_name: String,
    pub calls: i64,
    pub errors: i64,
    pub avg_ms: f64,
    pub max_ms: i64,
    pub avg_approval_wait_ms: f64,
}

#[derive(Debug, Clone)]
pub struct ModelAgg {
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub errors: i64,
    pub avg_ms: f64,
    pub max_ms: i64,
    pub avg_first_token_ms: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub total_input_tokens: i64,
    pub total_cached_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct ErrorRow {
    pub occurred_at: String,
    pub session_id: String,
    pub kind: String,
    pub status: Option<String>,
    pub call_id: Option<String>,
    pub journal_seq: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct DoctorReport {
    pub open_turns: i64,            // started but never finished (crash candidates)
    pub running_tools: i64,         // started_at set, finished_at NULL
    // committed_at NULL while finished
    pub uncommitted_results: i64,
    pub failed_attempts: i64,       // model attempts with status='error'
    pub indeterminate_tools: i64,
    pub in_flight_compactions: i64, // started but never committed/failed
    pub total_events: i64,
    pub total_sessions: i64,
}

// ── Queries ─────────────────────────────────────────────────────────

pub fn sessions(conn: &Connection, limit: u32) -> Result<Vec<SessionRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT s.session_id, s.run_id, s.started_at, s.finished_at,
                  s.model_provider, s.model,
                  (SELECT COUNT(*) FROM turns t WHERE t.session_id = s.session_id)
           FROM sessions s
           ORDER BY s.started_at DESC LIMIT ?1"#,
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(SessionRow {
            session_id: r.get(0)?,
            run_id: r.get(1)?,
            started_at: r.get(2)?,
            finished_at: r.get(3)?,
            model_provider: r.get(4)?,
            model: r.get(5)?,
            turn_count: r.get(6)?,
        })
    })?;
    rows.collect()
}

pub fn session_turns(conn: &Connection, session_id: &str) -> Result<Vec<TurnRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT turn_id, session_id, started_at, finished_at, status,
                  termination_reason, steps, model_calls, tool_calls, retries, duration_ms
           FROM turns WHERE session_id = ?1 ORDER BY started_at ASC"#,
    )?;
    let rows = stmt.query_map([session_id], map_turn)?;
    rows.collect()
}

pub fn turn(conn: &Connection, turn_id: &str) -> Result<Option<TurnRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT turn_id, session_id, started_at, finished_at, status,
                  termination_reason, steps, model_calls, tool_calls, retries, duration_ms
           FROM turns WHERE turn_id = ?1"#,
    )?;
    let mut rows = stmt.query_map([turn_id], map_turn)?;
    match rows.next() {
        Some(row) => Ok(Some(row?)),
        None => Ok(None),
    }
}

fn map_turn(r: &rusqlite::Row<'_>) -> Result<TurnRow, rusqlite::Error> {
    Ok(TurnRow {
        turn_id: r.get(0)?,
        session_id: r.get(1)?,
        started_at: r.get(2)?,
        finished_at: r.get(3)?,
        status: r.get(4)?,
        termination_reason: r.get(5)?,
        steps: r.get(6)?,
        model_calls: r.get(7)?,
        tool_calls: r.get(8)?,
        retries: r.get(9)?,
        duration_ms: r.get(10)?,
    })
}

/// Per-tool latency/error aggregates. `avg_approval_wait_ms` isolates
/// 审批等待 from 执行耗时 so "工具慢" can be attributed.
pub fn slow_tools(conn: &Connection, limit: u32) -> Result<Vec<ToolAgg>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT tool_name,
                  COUNT(*)                                                              AS calls,
                  SUM(COALESCE(is_error, 0))                                            AS errors,
                  AVG(duration_ms)                                                      AS avg_ms,
                  MAX(duration_ms)                                                      AS max_ms,
                  AVG(approval_wait_ms)                                                 AS avg_wait
           FROM tool_executions
           WHERE duration_ms IS NOT NULL
           GROUP BY tool_name
           ORDER BY avg_ms DESC
           LIMIT ?1"#,
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(ToolAgg {
            tool_name: r.get(0)?,
            calls: r.get(1)?,
            errors: r.get(2)?,
            avg_ms: r.get(3)?,
            max_ms: r.get(4)?,
            avg_approval_wait_ms: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// Per-model latency/error/cache aggregates. `cache_hit_rate` uses the
/// provider-reported cached tokens (billing truth), NOT the local prompt
/// hash — local hashes only indicate request stability.
pub fn slow_models(conn: &Connection, limit: u32) -> Result<Vec<ModelAgg>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT provider, model,
                  COUNT(*)                                                              AS calls,
                  SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END)                     AS errors,
                  AVG(duration_ms)                                                      AS avg_ms,
                  MAX(duration_ms)                                                      AS max_ms,
                  AVG(first_token_ms)                                                   AS avg_ttft,
                  SUM(COALESCE(input_tokens, 0))                                        AS input_toks,
                  SUM(COALESCE(cached_input_tokens, 0))                                 AS cached_toks
           FROM model_attempts
           WHERE status IN ('ok', 'error')
           GROUP BY provider, model
           ORDER BY avg_ms DESC
           LIMIT ?1"#,
    )?;
    let rows = stmt.query_map([limit], |r| {
        let input: i64 = r.get(6)?;
        let cached: i64 = r.get(7)?;
        Ok(ModelAgg {
            provider: r.get(0)?,
            model: r.get(1)?,
            calls: r.get(2)?,
            errors: r.get(3)?,
            avg_ms: r.get(4)?,
            max_ms: r.get(5)?,
            avg_first_token_ms: r.get(6)?,
            total_input_tokens: input,
            total_cached_tokens: cached,
            cache_hit_rate: if input > 0 {
                Some(cached as f64 / input as f64)
            } else {
                None
            },
        })
    })?;
    rows.collect()
}

/// Recent error-severity events across the whole projection.
pub fn errors(conn: &Connection, limit: u32) -> Result<Vec<ErrorRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT occurred_at, session_id, kind, status, call_id, journal_seq
           FROM telemetry_events
           WHERE severity = 'error'
           ORDER BY occurred_at DESC LIMIT ?1"#,
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(ErrorRow {
            occurred_at: r.get(0)?,
            session_id: r.get(1)?,
            kind: r.get(2)?,
            status: r.get(3)?,
            call_id: r.get(4)?,
            journal_seq: r.get(5)?,
        })
    })?;
    rows.collect()
}

/// Health check: lifecycle rows stuck in an intermediate state —
/// the direct answer to "这个 Turn 为什么卡住 / 崩溃后为什么进入
/// Indeterminate".
pub fn doctor(conn: &Connection) -> Result<DoctorReport, rusqlite::Error> {
    let mut report = DoctorReport::default();
    let one = |sql: &str| -> Result<i64, rusqlite::Error> {
        conn.query_row(sql, [], |r| r.get::<_, i64>(0))
    };
    report.open_turns = one(
        "SELECT COUNT(*) FROM turns WHERE finished_at IS NULL OR status = 'running'",
    )?;
    report.running_tools = one(
        "SELECT COUNT(*) FROM tool_executions WHERE started_at IS NOT NULL AND finished_at IS NULL",
    )?;
    report.uncommitted_results = one(
        "SELECT COUNT(*) FROM tool_executions WHERE finished_at IS NOT NULL AND committed_at IS NULL AND status != 'indeterminate'",
    )?;
    report.failed_attempts = one(
        "SELECT COUNT(*) FROM model_attempts WHERE status = 'error'",
    )?;
    report.indeterminate_tools = one(
        "SELECT COUNT(*) FROM tool_executions WHERE status = 'indeterminate'",
    )?;
    report.in_flight_compactions = one(
        "SELECT COUNT(*) FROM compactions WHERE status = 'started'",
    )?;
    report.total_events =
        one("SELECT COUNT(*) FROM telemetry_events")?;
    report.total_sessions = one("SELECT COUNT(*) FROM sessions")?;
    Ok(report)
}

/// The journal seq up to which the projection is complete, for one
/// session (used by re-projection bookkeeping / doctor output).
pub fn projection_cursor(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<i64>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT last_journal_seq FROM projection_cursors WHERE source='journal' AND session_id = ?1",
    )?;
    let mut rows = stmt.query_map([session_id], |r| r.get(0))?;
    match rows.next() {
        Some(v) => Ok(Some(v?)),
        None => Ok(None),
    }
}

/// Overall + per-model prompt-cache statistics (from `v_cache_stats`).
/// Numbers are the provider-reported cached tokens — billing truth, not
/// the local prompt hash.
#[derive(Debug, Clone)]
pub struct CacheStatsRow {
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_hit_rate: Option<f64>,
}

pub fn cache_stats(conn: &Connection) -> Result<Vec<CacheStatsRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT provider, model, calls,
                  COALESCE(input_tokens, 0), COALESCE(cached_input_tokens, 0),
                  COALESCE(cache_creation_tokens, 0), cache_hit_rate
           FROM v_cache_stats ORDER BY input_tokens DESC"#,
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(CacheStatsRow {
            provider: r.get(0)?,
            model: r.get(1)?,
            calls: r.get(2)?,
            input_tokens: r.get(3)?,
            cached_input_tokens: r.get(4)?,
            cache_creation_tokens: r.get(5)?,
            cache_hit_rate: r.get(6)?,
        })
    })?;
    rows.collect()
}

// ── P4: timeline / recovery / maintenance support ───────────────────

#[derive(Debug, Clone)]
pub struct TimelineRow {
    pub turn_id: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub status: String,
    pub termination_reason: Option<String>,
    pub duration_ms: Option<i64>,
    pub steps: Option<i64>,
    pub model_calls: Option<i64>,
    pub tool_calls: Option<i64>,
    pub retries: Option<i64>,
}

/// Chronological turn timeline for one session (from `v_session_timeline`).
pub fn timeline(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<TimelineRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT turn_id, started_at, finished_at, status, termination_reason,
                  duration_ms, steps, model_calls, tool_calls, retries
           FROM v_session_timeline WHERE session_id = ?1 ORDER BY started_at ASC"#,
    )?;
    let rows = stmt.query_map([session_id], |r| {
        Ok(TimelineRow {
            turn_id: r.get(0)?,
            started_at: r.get(1)?,
            finished_at: r.get(2)?,
            status: r.get(3)?,
            termination_reason: r.get(4)?,
            duration_ms: r.get(5)?,
            steps: r.get(6)?,
            model_calls: r.get(7)?,
            tool_calls: r.get(8)?,
            retries: r.get(9)?,
        })
    })?;
    rows.collect()
}

#[derive(Debug, Clone)]
pub struct RecoveryRow {
    pub anomaly: String,
    pub session_id: String,
    pub subject_id: Option<String>,
    pub occurred_at: Option<String>,
    pub detail: Option<String>,
}

/// Lifecycle anomalies across all sessions (from `v_recovery_anomalies`).
pub fn recovery_anomalies(conn: &Connection) -> Result<Vec<RecoveryRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        r#"SELECT anomaly, session_id, subject_id, occurred_at, detail
           FROM v_recovery_anomalies ORDER BY occurred_at DESC"#,
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(RecoveryRow {
            anomaly: r.get(0)?,
            session_id: r.get(1)?,
            subject_id: r.get(2)?,
            occurred_at: r.get(3)?,
            detail: r.get(4)?,
        })
    })?;
    rows.collect()
}
