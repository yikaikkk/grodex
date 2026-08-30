//! `grodex telemetry` — read-only queries over `~/.grodex/telemetry.db`.
//!
//! The telemetry DB is a query projection of the rollout journal; these
//! commands answer: 这个 Turn 为什么结束、哪个工具慢/审批慢、哪个模型
//! 慢/在重试、崩溃后停在哪。SQL lives in `grodex-telemetry::query`;
//! this module only formats.

use std::path::PathBuf;

use grodex_telemetry::query;

fn default_db_path() -> Option<PathBuf> {
    std::env::var("GRODEX_TELEMETRY_DB")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".grodex").join("telemetry.db")))
}

fn open_db(explicit: Option<&String>) -> Result<rusqlite::Connection, String> {
    let path = match (explicit, default_db_path()) {
        (Some(p), _) => PathBuf::from(p),
        (None, Some(p)) => p,
        (None, None) => return Err("无法定位 telemetry.db（无 HOME 目录，且未指定 --db）".into()),
    };
    if !path.exists() {
        return Err(format!("telemetry.db 不存在：{}（先运行一次 grodex 会话）", path.display()));
    }
    rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| format!("打开 telemetry.db 失败：{e}"))
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn fmt_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        format!("{ms:.0}ms")
    }
}

pub fn sessions(db: Option<&String>) -> Result<(), String> {
    let conn = open_db(db)?;
    let rows = query::sessions(&conn, 50).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("（无会话记录）");
        return Ok(());
    }
    println!(
        "{:<38} {:<12} {:<10} {:>5}  {:<20} {}",
        "SESSION", "RUN", "TURNS", "", "MODEL", "STARTED"
    );
    for r in rows {
        println!(
            "{:<38} {:<12} {:<10} {:>5}  {:<20} {}",
            trunc(&r.session_id, 38),
            trunc(&r.run_id, 12),
            r.turn_count,
            "",
            match (&r.model_provider, &r.model) {
                (Some(p), Some(m)) => format!("{p}/{m}"),
                _ => "-".into(),
            },
            r.started_at.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

pub fn session(db: Option<&String>, session_id: &str) -> Result<(), String> {
    let conn = open_db(db)?;
    let turns = query::session_turns(&conn, session_id).map_err(|e| e.to_string())?;
    if turns.is_empty() {
        return Err(format!("未找到会话 {session_id} 的 Turn 记录"));
    }
    println!(
        "{:<38} {:<11} {:<9} {:>5} {:>6} {:>6} {:>7} {:>9}  {}",
        "TURN", "STATUS", "REASON", "STEPS", "MODEL", "TOOLS", "RETRIES", "DURATION", "STARTED"
    );
    for t in &turns {
        println!(
            "{:<38} {:<11} {:<9} {:>5} {:>6} {:>6} {:>7} {:>9}  {}",
            trunc(&t.turn_id, 38),
            t.status,
            t.termination_reason.as_deref().unwrap_or("-"),
            t.steps.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            t.model_calls.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            t.tool_calls.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            t.retries.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            t.duration_ms.map(|v| fmt_ms(v as f64)).unwrap_or_else(|| "-".into()),
            t.started_at.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

pub fn turn_detail(db: Option<&String>, turn_id: &str) -> Result<(), String> {
    let conn = open_db(db)?;
    let Some(t) = query::turn(&conn, turn_id).map_err(|e| e.to_string())? else {
        return Err(format!("未找到 Turn {turn_id}"));
    };
    println!("turn        {}", t.turn_id);
    println!("session     {}", t.session_id);
    println!("status      {} ({})", t.status, t.termination_reason.as_deref().unwrap_or("-"));
    println!("started     {}", t.started_at.as_deref().unwrap_or("-"));
    println!("finished    {}", t.finished_at.as_deref().unwrap_or("-"));
    println!("duration    {}", t.duration_ms.map(|v| fmt_ms(v as f64)).unwrap_or_else(|| "-".into()));
    println!(
        "counters    steps={} model_calls={} tool_calls={} retries={}",
        t.steps.unwrap_or(0),
        t.model_calls.unwrap_or(0),
        t.tool_calls.unwrap_or(0),
        t.retries.unwrap_or(0),
    );

    // Model attempts for this turn.
    let mut stmt = conn
        .prepare(
            r#"SELECT started_at, provider, model, attempts, duration_ms, status,
                      error_class, http_status, retry_after_secs,
                      input_tokens, cached_input_tokens, first_token_ms
               FROM model_attempts WHERE turn_id = ?1 ORDER BY started_at ASC"#,
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, String, i64, Option<i64>, String, Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>)> = stmt
        .query_map([turn_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    if !rows.is_empty() {
        println!("model attempts:");
        for (started, provider, model, attempts, dur, status, err, http, retry_after, input, cached, ttft) in &rows {
            let mut line = format!(
                "  {started}  {provider}/{model}  {status} x{attempts}  {}",
                dur.map(|v| fmt_ms(v as f64)).unwrap_or_else(|| "-".into())
            );
            if let Some(e) = err {
                line.push_str(&format!("  error={e}"));
            }
            if let Some(h) = http {
                line.push_str(&format!("  http={h}"));
            }
            if let Some(ra) = retry_after {
                line.push_str(&format!("  retry_after={ra}s"));
            }
            if let Some(t) = ttft {
                line.push_str(&format!("  ttft={}", fmt_ms(*t as f64)));
            }
            if let (Some(i), Some(c)) = (input, cached) {
                if *i > 0 {
                    line.push_str(&format!("  cache={:.0}%", *c as f64 / *i as f64 * 100.0));
                }
            }
            println!("{line}");
        }
    }

    // Tool executions for this turn.
    let mut stmt = conn
        .prepare(
            r#"SELECT tool_name, prepared_at, started_at, finished_at, committed_at,
                      approval_wait_ms, duration_ms, is_error, status
               FROM tool_executions WHERE turn_id = ?1 ORDER BY prepared_at ASC"#,
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<i64>, Option<i64>, Option<i64>, String)> = stmt
        .query_map([turn_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    if !rows.is_empty() {
        println!("tool executions:");
        for (name, prepared, started, finished, committed, wait, dur, is_err, status) in &rows {
            let mut line = format!("  {:<12} {status:<13}", name.as_deref().unwrap_or("?"));
            line.push_str(&format!(" exec={}", dur.map(|v| fmt_ms(v as f64)).unwrap_or_else(|| "-".into())));
            if let Some(w) = wait {
                line.push_str(&format!(" approval_wait={}", fmt_ms(*w as f64)));
            }
            if *is_err == Some(1) {
                line.push_str(" ERROR");
            }
            if started.is_some() && finished.is_none() {
                line.push_str("  ← started but never finished（崩溃/卡住）");
            } else if finished.is_some() && committed.is_none() && status != "indeterminate" {
                line.push_str("  ← executed but result not committed");
            }
            let _ = prepared;
            println!("{line}");
        }
    }
    Ok(())
}

pub fn errors(db: Option<&String>, limit: u32) -> Result<(), String> {
    let conn = open_db(db)?;
    let rows = query::errors(&conn, limit).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("（无 error 级事件）");
        return Ok(());
    }
    println!("{:<25} {:<18} {:<8} {}  {}", "TIME", "SESSION", "KIND", "CALL", "SEQ");
    for r in &rows {
        println!(
            "{:<25} {:<18} {:<8} {:<24} {}",
            r.occurred_at,
            trunc(&r.session_id, 18),
            r.kind,
            r.call_id.as_deref().unwrap_or("-"),
            r.journal_seq.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
        );
    }
    Ok(())
}

pub fn slow_tools(db: Option<&String>, limit: u32) -> Result<(), String> {
    let conn = open_db(db)?;
    let rows = query::slow_tools(&conn, limit).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("（无工具耗时记录）");
        return Ok(());
    }
    println!(
        "{:<16} {:>6} {:>7} {:>9} {:>9} {:>12}",
        "TOOL", "CALLS", "ERRORS", "AVG", "MAX", "AVG_APPROVAL_WAIT"
    );
    for r in &rows {
        println!(
            "{:<16} {:>6} {:>7} {:>9} {:>9} {:>12}",
            trunc(&r.tool_name, 16),
            r.calls,
            r.errors,
            fmt_ms(r.avg_ms),
            fmt_ms(r.max_ms as f64),
            if r.avg_approval_wait_ms > 0.0 {
                fmt_ms(r.avg_approval_wait_ms)
            } else {
                "-".into()
            },
        );
    }
    Ok(())
}

pub fn slow_models(db: Option<&String>, limit: u32) -> Result<(), String> {
    let conn = open_db(db)?;
    let rows = query::slow_models(&conn, limit).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("（无模型调用记录）");
        return Ok(());
    }
    println!(
        "{:<14} {:<20} {:>6} {:>7} {:>9} {:>9} {:>10} {:>12}",
        "PROVIDER", "MODEL", "CALLS", "ERRORS", "AVG", "MAX", "CACHE_HIT", "INPUT_TOKENS"
    );
    for r in &rows {
        println!(
            "{:<14} {:<20} {:>6} {:>7} {:>9} {:>9} {:>10} {:>12}",
            trunc(&r.provider, 14),
            trunc(&r.model, 20),
            r.calls,
            r.errors,
            fmt_ms(r.avg_ms),
            fmt_ms(r.max_ms as f64),
            r.cache_hit_rate
                .map(|c| format!("{:.1}%", c * 100.0))
                .unwrap_or_else(|| "-".into()),
            r.total_input_tokens,
        );
    }
    Ok(())
}

pub fn doctor(db: Option<&String>) -> Result<(), String> {
    let conn = open_db(db)?;
    let r = query::doctor(&conn).map_err(|e| e.to_string())?;
    println!("telemetry doctor");
    println!("  sessions              {}", r.total_sessions);
    println!("  events                {}", r.total_events);
    println!("  open turns            {}{}", r.open_turns, if r.open_turns > 0 { "  ← started but never finished（崩溃候选）" } else { "" });
    println!("  running tools         {}{}", r.running_tools, if r.running_tools > 0 { "  ← started but never finished" } else { "" });
    println!("  uncommitted results   {}{}", r.uncommitted_results, if r.uncommitted_results > 0 { "  ← executed but not committed" } else { "" });
    println!("  indeterminate tools   {}", r.indeterminate_tools);
    println!("  in-flight compactions {}{}", r.in_flight_compactions, if r.in_flight_compactions > 0 { "  ← started but never committed/failed" } else { "" });
    println!("  failed model attempts {}", r.failed_attempts);
    if r.total_events == 0 {
        println!("\n（数据库为空 — 遥测尚未记录任何事件）");
    }
    Ok(())
}

pub fn cache(db: Option<&String>) -> Result<(), String> {
    let conn = open_db(db)?;
    let rows = query::cache_stats(&conn).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("（无缓存数据 — 尚未记录任何带 usage 的模型调用）");
        return Ok(());
    }
    println!(
        "{:<14} {:<20} {:>6} {:>14} {:>14} {:>12} {:>10}",
        "PROVIDER", "MODEL", "CALLS", "INPUT_TOKENS", "CACHED", "CREATED", "HIT_RATE"
    );
    let mut total_input = 0i64;
    let mut total_cached = 0i64;
    for r in &rows {
        total_input += r.input_tokens;
        total_cached += r.cached_input_tokens;
        println!(
            "{:<14} {:<20} {:>6} {:>14} {:>14} {:>12} {:>10}",
            trunc(&r.provider, 14),
            trunc(&r.model, 20),
            r.calls,
            r.input_tokens,
            r.cached_input_tokens,
            r.cache_creation_tokens,
            r.cache_hit_rate
                .map(|c| format!("{:.1}%", c * 100.0))
                .unwrap_or_else(|| "-".into()),
        );
    }
    let overall = if total_input > 0 {
        Some(total_cached as f64 / total_input as f64)
    } else {
        None
    };
    println!(
        "{:<14} {:<20} {:>6} {:>14} {:>14} {:>12} {:>10}",
        "TOTAL", "", "", total_input, total_cached, "", "",
    );
    if let Some(c) = overall {
        println!("\noverall cache hit rate: {:.1}%", c * 100.0);
    }
    Ok(())
}

pub fn timeline(db: Option<&String>, session_id: &str) -> Result<(), String> {
    let conn = open_db(db)?;
    let rows = query::timeline(&conn, session_id).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        return Err(format!("未找到会话 {session_id} 的 timeline 记录"));
    }
    println!(
        "{:<38} {:<11} {:<22} {:>9} {:>5} {:>5} {:>5} {:>7}  {}",
        "TURN", "STATUS", "REASON", "DURATION", "STEPS", "MODEL", "TOOLS", "RETRIES", "STARTED"
    );
    for r in &rows {
        println!(
            "{:<38} {:<11} {:<22} {:>9} {:>5} {:>5} {:>5} {:>7}  {}",
            trunc(&r.turn_id, 38),
            r.status,
            r.termination_reason.as_deref().unwrap_or("-"),
            r.duration_ms.map(|v| fmt_ms(v as f64)).unwrap_or_else(|| "-".into()),
            r.steps.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            r.model_calls.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            r.tool_calls.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            r.retries.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            r.started_at.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

pub fn recovery(db: Option<&String>) -> Result<(), String> {
    let conn = open_db(db)?;
    let rows = query::recovery_anomalies(&conn).map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("（无生命周期异常 — 所有 Turn/工具/结果都已闭环）");
        return Ok(());
    }
    println!("{:<22} {:<18} {:<38} {:<25} {}", "ANOMALY", "SESSION", "SUBJECT", "OCCURRED", "DETAIL");
    for r in &rows {
        println!(
            "{:<22} {:<18} {:<38} {:<25} {}",
            r.anomaly,
            trunc(&r.session_id, 18),
            r.subject_id.as_deref().unwrap_or("-"),
            r.occurred_at.as_deref().unwrap_or("-"),
            r.detail.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

/// Checkpoint WAL + VACUUM. Opens read-write (unlike the query commands).
pub fn vacuum(db: Option<&String>) -> Result<(), String> {
    let path = match (db, default_db_path()) {
        (Some(p), _) => PathBuf::from(p),
        (None, Some(p)) => p,
        (None, None) => return Err("无法定位 telemetry.db".into()),
    };
    if !path.exists() {
        return Err(format!("telemetry.db 不存在：{}", path.display()));
    }
    let conn = rusqlite::Connection::open(&path).map_err(|e| format!("打开失败：{e}"))?;
    let size_before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
        .map_err(|e| format!("vacuum 失败：{e}"))?;
    let size_after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "vacuum 完成：{} → {} 字节",
        fmt_bytes(size_before),
        fmt_bytes(size_after)
    );
    Ok(())
}

fn fmt_bytes(n: u64) -> String {
    if n >= 1_048_576 {
        format!("{:.1}MB", n as f64 / 1_048_576.0)
    } else if n >= 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{n}B")
    }
}

/// Export raw telemetry events as JSONL (optionally one session) to a
/// file or stdout — the interchange format for external analysis.
pub fn export(db: Option<&String>, session_id: Option<&str>, output: Option<&str>) -> Result<(), String> {
    let conn = open_db(db)?;
    let (sql, param): (&str, Vec<&str>) = match session_id {
        Some(sid) => (
            "SELECT payload_json FROM telemetry_events WHERE session_id = ?1 ORDER BY occurred_at ASC",
            vec![sid],
        ),
        None => ("SELECT payload_json FROM telemetry_events ORDER BY occurred_at ASC", vec![]),
    };
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query(rusqlite::params_from_iter(param)).map_err(|e| e.to_string())?;

    let mut out: Box<dyn std::io::Write> = match output {
        Some(path) => Box::new(std::fs::File::create(path).map_err(|e| format!("创建输出文件失败：{e}"))?),
        None => Box::new(std::io::stdout().lock()),
    };
    let mut count = 0usize;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let payload: String = row.get(0).map_err(|e| e.to_string())?;
        writeln!(out, "{payload}").map_err(|e| format!("写出失败：{e}"))?;
        count += 1;
    }
    if output.is_some() {
        println!("已导出 {count} 条事件");
    }
    Ok(())
}
