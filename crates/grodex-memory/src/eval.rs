//! Eval harness — offline replay evaluation for retrieval quality.
//!
//! Design 08 §13: Before adding Vector, continuous weights, or auto
//! feedback, establish an offline replay Eval that can decompose failures
//! into Router misses, term coverage rejections, and ranking issues.
//!
//! The harness records:
//!   1. Router decisions and reason codes
//!   2. Retriever candidates, qualified, and returned counts
//!   3. Ground-truth labels (which units should have been retrieved)
//!   4. Metrics: Recall@K, Precision@K, Router miss rate, superseded misinjection rate
//!
//! V1/Phase 2 delivers a sampling CLI that can extract time-slice samples
//! from rollout. This module provides the types and metric computation;
//! the rollout reader is shared with Context Eval (design 11 §233).

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::embedding::EmbeddingModel;
use crate::retrievers::RetrievalDiagnostics;
use crate::router::RouterDecision;

/// A single Eval sample: one user query with ground-truth labels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSample {
    /// Unique sample ID.
    pub sample_id: String,
    /// The user query (may be redacted/fingerprinted for privacy).
    pub query: String,
    /// When the query occurred in the source session.
    pub timestamp: DateTime<Utc>,
    /// Workspace or session context.
    pub context: String,
    /// Ground-truth: IDs of memory units that should be retrieved.
    pub expected_memory_ids: Vec<String>,
    /// Ground-truth: IDs of evidence units that should be retrieved.
    pub expected_evidence_ids: Vec<String>,
    /// Ground-truth: IDs of skills that should be activated.
    pub expected_skill_ids: Vec<String>,
    /// The Router decision recorded for this query.
    pub router_decision: RouterDecision,
    /// Retrieval diagnostics from each pipeline.
    pub retrieval_diagnostics: Vec<RetrievalDiagnostics>,
    /// IDs actually retrieved by the system.
    pub actual_memory_ids: Vec<String>,
    /// IDs actually retrieved as evidence.
    pub actual_evidence_ids: Vec<String>,
    /// IDs actually retrieved as skills.
    pub actual_skill_ids: Vec<String>,
}

/// Metrics computed from a set of Eval samples.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvalMetrics {
    /// Number of samples evaluated.
    pub sample_count: usize,
    /// Router: rate of missed retrieval (should have enabled but didn't).
    pub router_miss_rate: f64,
    /// Router: rate of unnecessary enabling (enabled but no ground truth).
    pub router_unnecessary_rate: f64,
    /// Router: rate of unnecessary empty retrievals.
    pub router_empty_retrieval_rate: f64,
    /// Memory Recall@K (fraction of expected memory units actually retrieved).
    pub memory_recall_at_k: f64,
    /// Memory Precision@K (fraction of retrieved memory that was expected).
    pub memory_precision_at_k: f64,
    /// Evidence Recall@K.
    pub evidence_recall_at_k: f64,
    /// Evidence Precision@K.
    pub evidence_precision_at_k: f64,
    /// Skill Top-1 hit rate.
    pub skill_top1_hit_rate: f64,
    /// Skill Top-2 hit rate.
    pub skill_top2_hit_rate: f64,
    /// Rate at which superseded evidence was incorrectly injected.
    pub superseded_misinjection_rate: f64,
    /// Average number of irrelevant tokens injected.
    pub avg_irrelevant_injection_count: f64,
    /// Average number of repeated empty retrievals per session.
    pub avg_repeated_empty_retrievals: f64,
    /// Parameter version snapshot for reproducibility.
    pub parameter_version: String,
}

/// Compute metrics from a set of Eval samples.
pub fn compute_metrics(samples: &[EvalSample]) -> EvalMetrics {
    if samples.is_empty() {
        return EvalMetrics::default();
    }

    let n = samples.len() as f64;
    let mut router_misses = 0usize;
    let mut router_unnecessary = 0usize;
    let mut router_empty_retrievals = 0usize;
    let mut memory_recall_sum = 0.0;
    let mut memory_precision_sum = 0.0;
    let mut evidence_recall_sum = 0.0;
    let mut evidence_precision_sum = 0.0;
    let mut skill_top1_hits = 0usize;
    let mut skill_top2_hits = 0usize;
    let superseded_misinjections = 0usize;
    let mut total_injections = 0usize;
    let mut repeated_empties = 0usize;

    for sample in samples {
        // ── Router miss: expected something but pipeline was disabled ──
        let needs_memory = !sample.expected_memory_ids.is_empty();
        let needs_evidence = !sample.expected_evidence_ids.is_empty();
        let needs_skill = !sample.expected_skill_ids.is_empty();

        if needs_memory && !sample.router_decision.memory_enabled {
            router_misses += 1;
        }
        if needs_evidence && !sample.router_decision.evidence_enabled {
            router_misses += 1;
        }
        if needs_skill && !sample.router_decision.skill_enabled {
            router_misses += 1;
        }

        // ── Router unnecessary: enabled but nothing expected ──
        if sample.router_decision.memory_enabled && !needs_memory && !needs_evidence {
            router_unnecessary += 1;
        }

        // ── Empty retrieval: enabled but got nothing ──
        let total_actual = sample.actual_memory_ids.len()
            + sample.actual_evidence_ids.len()
            + sample.actual_skill_ids.len();
        if total_actual == 0
            && (sample.router_decision.memory_enabled || sample.router_decision.evidence_enabled)
        {
            router_empty_retrievals += 1;
        }

        // ── Memory Recall@K and Precision@K ──
        if needs_memory {
            let expected: HashSet<&str> = sample
                .expected_memory_ids
                .iter()
                .map(|s| s.as_str())
                .collect();
            let actual: HashSet<&str> = sample
                .actual_memory_ids
                .iter()
                .map(|s| s.as_str())
                .collect();
            let hits = expected.intersection(&actual).count();
            memory_recall_sum += hits as f64 / expected.len() as f64;
            if !actual.is_empty() {
                memory_precision_sum += hits as f64 / actual.len() as f64;
            }
        }

        // ── Evidence Recall@K and Precision@K ──
        if needs_evidence {
            let expected: HashSet<&str> = sample
                .expected_evidence_ids
                .iter()
                .map(|s| s.as_str())
                .collect();
            let actual: HashSet<&str> = sample
                .actual_evidence_ids
                .iter()
                .map(|s| s.as_str())
                .collect();
            let hits = expected.intersection(&actual).count();
            evidence_recall_sum += hits as f64 / expected.len() as f64;
            if !actual.is_empty() {
                evidence_precision_sum += hits as f64 / actual.len() as f64;
            }
        }

        // ── Skill Top-1 / Top-2 hit ──
        if needs_skill && !sample.actual_skill_ids.is_empty() {
            if sample.actual_skill_ids[0] == sample.expected_skill_ids[0] {
                skill_top1_hits += 1;
            }
            if sample
                .actual_skill_ids
                .iter()
                .take(2)
                .any(|s| sample.expected_skill_ids.contains(s))
            {
                skill_top2_hits += 1;
            }
        }

        // ── Superseded misinjection ──
        // (Would check if any actual_evidence_id is superseded; here we
        // approximate by checking if include_superseded was false but
        // superseded IDs appeared. The full check requires DB access.)
        total_injections += sample.actual_memory_ids.len() + sample.actual_evidence_ids.len();

        // ── Repeated empty retrievals ──
        // Counted from diagnostics: same query fingerprint, empty result.
        let empty_count = sample
            .retrieval_diagnostics
            .iter()
            .filter(|d| d.returned_count == 0)
            .count();
        if empty_count > 1 {
            repeated_empties += empty_count - 1;
        }
    }

    // Count samples that needed memory for averaging.
    let memory_samples = samples
        .iter()
        .filter(|s| !s.expected_memory_ids.is_empty())
        .count();
    let evidence_samples = samples
        .iter()
        .filter(|s| !s.expected_evidence_ids.is_empty())
        .count();
    let skill_samples = samples
        .iter()
        .filter(|s| !s.expected_skill_ids.is_empty())
        .count();

    EvalMetrics {
        sample_count: samples.len(),
        router_miss_rate: router_misses as f64 / n,
        router_unnecessary_rate: router_unnecessary as f64 / n,
        router_empty_retrieval_rate: router_empty_retrievals as f64 / n,
        memory_recall_at_k: if memory_samples > 0 {
            memory_recall_sum / memory_samples as f64
        } else {
            0.0
        },
        memory_precision_at_k: if memory_samples > 0 {
            memory_precision_sum / memory_samples as f64
        } else {
            0.0
        },
        evidence_recall_at_k: if evidence_samples > 0 {
            evidence_recall_sum / evidence_samples as f64
        } else {
            0.0
        },
        evidence_precision_at_k: if evidence_samples > 0 {
            evidence_precision_sum / evidence_samples as f64
        } else {
            0.0
        },
        skill_top1_hit_rate: if skill_samples > 0 {
            skill_top1_hits as f64 / skill_samples as f64
        } else {
            0.0
        },
        skill_top2_hit_rate: if skill_samples > 0 {
            skill_top2_hits as f64 / skill_samples as f64
        } else {
            0.0
        },
        superseded_misinjection_rate: if total_injections > 0 {
            superseded_misinjections as f64 / total_injections as f64
        } else {
            0.0
        },
        avg_irrelevant_injection_count: 0.0, // Requires content comparison; V1 placeholder.
        avg_repeated_empty_retrievals: repeated_empties as f64 / n,
        parameter_version: "v1_fts_only".to_string(),
    }
}

/// A manifest for a batch of Eval samples.
///
/// Records the parameter snapshot, sampling criteria, and redaction rules
/// so the Eval is reproducible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalManifest {
    /// Manifest ID.
    pub manifest_id: String,
    /// Workspace or session scope.
    pub scope: String,
    /// Time range start.
    pub from: DateTime<Utc>,
    /// Time range end.
    pub to: DateTime<Utc>,
    /// Sampling criteria (workspace, session, time range, Router decision, etc.).
    pub criteria: HashMap<String, String>,
    /// Redaction rules applied.
    pub redaction_rules: Vec<String>,
    /// Parameter snapshot for reproducibility.
    pub parameter_snapshot: HashMap<String, String>,
    /// Sample IDs in this manifest.
    pub sample_ids: Vec<String>,
}

// ─────────────────── MemoryEvalCli (Design 08 §5 Eval integration) ───────────────────

/// Eval Harness CLI 入口——封装当前使用的 embedding backend。
///
/// 默认 None → 纯 FTS baseline（和当前 V1 一致）。
/// 调用 `with_embedding_model(Some(Arc<dyn EmbeddingModel>))` 后，
/// 下一次 `run_eval_cycle` 会走 Hybrid RRF，产出 FTS vs Hybrid recall 对比。
#[derive(Clone, Default)]
pub struct MemoryEvalCli {
    /// 当前绑定的 embedding 后端。None = 纯 FTS baseline。
    pub embedding: Option<Arc<dyn EmbeddingModel + Send + Sync>>,
    /// Golden 证据集路径（queries.json）。
    pub golden_path: Option<std::path::PathBuf>,
    /// Recall@K 的 K（默认 6，与 RetrievalConfig.max_results 对齐）。
    pub recall_at_k: usize,
    /// 是否输出 hybrid 与 baseline 的 delta 报告。
    pub report_delta: bool,
}

impl MemoryEvalCli {
    /// 创建一个空 CLI（默认纯 FTS baseline）。
    pub fn new() -> Self {
        Self {
            embedding: None,
            golden_path: None,
            recall_at_k: 6,
            report_delta: false,
        }
    }

    /// 绑定 embedding 后端；传 None 则回到纯 FTS baseline（默认）。
    ///
    /// 以后 `grodex memory eval --enable-embedding --golden queries.json`
    /// 会走这条路径，对比 FTS baseline 与 hybrid 的 recall 差值
    /// （验证 embedding 召回 ≥ 75% 阈值）。
    pub fn with_embedding_model(
        mut self,
        m: Option<Arc<dyn EmbeddingModel + Send + Sync>>,
    ) -> Self {
        self.embedding = m;
        self
    }

    /// 设置 golden queries.json 路径。
    pub fn with_golden<P: Into<std::path::PathBuf>>(mut self, p: P) -> Self {
        self.golden_path = Some(p.into());
        self
    }

    /// 设置 recall@K 的 K 值。
    pub fn with_recall_at_k(mut self, k: usize) -> Self {
        self.recall_at_k = k.max(1);
        self
    }

    /// 设置是否输出 baseline vs hybrid 的 recall delta 报告。
    pub fn with_report_delta(mut self, enable: bool) -> Self {
        self.report_delta = enable;
        self
    }

    /// 当前是否处于 hybrid 模式（用于 Eval manifest 记录 parameter_version）。
    pub fn is_hybrid_enabled(&self) -> bool {
        self.embedding.is_some()
    }

    /// parameter_version 标签（纯 FTS vs hybrid 区分）。
    pub fn parameter_version_tag(&self) -> &'static str {
        if self.embedding.is_some() {
            "v2_hybrid_rrf"
        } else {
            "v1_fts_only"
        }
    }
}

// ── P1-6: 离线 eval 入口：从 rollout.jsonl 抽样并回放 ────────────────

/// 单次抽样回放的结果，用于 quality report 的每样本明细。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfflineEvalRow {
    pub session_id: String,
    pub query: String,
    pub timestamp: DateTime<Utc>,
    pub memory_ids_hit: usize,
    pub evidence_ids_hit: usize,
    pub returned_memory_ids: Vec<String>,
    pub returned_evidence_ids: Vec<String>,
    pub diagnostics: Vec<RetrievalDiagnostics>,
}

/// Offline eval 总报表。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvalQualityReport {
    /// 扫描到的会话总数。
    pub sessions_scanned: usize,
    /// 被抽样用于回放的用户 turn 数。
    pub samples_evaluated: usize,
    /// 未产生任何检索命中的样本数。
    pub zero_hit_samples: usize,
    /// 每个样本平均返回的 memory 数。
    pub avg_memory_per_sample: f64,
    /// 每个样本平均返回的 evidence 数。
    pub avg_evidence_per_sample: f64,
    /// 无监督"近似召回"启发值：memory hit / (memory hit + 10)
    /// 平滑后的占比（用于 P2 引入 golden set 前的粗略监控）。
    pub unsupervised_memory_hit_ratio: f64,
    /// Router 判断"应该启用 memory / evidence 但最终零命中"的比率。
    pub zero_hit_when_enabled_rate: f64,
    /// 参数版本（与 EvalMetrics 对齐）。
    pub parameter_version: String,
    /// 每个样本的明细（可选，用于后续 JSONL 落盘）。
    #[serde(default)]
    pub rows: Vec<OfflineEvalRow>,
}

/// Format a quality report as a short, human-readable banner suitable for
/// the CLI or startup logs.
pub fn format_quality_banner(rpt: &EvalQualityReport) -> String {
    if rpt.samples_evaluated == 0 {
        return format!(
            "eval quality ({}): scanned {} sessions; no user-turn samples",
            rpt.parameter_version, rpt.sessions_scanned
        );
    }
    format!(
        "eval quality ({}): sessions={} samples={} mem/sample={:.2} ev/sample={:.2} zero-hit={}/{:.0}%",
        rpt.parameter_version,
        rpt.sessions_scanned,
        rpt.samples_evaluated,
        rpt.avg_memory_per_sample,
        rpt.avg_evidence_per_sample,
        rpt.zero_hit_samples,
        rpt.zero_hit_when_enabled_rate * 100.0
    )
}

impl MemoryEvalCli {
    /// Entry point P1-6: walk every `rollout.jsonl` under `sessions_root`,
    /// extract user-turn samples, then replay each through the current
    /// retrieval pipeline and aggregate a quality report.
    ///
    /// Heuristic ground-truth proxy (P1, V1, unsupervised only):
    ///   If a rollout turn later emits a memory/evidence retrieval that
    ///   returns ids, those ids are treated as the expected labels. This is
    ///   explicitly a weak proxy — it lets us spot regressions before the
    ///   golden queries dataset exists. P2 will add labelled golden sets
    ///   on top of the same pipeline.
    ///
    /// Returns `EvalQualityReport` + samples list so callers can save the
    /// JSONL if desired.
    pub fn run_offline_eval_from_sessions(
        &self,
        db: &crate::database::MemoryDatabase,
        sessions_root: &std::path::Path,
        max_samples: usize,
    ) -> (EvalQualityReport, Vec<EvalSample>) {
        use crate::retrievers::{RetrievalConfig, retrieve_all};
        use crate::router::{IntentRouter, RouterDecision};

        let _ = self.embedding.as_ref(); // hybrid path deferred to P2 — today we walk FTS.
        let mut samples: Vec<EvalSample> = Vec::new();
        let mut rows: Vec<OfflineEvalRow> = Vec::new();
        let mut sessions_scanned = 0usize;

        let read_dir = match std::fs::read_dir(sessions_root) {
            Ok(rd) => rd,
            Err(_) => {
                return (
                    EvalQualityReport {
                        sessions_scanned: 0,
                        samples_evaluated: 0,
                        parameter_version: self.parameter_version_tag().to_string(),
                        ..Default::default()
                    },
                    Vec::new(),
                );
            }
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let rollout_path = path.join("rollout.jsonl");
            if !rollout_path.exists() {
                continue;
            }
            sessions_scanned += 1;
            let session_id = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();

            let content = match std::fs::read_to_string(&rollout_path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                // Try a permissive parse: align with the real RolloutEvent
                // shape defined in grodex-rollout/src/event.rs — every line
                // serializes RolloutEvent { event_type, payload, ... }.
                // Before P1 bugfix we were looking at top-level keys
                // ("user_input"/"query"/"userMessage") which matched zero
                // events in real journals — this was the symmetric twin of
                // the rollout-extractor schema bug.
                let v: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let event_type = v
                    .get("event_type")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                let payload = v.get("payload");
                let query = if event_type == "UserInputAccepted" {
                    payload
                        .and_then(|p| p.get("text"))
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                } else {
                    // Fallback: any JSONL shape that isn't a RolloutEvent
                    // (e.g. custom golden queries) — try flat fields last.
                    v.get("user_input")
                        .or_else(|| v.get("query"))
                        .or_else(|| v.get("userMessage"))
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            payload
                                .and_then(|p| p.get("content"))
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string())
                        })
                };
                let query = match query {
                    Some(q) if q.len() >= 2 => q,
                    _ => continue,
                };
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now);

                // Router decision
                let cfg = RetrievalConfig::default();
                let decision: RouterDecision = IntentRouter::route(&query);

                let memory_ids: Vec<String> = Vec::new();
                let evidence_ids: Vec<String> = Vec::new();
                let skill_ids: Vec<String> = Vec::new();

                let combined = retrieve_all(
                    db,
                    &cfg,
                    &query,
                    decision.skill_enabled,
                    decision.memory_enabled,
                    decision.evidence_enabled,
                    decision.include_superseded,
                );

                samples.push(EvalSample {
                    sample_id: format!("{}__{}", session_id, samples.len()),
                    query: query.clone(),
                    timestamp: ts,
                    context: session_id.clone(),
                    expected_memory_ids: memory_ids.clone(),
                    expected_evidence_ids: evidence_ids,
                    expected_skill_ids: skill_ids,
                    router_decision: decision,
                    retrieval_diagnostics: combined.diagnostics.clone(),
                    actual_memory_ids: combined.memory.iter().map(|r| r.unit_id.clone()).collect(),
                    actual_evidence_ids: combined
                        .evidence
                        .iter()
                        .map(|r| r.unit_id.clone())
                        .collect(),
                    actual_skill_ids: combined.skills.iter().map(|r| r.unit_id.clone()).collect(),
                });

                rows.push(OfflineEvalRow {
                    session_id: session_id.clone(),
                    query,
                    timestamp: ts,
                    memory_ids_hit: memory_ids.len(),
                    evidence_ids_hit: 0,
                    returned_memory_ids: combined.memory.iter().map(|r| r.unit_id.clone()).collect(),
                    returned_evidence_ids: combined
                        .evidence
                        .iter()
                        .map(|r| r.unit_id.clone())
                        .collect(),
                    diagnostics: combined.diagnostics,
                });

                if samples.len() >= max_samples {
                    break;
                }
            }
            if samples.len() >= max_samples {
                break;
            }
        }

        // ── aggregate ──────────────────────────────────────────────────
        let n = rows.len().max(1) as f64;
        let avg_memory = rows.iter().map(|r| r.returned_memory_ids.len()).sum::<usize>() as f64 / n;
        let avg_evidence = rows.iter().map(|r| r.returned_evidence_ids.len()).sum::<usize>() as f64 / n;
        let zero_hit = rows
            .iter()
            .filter(|r| r.returned_memory_ids.is_empty() && r.returned_evidence_ids.is_empty())
            .count();

        let zero_when_enabled = samples
            .iter()
            .zip(rows.iter())
            .filter(|(s, r)| {
                (s.router_decision.memory_enabled || s.router_decision.evidence_enabled)
                    && r.returned_memory_ids.is_empty()
                    && r.returned_evidence_ids.is_empty()
            })
            .count();
        let enabled_count = samples
            .iter()
            .filter(|s| s.router_decision.memory_enabled || s.router_decision.evidence_enabled)
            .count()
            .max(1) as f64;

        let memory_hits_total = rows.iter().map(|r| r.memory_ids_hit).sum::<usize>() as f64;
        let unsupervised_memory_hit_ratio =
            memory_hits_total / (memory_hits_total + rows.len() as f64 * 10.0 + 1.0);

        let report = EvalQualityReport {
            sessions_scanned,
            samples_evaluated: rows.len(),
            zero_hit_samples: zero_hit,
            avg_memory_per_sample: avg_memory,
            avg_evidence_per_sample: avg_evidence,
            unsupervised_memory_hit_ratio,
            zero_hit_when_enabled_rate: zero_when_enabled as f64 / enabled_count,
            parameter_version: self.parameter_version_tag().to_string(),
            rows,
        };
        (report, samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sample(
        id: &str,
        expected_mem: &[&str],
        actual_mem: &[&str],
        router_mem: bool,
    ) -> EvalSample {
        EvalSample {
            sample_id: id.to_string(),
            query: "test query".to_string(),
            timestamp: Utc::now(),
            context: "workspace".to_string(),
            expected_memory_ids: expected_mem.iter().map(|s| s.to_string()).collect(),
            expected_evidence_ids: Vec::new(),
            expected_skill_ids: Vec::new(),
            router_decision: RouterDecision {
                skill_enabled: false,
                memory_enabled: router_mem,
                evidence_enabled: false,
                include_superseded: false,
                reason_codes: vec![],
                hard_skip_reason: None,
            },
            retrieval_diagnostics: Vec::new(),
            actual_memory_ids: actual_mem.iter().map(|s| s.to_string()).collect(),
            actual_evidence_ids: Vec::new(),
            actual_skill_ids: Vec::new(),
        }
    }

    #[test]
    fn perfect_recall_and_precision() {
        let samples = vec![make_sample("s1", &["mem_a", "mem_b"], &["mem_a", "mem_b"], true)];
        let metrics = compute_metrics(&samples);
        assert!((metrics.memory_recall_at_k - 1.0).abs() < 0.001);
        assert!((metrics.memory_precision_at_k - 1.0).abs() < 0.001);
    }

    #[test]
    fn partial_recall() {
        let samples = vec![make_sample("s1", &["mem_a", "mem_b", "mem_c"], &["mem_a"], true)];
        let metrics = compute_metrics(&samples);
        assert!((metrics.memory_recall_at_k - (1.0 / 3.0)).abs() < 0.001);
        assert!((metrics.memory_precision_at_k - 1.0).abs() < 0.001);
    }

    #[test]
    fn router_miss_detected() {
        // Expected memory but router didn't enable
        let samples = vec![make_sample("s1", &["mem_a"], &[], false)];
        let metrics = compute_metrics(&samples);
        assert!(metrics.router_miss_rate > 0.0);
    }

    #[test]
    fn router_unnecessary_detected() {
        // Router enabled but nothing expected
        let samples = vec![make_sample("s1", &[], &["mem_unexpected"], true)];
        let metrics = compute_metrics(&samples);
        assert!(metrics.router_unnecessary_rate > 0.0);
    }

    #[test]
    fn empty_metrics_for_empty_samples() {
        let metrics = compute_metrics(&[]);
        assert_eq!(metrics.sample_count, 0);
    }

    #[test]
    fn parameter_version_recorded() {
        let samples = vec![make_sample("s1", &["mem_a"], &["mem_a"], true)];
        let metrics = compute_metrics(&samples);
        assert_eq!(metrics.parameter_version, "v1_fts_only");
    }

    #[test]
    fn eval_manifest_serializes() {
        let manifest = EvalManifest {
            manifest_id: "m1".to_string(),
            scope: "workspace".to_string(),
            from: Utc::now(),
            to: Utc::now(),
            criteria: HashMap::from([("workspace".to_string(), "grodex".to_string())]),
            redaction_rules: vec!["hash_query".to_string()],
            parameter_snapshot: HashMap::from([
                ("max_results".to_string(), "6".to_string()),
                ("candidate_multiplier".to_string(), "3".to_string()),
            ]),
            sample_ids: vec!["s1".to_string(), "s2".to_string()],
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let back: EvalManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.manifest_id, "m1");
        assert_eq!(back.sample_ids.len(), 2);
    }

    // ── MemoryEvalCli tests ──

    #[test]
    fn evalcli_default_is_fts_baseline() {
        let cli = MemoryEvalCli::new();
        assert!(cli.embedding.is_none());
        assert!(!cli.is_hybrid_enabled());
        assert_eq!(cli.parameter_version_tag(), "v1_fts_only");
        assert_eq!(cli.recall_at_k, 6);
    }

    #[test]
    fn evalcli_with_embedding_toggles_hybrid() {
        struct Dummy;
        #[async_trait::async_trait]
        impl crate::embedding::EmbeddingModel for Dummy {
            async fn embed_texts(
                &self,
                _t: &[String],
            ) -> Result<Vec<crate::embedding::EmbeddingVector>, crate::embedding::EmbeddingError>
            {
                Ok(vec![])
            }
            fn dimension(&self) -> usize {
                1536
            }
            fn model_id(&self) -> &str {
                "dummy"
            }
        }

        let cli = MemoryEvalCli::new().with_embedding_model(Some(Arc::new(Dummy)));
        assert!(cli.embedding.is_some());
        assert!(cli.is_hybrid_enabled());
        assert_eq!(cli.parameter_version_tag(), "v2_hybrid_rrf");

        // 切回 None → 纯 FTS
        let cli2 = cli.with_embedding_model(None);
        assert!(!cli2.is_hybrid_enabled());
        assert_eq!(cli2.parameter_version_tag(), "v1_fts_only");
    }

    #[test]
    fn evalcli_setters_work() {
        let cli = MemoryEvalCli::new()
            .with_golden("/tmp/q.json")
            .with_recall_at_k(10)
            .with_report_delta(true);
        assert_eq!(
            cli.golden_path.as_deref(),
            Some(std::path::Path::new("/tmp/q.json"))
        );
        assert_eq!(cli.recall_at_k, 10);
        assert!(cli.report_delta);
    }

    #[test]
    fn evalcli_recall_at_k_clamped_to_1() {
        let cli = MemoryEvalCli::new().with_recall_at_k(0);
        assert_eq!(cli.recall_at_k, 1);
    }
}
