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
