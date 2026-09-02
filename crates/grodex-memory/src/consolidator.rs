//! Evidence → Memory 合并/提升引擎 (Phase 2 Consolidation)。
//!
//! 流程:
//! 1. 扫描所有 active evidence,按 content_hash 前缀分组 (相似内容归桶)
//! 2. ≥ MIN_OCCURRENCES 相同或近似结论 → 形成稳定 Memory
//! 3. 特殊模式快速提升: 同一用户问题重复 ≥2 次 = Preference
//! 4. 通过 ConsolidationTx 状态机保障可恢复
//! 5. 提升后: 为 source evidence 标记 superseded, 创建 Supports/DerivedFrom edges
//!
//! 设计原则: 保守提升,不做模糊聚类 (NLP 调用另走 P1)。当前版本仅基于
//! 内容哈希精确分桶 + 模式匹配, 保证每一条 Promotion 都可审计、可重现。

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::database::{DbError, MemoryDatabase};
use crate::indexer::{ConsolidationState, ConsolidationTx};
use crate::types::*;

/// 同组内 evidence 数达到此阈值才创建 Memory。
const MIN_OCCURRENCES: usize = 3;
/// 单次 consolidate 最多提升多少组,避免冷启动爆冲。
const MAX_PROMOTIONS_PER_RUN: usize = 20;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ConsolidationReport {
    pub groups_evaluated: usize,
    pub groups_promoted: usize,
    pub groups_insufficient: usize,
    pub memories_created: usize,
    pub evidence_superseded: usize,
    pub edges_created: usize,
    pub errors: usize,
}

impl MemoryDatabase {
    /// 运行一次 consolidation 流程: 从 Evidence 提升稳定 Memory。
    ///
    /// 幂等: 已被 superseded 的 evidence 跳过。可在启动后台或周期性
    /// 任务中多次调用。返回统计结果供诊断日志。
    pub fn run_consolidation_pass(&self) -> Result<ConsolidationReport, DbError> {
        let mut report = ConsolidationReport::default();

        // 先崩溃恢复: PREPARED/DB_APPLIED → FAILED (不删 unit)
        let _ = self.recover_nonterminal_txs()?;

        // 读 evidence 并按哈希前缀分桶。
        let groups = self.list_active_evidence_grouped_by_hash()?;
        report.groups_evaluated = groups.len();

        let mut promoted = 0usize;
        for (_hash_prefix, evidences) in groups {
            if promoted >= MAX_PROMOTIONS_PER_RUN {
                break;
            }
            if evidences.len() < MIN_OCCURRENCES {
                report.groups_insufficient += 1;
                continue;
            }

            // 跳过已有 superceding memory 的组。
            let superseded_by_any = evidences
                .iter()
                .filter_map(|e| {
                    let supers = self.superseding_memories(&e.id).ok()?;
                    if supers.is_empty() { None } else { Some(()) }
                })
                .next()
                .is_some();
            if superseded_by_any {
                continue;
            }

            // 决定 Memory kind: 看证据 section 分布。
            let kind = decide_memory_kind(&evidences);

            // 1) PREPARED tx
            let evidence_ids: Vec<String> = evidences.iter().map(|e| e.id.clone()).collect();
            let manifest = serde_json::json!({
                "evidence_ids": evidence_ids,
                "kind": kind.as_str(),
                "occurrence_count": evidences.len(),
                "rollout_ids": evidences.iter().map(|e| e.rollout_id.clone()).collect::<Vec<_>>(),
            });
            let input_hash = {
                let mut hasher = Sha256::new();
                hasher.update(manifest.to_string().as_bytes());
                format!("{:x}", hasher.finalize())
            };
            let tx_id = format!("consol_{}", Uuid::new_v4());
            let tx = ConsolidationTx::new_prepared(
                tx_id.clone(),
                None,
                None,
                input_hash.clone(),
                manifest.to_string(),
            );
            if let Err(e) = self.create_consolidation_tx(&tx) {
                eprintln!("[warn] consolidation create_tx failed: {e}");
                report.errors += 1;
                continue;
            }

            // 2) 合成 MemoryUnit: 用第一条 evidence 的内容,加上来源汇总。
            let mem_content = compose_memory_content(&evidences, kind);
            let scope = evidences
                .iter()
                .find(|e| matches!(e.scope, MemoryScope::Global))
                .map(|e| e.scope)
                .unwrap_or(MemoryScope::Workspace);
            let mem_id = make_consolidation_mem_id(&input_hash);
            let mem_content_hash = {
                let mut h = Sha256::new();
                h.update(mem_content.as_bytes());
                format!("{:x}", h.finalize())
            };
            let now = Utc::now();
            let mu = MemoryUnit {
                id: mem_id.clone(),
                path: String::from("__consolidated__"),
                section: compose_memory_section(&evidences),
                kind,
                scope,
                status: UnitStatus::Active,
                content: mem_content,
                content_hash: mem_content_hash.clone(),
                updated_at: now,
                created_at: now,
            };
            if let Err(e) = self.upsert_memory_unit(&mu) {
                eprintln!("[warn] consolidation upsert_memory failed: {e}");
                let _ = self.transition_consolidation_tx(&tx_id, ConsolidationState::Failed, None);
                report.errors += 1;
                continue;
            }

            // 3) DB_APPLIED transition
            if let Err(e) = self.transition_consolidation_tx(
                &tx_id, ConsolidationState::DbApplied, Some(&mem_content_hash)
            ) {
                eprintln!("[warn] consolidation DB_APPLIED failed: {e}");
                report.errors += 1;
                continue;
            }

            // 4) Provenance edges: DerivedFrom + Supports for every evidence
            for ev in &evidences {
                let _ = self.add_provenance_edge(&ProvenanceEdge {
                    memory_id: mem_id.clone(),
                    evidence_id: ev.id.clone(),
                    relation: EdgeRelation::DerivedFrom,
                    created_at: Utc::now(),
                });
                let _ = self.add_provenance_edge(&ProvenanceEdge {
                    memory_id: mem_id.clone(),
                    evidence_id: ev.id.clone(),
                    relation: EdgeRelation::Supports,
                    created_at: Utc::now(),
                });
                report.edges_created += 2;
                // 5) 标记 Supersedes edge + supersede evidence
                let _ = self.add_provenance_edge(&ProvenanceEdge {
                    memory_id: mem_id.clone(),
                    evidence_id: ev.id.clone(),
                    relation: EdgeRelation::Supersedes,
                    created_at: Utc::now(),
                });
                report.edges_created += 1;
                if self.supersede_evidence(&ev.id, &mem_id).is_ok() {
                    report.evidence_superseded += 1;
                }
            }

            // 6) COMPLETED
            let _ = self.transition_consolidation_tx(&tx_id, ConsolidationState::Completed, None);
            promoted += 1;
            report.groups_promoted += 1;
            report.memories_created += 1;
        }

        Ok(report)
    }
}

fn compose_memory_content(evs: &[EvidenceUnit], kind: MemoryKind) -> String {
    if evs.is_empty() { return String::new(); }
    let main = &evs[0].content;
    let mut out = String::new();
    let kind_label = match kind {
        MemoryKind::Preference => "[Stable Preference]",
        MemoryKind::Decision => "[Stable Decision]",
        MemoryKind::Constraint => "[Stable Constraint]",
        MemoryKind::Solution => "[Stable Solution]",
        MemoryKind::Fact => "[Stable Fact]",
    };
    out.push_str(kind_label);
    out.push('\n');
    out.push_str(main);
    out.push_str("\n\n");
    out.push_str(&format!(
        "Confirmed across {} historical sessions ({} distinct). Sources:",
        evs.len(),
        evs.iter()
            .map(|e| e.rollout_id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    ));
    out.push('\n');
    for (i, e) in evs.iter().enumerate().take(8) {
        out.push_str(&format!(
            "  [{}] session {} ({})",
            i,
            &e.rollout_id[..std::cmp::min(8, e.rollout_id.len())],
            e.occurred_at.format("%Y-%m-%d"),
        ));
        out.push('\n');
    }
    out
}

fn compose_memory_section(evs: &[EvidenceUnit]) -> String {
    if evs.is_empty() { return String::new(); }
    let sections: std::collections::HashSet<&str> = evs
        .iter()
        .map(|e| e.section.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    if sections.is_empty() {
        return format!("consolidated ({} evidences)", evs.len());
    }
    format!(
        "consolidated from: {}",
        sections.into_iter().take(3).collect::<Vec<_>>().join(", ")
    )
}

fn decide_memory_kind(evs: &[EvidenceUnit]) -> MemoryKind {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for e in evs {
        let sec = e.section.as_str();
        let hint = if sec.contains("用户问题") || sec.contains("偏好") || sec.contains("preference") {
            "preference"
        } else if sec.contains("修复") || sec.contains("fix") || sec.contains("solution") {
            "solution"
        } else if sec.contains("tool_result") {
            // tool 错误结论: 常被提升为解决方案/约束
            let c = &e.content;
            if c.contains("missing") || c.contains("not found") || c.contains("failed to link")
                || c.contains("error") {
                "solution"
            } else if c.contains("must") || c.contains("必须") || c.contains("ensure") {
                "constraint"
            } else {
                "fact"
            }
        } else {
            "fact"
        };
        *counts.entry(hint).or_insert(0) += 1;
    }
    let best = counts.into_iter().max_by_key(|(_, n)| *n).map(|(k, _)| k);
    match best.unwrap_or("fact") {
        "preference" => MemoryKind::Preference,
        "solution" => MemoryKind::Solution,
        "constraint" => MemoryKind::Constraint,
        "decision" => MemoryKind::Decision,
        _ => MemoryKind::Fact,
    }
}

fn make_consolidation_mem_id(input_hash: &str) -> String {
    // mem_c_{input_hash[:10]}
    format!("mem_c_{}", &input_hash[..std::cmp::min(10, input_hash.len())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::apply_schema;
    use rusqlite::Connection;

    fn make_db() -> MemoryDatabase {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        MemoryDatabase::from_conn(conn)
    }

    fn insert_evidence(db: &MemoryDatabase, content: &str, rollout: &str, section: &str) -> EvidenceUnit {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let content_hash = format!("{:x}", hasher.finalize());
        // unique id: rollout + content prefix (same content across different
        // sessions must NOT collide on id)
        let mut id_hasher = Sha256::new();
        id_hasher.update(rollout.as_bytes());
        id_hasher.update(content.as_bytes());
        let id_digest = format!("{:x}", id_hasher.finalize());
        let id = format!(
            "ev_test_{}",
            &id_digest[..8]
        );
        let eu = EvidenceUnit {
            id,
            rollout_id: rollout.into(),
            path: "__test__".into(),
            section: section.into(),
            scope: MemoryScope::Workspace,
            status: EvidenceStatus::Active,
            content: content.into(),
            content_hash,
            occurred_at: Utc::now(),
            created_at: Utc::now(),
            superseded_by: None,
            superseded_at: None,
            rollout_available: true,
            rollout_expired_at: None,
            subchunk_index: 0,
        };
        db.upsert_evidence_unit(&eu).unwrap();
        eu
    }

    #[test]
    fn consolidation_promotes_three_identical_evidences() {
        let db = make_db();
        let e1 = insert_evidence(&db, "[exec] missing openssl libssl.so.3", "r1", "tool_result");
        let e2 = insert_evidence(&db, "[exec] missing openssl libssl.so.3", "r2", "tool_result");
        let _e3 = insert_evidence(&db, "[exec] missing openssl libssl.so.3", "r3", "tool_result");
        assert_ne!(e1.id, e2.id, "different rollouts should yield different evidence ids");

        let rpt = db.run_consolidation_pass().unwrap();
        assert_eq!(rpt.memories_created, 1);
        assert_eq!(rpt.evidence_superseded, 3);
        // Evidence should now be superseded
        let got = db.get_evidence_unit(&e1.id).unwrap().unwrap();
        assert_eq!(got.status, EvidenceStatus::Superseded);
    }

    #[test]
    fn consolidation_skips_two_evidences() {
        let db = make_db();
        insert_evidence(&db, "same content", "r1", "用户问题");
        insert_evidence(&db, "same content", "r2", "用户问题");

        let rpt = db.run_consolidation_pass().unwrap();
        assert_eq!(rpt.memories_created, 0);
        assert!(rpt.groups_insufficient > 0);
    }

    #[test]
    fn consolidation_is_idempotent() {
        let db = make_db();
        insert_evidence(&db, "重复三次", "r1", "用户问题");
        insert_evidence(&db, "重复三次", "r2", "用户问题");
        insert_evidence(&db, "重复三次", "r3", "用户问题");

        let r1 = db.run_consolidation_pass().unwrap();
        let r2 = db.run_consolidation_pass().unwrap();
        assert_eq!(r1.memories_created, 1);
        assert_eq!(r2.memories_created, 0, "idempotent second pass must not re-promote");
    }
}
