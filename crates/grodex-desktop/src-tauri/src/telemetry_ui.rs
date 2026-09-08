//! Observability surface for the desktop UI — read-only queries over
//! `~/.grodex/telemetry.db`.
//!
//! The `grodex serve` sidecar writes the telemetry DB (SQLite WAL); this
//! process opens it read-only and is concurrency-safe against the writer,
//! exactly like the memory panel's `memory_ui.rs` reads `~/.grodex/memory.db`.
//! No ACP protocol changes — the projection is already populated at runtime.

use std::path::PathBuf;

use grodex_telemetry::query;
use serde::Serialize;

// ── DTOs (serde camelCase for the React frontend) ────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAggDto {
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatsDto {
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_hit_rate: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryOverview {
    pub sessions: i64,
    pub turns: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cached_tokens: i64,
    pub total_cache_creation_tokens: i64,
    pub overall_cache_hit_rate: Option<f64>,
    pub models: Vec<ModelAggDto>,
    pub cache: Vec<CacheStatsDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnModelAttemptDto {
    pub provider: String,
    pub model: String,
    pub attempts: Option<i64>,
    pub status: Option<String>,
    pub error_class: Option<String>,
    pub http_status: Option<i64>,
    pub duration_ms: Option<i64>,
    pub first_token_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRetrievalDto {
    pub turn_id: Option<String>,
    pub query_chars: Option<i64>,
    pub selected_count: Option<i64>,
    pub duration_ms: Option<i64>,
    pub router_kind: Option<String>,
    pub occurred_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnDto {
    pub turn_id: String,
    pub status: String,
    pub termination_reason: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub steps: Option<i64>,
    pub model_calls: Option<i64>,
    pub tool_calls: Option<i64>,
    pub retries: Option<i64>,
    pub attempts: Vec<TurnModelAttemptDto>,
    pub memory_retrievals: Vec<MemoryRetrievalDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub session_id: String,
    pub turns: Vec<TurnDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDto {
    pub occurred_at: String,
    pub session_id: String,
    pub kind: String,
    pub status: Option<String>,
    pub call_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorDto {
    pub open_turns: i64,
    pub running_tools: i64,
    pub uncommitted_results: i64,
    pub failed_attempts: i64,
    pub indeterminate_tools: i64,
    pub in_flight_compactions: i64,
    pub total_events: i64,
    pub total_sessions: i64,
    pub errors: Vec<ErrorDto>,
}

// ── DB access ─────────────────────────────────────────────────────────

fn db_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("GRODEX_TELEMETRY_DB") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| "无法解析 HOME".to_string())?;
    Ok(home.join(".grodex").join("telemetry.db"))
}

fn open_db() -> Result<rusqlite::Connection, String> {
    let p = db_path()?;
    if !p.exists() {
        return Err(format!(
            "可观测数据库不存在：{}（先跑过至少一次会话才会生成）",
            p.display()
        ));
    }
    rusqlite::Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("打开可观测数据库失败 ({}): {e}", p.display()))
}

// ── Commands ──────────────────────────────────────────────────────────

/// Global overview: session/turn counts, token totals, overall cache hit
/// rate, and per-model latency/cache aggregates.
#[tauri::command]
pub fn telemetry_overview() -> Result<TelemetryOverview, String> {
    let conn = open_db()?;
    let t = query::overview(&conn).map_err(|e| e.to_string())?;

    let models = query::slow_models(&conn, 50)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| ModelAggDto {
            provider: r.provider,
            model: r.model,
            calls: r.calls,
            errors: r.errors,
            avg_ms: r.avg_ms,
            max_ms: r.max_ms,
            avg_first_token_ms: r.avg_first_token_ms,
            cache_hit_rate: r.cache_hit_rate,
            total_input_tokens: r.total_input_tokens,
            total_cached_tokens: r.total_cached_tokens,
        })
        .collect();

    let cache = query::cache_stats(&conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|r| CacheStatsDto {
            provider: r.provider,
            model: r.model,
            calls: r.calls,
            input_tokens: r.input_tokens,
            cached_input_tokens: r.cached_input_tokens,
            cache_creation_tokens: r.cache_creation_tokens,
            cache_hit_rate: r.cache_hit_rate,
        })
        .collect();

    Ok(TelemetryOverview {
        sessions: t.sessions,
        turns: t.turns,
        total_input_tokens: t.input_tokens,
        total_output_tokens: t.output_tokens,
        total_cached_tokens: t.cached_input_tokens,
        total_cache_creation_tokens: t.cache_creation_tokens,
        overall_cache_hit_rate: t.overall_cache_hit_rate,
        models,
        cache,
    })
}

/// Per-session drill-down: one row per turn with its model-attempt detail
/// (TTFT / tokens / cache) and memory-retrieval latency.
#[tauri::command]
pub fn telemetry_session(session_id: String) -> Result<SessionDetail, String> {
    let conn = open_db()?;
    let turns = query::session_turns(&conn, &session_id).map_err(|e| e.to_string())?;

    // Session-scoped memory retrievals, bucketed by turn for the join below.
    let mut mem_by_turn: std::collections::HashMap<String, Vec<MemoryRetrievalDto>> =
        std::collections::HashMap::new();
    for m in query::memory_retrieval_latency(&conn, &session_id).map_err(|e| e.to_string())? {
        if let Some(tid) = m.turn_id.clone() {
            mem_by_turn.entry(tid).or_default().push(MemoryRetrievalDto {
                turn_id: m.turn_id,
                query_chars: m.query_chars,
                selected_count: m.selected_count,
                duration_ms: m.duration_ms,
                router_kind: m.router_kind,
                occurred_at: m.occurred_at,
            });
        }
    }

    let mut out = Vec::with_capacity(turns.len());
    for t in turns {
        let attempts = query::turn_model_attempts(&conn, &t.turn_id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|a| TurnModelAttemptDto {
                provider: a.provider,
                model: a.model,
                attempts: a.attempts,
                status: a.status,
                error_class: a.error_class,
                http_status: a.http_status,
                duration_ms: a.duration_ms,
                first_token_ms: a.first_token_ms,
                input_tokens: a.input_tokens,
                cached_input_tokens: a.cached_input_tokens,
                cache_creation_tokens: a.cache_creation_tokens,
                output_tokens: a.output_tokens,
                reasoning_tokens: a.reasoning_tokens,
                total_tokens: a.total_tokens,
            })
            .collect();
        let memory_retrievals = mem_by_turn.remove(&t.turn_id).unwrap_or_default();
        out.push(TurnDto {
            turn_id: t.turn_id,
            status: t.status,
            termination_reason: t.termination_reason,
            started_at: t.started_at,
            finished_at: t.finished_at,
            duration_ms: t.duration_ms,
            steps: t.steps,
            model_calls: t.model_calls,
            tool_calls: t.tool_calls,
            retries: t.retries,
            attempts,
            memory_retrievals,
        });
    }
    Ok(SessionDetail {
        session_id,
        turns: out,
    })
}

/// Health check: lifecycle rows stuck mid-flight plus recent error events.
#[tauri::command]
pub fn telemetry_doctor() -> Result<DoctorDto, String> {
    let conn = open_db()?;
    let d = query::doctor(&conn).map_err(|e| e.to_string())?;
    let errors = query::errors(&conn, 50)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|e| ErrorDto {
            occurred_at: e.occurred_at,
            session_id: e.session_id,
            kind: e.kind,
            status: e.status,
            call_id: e.call_id,
        })
        .collect();
    Ok(DoctorDto {
        open_turns: d.open_turns,
        running_tools: d.running_tools,
        uncommitted_results: d.uncommitted_results,
        failed_attempts: d.failed_attempts,
        indeterminate_tools: d.indeterminate_tools,
        in_flight_compactions: d.in_flight_compactions,
        total_events: d.total_events,
        total_sessions: d.total_sessions,
        errors,
    })
}
