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
use std::collections::{HashMap, HashSet};
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
    /// 幂等: 同一条 (rollout_id + content_hash) 不会重复写入。
    /// 建议在启动时的后台任务中调用,不要阻塞主线程。
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

        // 查询已提取过的 rollout_id, 避免重复扫描
        let extracted: HashSet<String> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT DISTINCT rollout_id FROM evidence_units"
            )?;
            let ids: HashSet<String> = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);
            drop(conn);
            ids
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
            if extracted.contains(&sdir_name) {
                report.sessions_skipped += 1;
                continue;
            }

            let journal = sdir.join("rollout.jsonl");
            if !journal.exists() {
                report.sessions_skipped += 1;
                continue;
            }
            match extract_single_session(self, &sdir_name, &journal, MAX_EVIDENCE_PER_SESSION) {
                Ok(n) => {
                    report.evidence_created += n;
                }
                Err(_) => {
                    report.failed_sessions += 1;
                }
            }
        }

        report.evidence_dedup = 0; // TODO: count deduplications inside loop if need
        Ok(report)
    }

    /// 抽取单一会话的 rollout.jsonl → EvidenceUnits（会话退出时用）。
    ///
    /// 与 [Self::extract_evidence_from_rollouts] 的区别：
    /// - 只处理指定 session_id 的 rollout.jsonl，不枚举 sessions 根目录
    /// - 不按 `evidence_units.rollout_id` 跳过会话（同一个会话的增量事件允许重复扫）；
    ///   去重由 `(rollout_id, content_hash)` 的 DB UNIQUE 约束保证（INSERT OR IGNORE）。
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
        extract_single_session(self, session_id, rollout_jsonl, MAX_EVIDENCE_PER_SESSION)
    }
}

fn extract_single_session(
    db: &MemoryDatabase,
    rollout_id: &str,
    journal: &Path,
    max_units: usize,
) -> Result<usize, DbError> {
    let file = match std::fs::File::open(journal) {
        Ok(f) => f,
        Err(_) => return Ok(0),
    };
    use std::io::{BufRead, BufReader};
    let reader = BufReader::new(file);

    let mut count = 0usize;
    let mut lines_read = 0usize;

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
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
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
            // ── Sub-agent 结束: 视为 assistant 的一部分 ────────────
            "SubAgentTaskFinished" => {
                if let Some(result) = payload
                    .get("final_result")
                    .or_else(|| payload.get("summary"))
                    .or_else(|| payload.get("result"))
                {
                    let s = match result {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let s = s.trim().to_string();
                    if !s.is_empty() && count < max_units {
                        if let Some(eu) = build_evidence(
                            rollout_id,
                            journal,
                            "subagent_summary",
                            &s,
                            MemoryScope::Workspace,
                            occurred,
                        ) {
                            let _ = db.upsert_evidence_unit(&eu);
                            count += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(count)
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
    Some(EvidenceUnit {
        id,
        rollout_id: rollout_id.to_string(),
        path,
        section: section.to_string(),
        scope,
        status: EvidenceStatus::Active,
        content,
        content_hash,
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
