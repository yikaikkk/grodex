//! 历史 Rollout → EvidenceUnit 提取器。
//!
//! 提取流程 (对齐真实 RolloutEvent schema, 见 grodex-rollout/src/event.rs):
//!   ~/.grodex/sessions/{session_id}/rollout.jsonl
//!     → 逐行读 JSONL 事件, 每行是一个序列化的 RolloutEvent:
//!          { schema_version, seq, session_id, turn_id?, step_id?, timestamp,
//!            event_type, payload: {...}, sensitivity }
//!     → 关键事件提取:
//!        · UserInputAccepted → payload.text → 用户问题 Evidence(kind=UserQuestion)
//!        · ModelItemProduced → payload.assistant_text (累计 turn_summary)
//!        · ToolCallPrepared / ToolCallApproved / ToolExecutionStarted
//!            → payload.{call_id, name} → call_id→name 映射
//!        · ToolResultCommitted → 记录 tool 执行结果 (call_id 反查 name)
//!        · TurnCompleted → 提交 turn_summary 作为 assistant 结论 Evidence
//!     → 生成 EvidenceUnit, 写入 evidence_units + evidence_fts
//!     → 同 session 已提取过的跳过 (通过 rollout_id 去重)
//!
//! 注意: 我们之前错误地使用了顶层字段 "event" + 匹配 "UserTurnStarted"
//! 等假枚举名, 导致 "0 命中"。现在统一读 "event_type" 字符串 +
//! payload 嵌套, 与 reducer / context_projection 内部使用的一致。
//! 提取结果用于 consolidation (P0-3) 做稳定 Memory 沉淀。

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::database::{DbError, MemoryDatabase};
use crate::types::{EvidenceStatus, EvidenceUnit, MemoryScope};

/// 单会话可提取的最大 evidence 数。
const MAX_EVIDENCE_PER_SESSION: usize = 50;
/// 单 session 的总处理上限, 防止百万级 jsonl。
const MAX_LINES_PER_SESSION: usize = 20_000;
/// 单条 Evidence 正文大小上限。
const MAX_EVIDENCE_CHARS: usize = 4000;

/// 提取报告。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ExtractionReport {
    pub sessions_scanned: usize,
    pub sessions_new: usize,
    pub sessions_skipped: usize,
    pub evidence_created: usize,
    pub evidence_dedup: usize,
    pub failed_sessions: usize,
}

impl MemoryDatabase {
    /// 扫描 ~/.grodex/sessions/ 下所有 rollout.jsonl, 提取 EvidenceUnits。
    ///
    /// (P0-12) Incremental cursor: per rollout, we remember how many
    /// journal lines were *successfully* processed as the
    /// `rollout_last_processed_seq` value (stored in memory_tasks as
    /// the highest `rollout_until_seq` row with status=succeeded).
    /// This means subsequent scans on a still-growing session only
    /// re-read from line (cursor + 1) onwards, so new turns appended
    /// after the first extraction pass are no longer silently skipped.
    ///
    /// For sessions first scanned under the old coarse dedup
    /// (DISTINCT rollout_id jump), we treat max_processed_seq=None as
    /// "re-scan from the top" — because the fingerprint UNIQUE
    /// constraint in `upsert_evidence_unit` already collapses
    /// identical rows, no duplicate evidence is produced.
    pub fn extract_evidence_from_rollouts(
        &self,
        sessions_root: &Path,
    ) -> Result<ExtractionReport, DbError> {
        let mut report = ExtractionReport::default();

        // 收集 session 目录
        let session_dirs: Vec<PathBuf> = match std::fs::read_dir(sessions_root) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| {
                    e.metadata()
                        .ok()
                        .map(|m| m.is_dir())
                        .unwrap_or(false)
                })
                .map(|e| e.path())
                .collect(),
            Err(_) => return Ok(report),
        };
        report.sessions_scanned = session_dirs.len();

        // P0-12: Read the current cursor for every session we might
        // touch. Prefetch into a HashMap so the inner loop avoids a
        // round-trip per session dir.
        let cursors: std::collections::HashMap<String, i64> = {
            let mut hm = std::collections::HashMap::new();
            for dir in &session_dirs {
                if let Some(name) = dir.file_name().map(|n| n.to_string_lossy().to_string()) {
                    if let Ok(Some(seq)) = self.rollout_last_processed_seq(&name) {
                        hm.insert(name, seq);
                    }
                }
            }
            hm
        };

        for sdir in session_dirs {
            let sdir_name = sdir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if sdir_name.is_empty() {
                continue;
            }
            report.sessions_new += 1;

            let journal = sdir.join("rollout.jsonl");
            if !journal.exists() {
                report.sessions_skipped += 1;
                continue;
            }
            // P0-12: skip the first N lines we already processed.
            let skip = cursors.get(&sdir_name).copied().unwrap_or(0).max(0) as usize;
            match extract_single_session(
                self,
                &sdir_name,
                &journal,
                MAX_EVIDENCE_PER_SESSION,
                skip,
            ) {
                Ok((evidence_count, lines_consumed)) => {
                    report.evidence_created += evidence_count;
                    // Durably record the new inclusive upper line for
                    // this rollout, idempotently (the UNIQUE key keeps
                    // successive identical scans from stacking).
                    let seq_end = (skip + lines_consumed) as i64;
                    if seq_end > 0 {
                        use crate::types::{MemoryTask, MemoryTaskStatus};
                        let tid = format!(
                            "memtask:rollout:{}:{}",
                            sdir_name, seq_end
                        );
                        let mut task = MemoryTask::rollout_task(
                            tid,
                            sdir_name.clone(),
                            seq_end,
                        );
                        task.status = MemoryTaskStatus::Succeeded;
                        if let Err(e) = self.enqueue_memory_task(&task) {
                            tracing::debug!(
                                rollout_id = %sdir_name,
                                seq_end,
                                error = %e,
                                "rollout cursor enqueue failed (extraction still committed)"
                            );
                        } else {
                            let _ = self.update_memory_task_status(
                                &format!(
                                    "memtask:rollout:{}:{}",
                                    sdir_name, seq_end
                                ),
                                MemoryTaskStatus::Succeeded,
                                Some(&format!(
                                    "evidence={evidence_count},lines={lines_consumed},skip={skip}"
                                )),
                            );
                        }
                    }
                }
                Err(_) => {
                    report.failed_sessions += 1;
                }
            }
        }

        report.evidence_dedup = 0; // fingerprint dedup happens inside upsert
        Ok(report)
    }

    /// 抽取单一会话的 rollout.jsonl → EvidenceUnits（会话退出时用）。
    ///
    /// 与 [Self::extract_evidence_from_rollouts] 的区别：
    /// - 只处理指定 session_id 的 rollout.jsonl，不枚举 sessions 根目录
    /// - 不按 rollout 跳过会话（同一个会话的增量事件允许重复扫）；
    ///   去重由 `fingerprint` UNIQUE 约束 + `upsert_evidence_unit` 内置
    ///   fingerprint 预查找保证。
    ///
    /// 幂等、fail-safe：`rollout.jsonl` 不存在 / 不可读时返回 `0`，不报错。
    pub fn extract_evidence_from_session(
        &self,
        session_id: &str,
        rollout_jsonl: &Path,
    ) -> Result<usize, DbError> {
        if !rollout_jsonl.exists() {
            return Ok(0);
        }
        extract_single_session(self, session_id, rollout_jsonl, MAX_EVIDENCE_PER_SESSION, 0)
            .map(|(ev, _)| ev)
    }
}

fn extract_single_session(
    db: &MemoryDatabase,
    rollout_id: &str,
    journal: &Path,
    max_units: usize,
    skip_first_n_lines: usize,
) -> Result<(usize, usize), DbError> {
    let file = match std::fs::File::open(journal) {
        Ok(f) => f,
        Err(_) => return Ok((0, 0)),
    };
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(file);

    let mut count = 0usize;
    let mut lines_read = 0usize;
    let mut lines_consumed = 0usize;

    // ── 跨事件 state ──────────────────────────────────────────────
    // call_id -> tool 名 (从 ToolCallPrepared / ToolCallApproved / ToolExecutionStarted 回推)
    let mut call_names: HashMap<String, String> = HashMap::new();
    // 当前 turn 累计的 assistant 正文
    let mut pending_assistant_texts: Vec<String> = Vec::new();
    // 当前 turn 的发生时间(取该 turn 最早事件)
    let mut pending_assistant_ts: Option<chrono::DateTime<Utc>> = None;

    for line in reader.lines() {
        if lines_read >= MAX_LINES_PER_SESSION {
            break;
        }
        lines_read += 1;
        // P0-12: skip the lines the last successful scan already
        // consumed. Still count them in lines_read so the line-number
        // caps above behave uniformly regardless of cursor position.
        if lines_read <= skip_first_n_lines {
            lines_consumed += 1;
            continue;
        }
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        lines_consumed += 1;
        let evt: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = evt
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let payload = evt
            .get("payload")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let occurred = pick_timestamp(&evt);

        match event_type {
            // ── 用户问题 ────────────────────────────────────────────
            "UserInputAccepted" => {
                if count >= max_units { break; }
                let text = payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if text.trim().is_empty() { continue; }
                if let Some(eu) = build_evidence(
                    rollout_id,
                    journal,
                    "用户问题",
                    text,
                    MemoryScope::Workspace,
                    occurred,
                ) {
                    let _ = db.upsert_evidence_unit(&eu);
                    count += 1;
                }
            }
            // ── 建立 call_id -> tool_name 映射 ──────────────────────
            "ToolCallPrepared" | "ToolCallApproved" | "ToolExecutionStarted" => {
                if let (Some(cid), Some(name)) = (
                    payload.get("call_id").and_then(|v| v.as_str()),
                    payload.get("name").and_then(|v| v.as_str()),
                ) {
                    call_names.entry(cid.to_string())
                        .or_insert_with(|| name.to_string());
                }
            }
            // ── Tool 结果（成功 & 失败都记） ────────────────────────
            // 优先 ToolResultCommitted (已被模型实际消费的版本), 如果
            // 错过了再退回 ToolExecutionFinished (crash 情况下仍然能
            // 捞到结果, 与 reducer 的 Finished-not-Committed 逻辑对称).
            "ToolResultCommitted" | "ToolExecutionFinished" => {
                if count >= max_units { break; }
                let call_id = payload.get("call_id").and_then(|v| v.as_str());
                let is_err = payload
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let content = payload
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if content.trim().is_empty() {
                    continue;
                }
                let tool_name = call_id
                    .and_then(|c| call_names.get(c).map(|s| s.as_str()))
                    .unwrap_or("tool");
                let truncated = if content.chars().count() > MAX_EVIDENCE_CHARS {
                    format!(
                        "...(truncated, {} chars)...\n{}",
                        content.chars().count(),
                        content.chars().take(MAX_EVIDENCE_CHARS).collect::<String>()
                    )
                } else {
                    content.to_string()
                };
                let prefix = if is_err { "tool_error" } else { "tool_result" };
                let section = format!("{prefix}:{tool_name}");
                let body = format!("[{tool_name}] {}", truncated);
                if let Some(eu) = build_evidence(
                    rollout_id,
                    journal,
                    &section,
                    &body,
                    MemoryScope::Workspace,
                    occurred,
                ) {
                    let _ = db.upsert_evidence_unit(&eu);
                    count += 1;
                }
            }
            // ── 模型正文: 累计 turn_summary, 到 TurnCompleted 提交 ──
            "ModelItemProduced" => {
                if let Some(t) = payload.get("assistant_text").and_then(|v| v.as_str()) {
                    let t = t.trim();
                    if !t.is_empty() {
                        pending_assistant_texts.push(t.to_string());
                        if pending_assistant_ts.is_none() {
                            pending_assistant_ts = Some(occurred);
                        }
                    }
                }
            }
            // ── Turn 终态: 提交 assistant 总结 Evidence ────────────
            "TurnCompleted" => {
                if count >= max_units {
                    // still reset state below
                } else {
                    let combined = pending_assistant_texts
                        .join("\n");
                    let combined = combined.trim();
                    if !combined.is_empty() {
                        let truncated = if combined.chars().count() > MAX_EVIDENCE_CHARS {
                            format!(
                                "...(truncated, {} chars)...\n{}",
                                combined.chars().count(),
                                combined.chars().take(MAX_EVIDENCE_CHARS).collect::<String>()
                            )
                        } else {
                            combined.to_string()
                        };
                        let ts = pending_assistant_ts.unwrap_or(occurred);
                        if let Some(eu) = build_evidence(
                            rollout_id,
                            journal,
                            "turn_summary",
                            &truncated,
                            MemoryScope::Workspace,
                            ts,
                        ) {
                            let _ = db.upsert_evidence_unit(&eu);
                            count += 1;
                        }
                    }
                }
                // Reset per-turn state.
                pending_assistant_texts.clear();
                pending_assistant_ts = None;
            }
            // ── Sub-agent 事件不提取 ────────────────────────────────
            // 用户约束：subagent 的所有输入来自主 agent，而非用户直接输入，
            // 因此 SubAgentTaskFinished 不应产生 evidence（避免污染长期记忆）。
            "SubAgentTaskFinished" => {}
            // ── 中间推理不提取：非稳定事实 ──────────────────────────
            "ModelItemProduced" => {}
            _ => {}
        }
    }
    Ok((count, lines_consumed))
}

fn build_evidence(
    rollout_id: &str,
    journal_path: &Path,
    section: &str,
    content: &str,
    scope: MemoryScope,
    occurred_at: chrono::DateTime<Utc>,
) -> Option<EvidenceUnit> {
    let content = content.trim().to_string();
    if content.is_empty() {
        return None;
    }
    let content_hash = {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    // 稳定 ID = ev_ + hash(rollout_id + section + content_hash) 前 16 位
    let mut id_hasher = Sha256::new();
    id_hasher.update(format!("{}:{}:{}", rollout_id, section, content_hash).as_bytes());
    let id_hash = format!("{:x}", id_hasher.finalize());
    let id = format!("ev_{}", &id_hash[..16]);
    let path = journal_path.to_string_lossy().to_string();
    let now = Utc::now();
    let fingerprint = EvidenceUnit::compute_fingerprint(
        rollout_id,
        &path,
        section,
        0,
        &content_hash,
    );
    Some(EvidenceUnit {
        id,
        rollout_id: rollout_id.to_string(),
        path,
        section: section.to_string(),
        scope,
        status: EvidenceStatus::Active,
        content,
        content_hash,
        fingerprint,
        occurred_at,
        created_at: now,
        superseded_by: None,
        superseded_at: None,
        rollout_available: true,
        rollout_expired_at: None,
        subchunk_index: 0,
    })
}

// ─────── JSON Value 辅助提取 ───────

fn pick_user_content(evt: &Value) -> String {
    evt.get("content")
        .or_else(|| evt.get("text"))
        .or_else(|| evt.get("prompt"))
        .or_else(|| evt.get("user"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn pick_tool_result(evt: &Value) -> String {
    evt.get("result")
        .or_else(|| evt.get("output"))
        .or_else(|| evt.get("response"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn pick_assistant_summary(evt: &Value) -> String {
    evt.get("summary")
        .or_else(|| evt.get("final_answer"))
        .or_else(|| evt.get("assistant"))
        .or_else(|| evt.get("text"))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn pick_timestamp(evt: &Value) -> chrono::DateTime<Utc> {
    evt.get("timestamp")
        .or_else(|| evt.get("time"))
        .or_else(|| evt.get("ts"))
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write each JSON value as a line into a rollout.jsonl under `dir`.
    fn write_rollout(dir: &std::path::Path, events: &[serde_json::Value]) -> std::path::PathBuf {
        let journal = dir.join("rollout.jsonl");
        let mut f = std::fs::File::create(&journal).unwrap();
        for evt in events {
            writeln!(f, "{}", evt).unwrap();
        }
        f
        .flush()
        .unwrap();
        journal
    }

    fn mk_event(event_type: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "event_type": event_type,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "payload": payload,
        })
    }

    /// SubAgentTaskFinished must NOT produce evidence (user constraint:
    /// subagent inputs come from the main agent, not the user directly).
    #[test]
    fn subagent_task_finished_produces_no_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = write_rollout(tmp.path(), &[
            mk_event("SubAgentTaskFinished", serde_json::json!({
                "final_result": "subagent did some work",
                "summary": "subagent summary",
            })),
            mk_event("SubAgentTaskFinished", serde_json::json!({
                "result": "another subagent result",
            })),
        ]);

        let db = MemoryDatabase::open_in_memory().unwrap();
        let n = db.extract_evidence_from_session("test_subagent", &journal).unwrap();
        assert_eq!(n, 0, "SubAgentTaskFinished must not produce evidence");
        // DB should also have zero evidence units.
        let count = db.conn.lock().unwrap()
            .query_row("SELECT COUNT(*) FROM evidence_units", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// ModelItemProduced (intermediate reasoning) must NOT produce evidence.
    #[test]
    fn model_item_produced_produces_no_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = write_rollout(tmp.path(), &[
            mk_event("ModelItemProduced", serde_json::json!({
                "text": "thinking about the problem",
            })),
        ]);

        let db = MemoryDatabase::open_in_memory().unwrap();
        let n = db.extract_evidence_from_session("test_model_item", &journal).unwrap();
        assert_eq!(n, 0, "ModelItemProduced must not produce evidence");
    }

    /// Control: UserInputAccepted DOES produce evidence (positive case).
    #[test]
    fn user_input_accepted_produces_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = write_rollout(tmp.path(), &[
            mk_event("UserInputAccepted", serde_json::json!({
                "text": "记住我喜欢 Rust",
            })),
        ]);

        let db = MemoryDatabase::open_in_memory().unwrap();
        let n = db.extract_evidence_from_session("test_user_input", &journal).unwrap();
        assert_eq!(n, 1, "UserInputAccepted should produce 1 evidence unit");
    }
}
