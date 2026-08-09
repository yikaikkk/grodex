//! Eval sampling — extract queries from rollout and run retrieval eval.
//!
//! Design 08 §13: The eval harness needs real replay data from rollout
//! journals. This module provides the bridge between rollout events and
//! the Eval harness types in `eval.rs`.
//!
//! Workflow:
//!   1. CLI reads rollout events (UserInputAccepted) → extract queries
//!   2. For each query, run IntentRouter + retrieve_all against a MemoryDatabase
//!   3. Optionally load ground-truth labels from a JSON file
//!   4. Compute EvalMetrics via `eval::compute_metrics`

use std::collections::HashMap;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::eval::{compute_metrics, EvalManifest, EvalMetrics, EvalSample};
use crate::retrievers::{retrieve_all, RetrievalConfig};
use crate::router::IntentRouter;
use crate::database::MemoryDatabase;

/// A user query extracted from a rollout journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedQuery {
    /// The sequence number of the UserInputAccepted event.
    pub seq: u64,
    /// The user's input text.
    pub text: String,
    /// Wall-clock timestamp from the event.
    pub timestamp: chrono::DateTime<Utc>,
    /// Turn ID if present.
    pub turn_id: Option<String>,
}

/// Ground-truth labels for eval, loaded from a JSON file.
///
/// Format: a map from query text (or seq as string) to expected IDs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvalLabels {
    /// Map from query text to expected IDs.
    pub labels: HashMap<String, QueryLabels>,
}

/// Ground-truth labels for a single query.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QueryLabels {
    /// Expected memory unit IDs.
    #[serde(default)]
    pub expected_memory_ids: Vec<String>,
    /// Expected evidence unit IDs.
    #[serde(default)]
    pub expected_evidence_ids: Vec<String>,
    /// Expected skill IDs.
    #[serde(default)]
    pub expected_skill_ids: Vec<String>,
}

impl EvalLabels {
    /// Load labels from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Look up labels for a query by text (exact match).
    pub fn lookup(&self, query: &str) -> Option<&QueryLabels> {
        self.labels.get(query)
    }
}

/// Extract user queries from rollout event payloads.
///
/// Accepts raw event payloads (type + payload JSON) as produced by
/// `FileRolloutStore::replay_from`. Only `UserInputAccepted` events
/// are extracted; other events are ignored.
pub fn extract_queries_from_events(
    events: &[(u64, &str, &serde_json::Value, chrono::DateTime<Utc>, Option<String>)],
) -> Vec<ExtractedQuery> {
    let mut queries = Vec::new();
    for (seq, event_type, payload, timestamp, turn_id) in events {
        if *event_type == "UserInputAccepted" {
            let text = payload
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !text.is_empty() {
                queries.push(ExtractedQuery {
                    seq: *seq,
                    text,
                    timestamp: *timestamp,
                    turn_id: turn_id.clone(),
                });
            }
        }
    }
    queries
}

/// Run eval against a memory database for a set of extracted queries.
///
/// For each query:
///   1. Run IntentRouter to get a RouterDecision
///   2. Run retrieve_all with the router's flags
///   3. Record actual retrieved IDs + diagnostics
///   4. If labels are provided, attach expected IDs
///
/// Returns EvalSamples ready for `compute_metrics`.
pub fn run_eval_against_db(
    db: &MemoryDatabase,
    config: &RetrievalConfig,
    queries: &[ExtractedQuery],
    labels: Option<&EvalLabels>,
) -> Vec<EvalSample> {
    let mut samples = Vec::with_capacity(queries.len());

    for (i, query) in queries.iter().enumerate() {
        let router_decision = IntentRouter::route(&query.text);

        let combined = retrieve_all(
            db,
            config,
            &query.text,
            router_decision.skill_enabled,
            router_decision.memory_enabled,
            router_decision.evidence_enabled,
            router_decision.include_superseded,
        );

        let actual_memory_ids: Vec<String> =
            combined.memory.iter().map(|r| r.unit_id.clone()).collect();
        let actual_evidence_ids: Vec<String> =
            combined.evidence.iter().map(|r| r.unit_id.clone()).collect();
        let actual_skill_ids: Vec<String> =
            combined.skills.iter().map(|r| r.unit_id.clone()).collect();

        let (expected_memory_ids, expected_evidence_ids, expected_skill_ids) =
            if let Some(lbls) = labels {
                if let Some(ql) = lbls.lookup(&query.text) {
                    (
                        ql.expected_memory_ids.clone(),
                        ql.expected_evidence_ids.clone(),
                        ql.expected_skill_ids.clone(),
                    )
                } else {
                    (Vec::new(), Vec::new(), Vec::new())
                }
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };

        samples.push(EvalSample {
            sample_id: format!("eval-{}-seq{}", i, query.seq),
            query: query.text.clone(),
            timestamp: query.timestamp,
            context: format!("seq={}", query.seq),
            expected_memory_ids,
            expected_evidence_ids,
            expected_skill_ids,
            router_decision: router_decision.clone(),
            retrieval_diagnostics: combined.diagnostics.clone(),
            actual_memory_ids,
            actual_evidence_ids,
            actual_skill_ids,
        });
    }

    samples
}

/// Run a complete eval cycle: extract queries, run retrieval, compute metrics.
///
/// Convenience function that chains `extract_queries_from_events` →
/// `run_eval_against_db` → `compute_metrics`.
pub fn run_eval_cycle(
    db: &MemoryDatabase,
    config: &RetrievalConfig,
    events: &[(u64, &str, &serde_json::Value, chrono::DateTime<Utc>, Option<String>)],
    labels: Option<&EvalLabels>,
) -> (Vec<EvalSample>, EvalMetrics) {
    let queries = extract_queries_from_events(events);
    let samples = run_eval_against_db(db, config, &queries, labels);
    let metrics = compute_metrics(&samples);
    (samples, metrics)
}

/// Build an EvalManifest for a completed eval run.
pub fn build_manifest(
    scope: &str,
    from: chrono::DateTime<Utc>,
    to: chrono::DateTime<Utc>,
    sample_ids: Vec<String>,
    config: &RetrievalConfig,
) -> EvalManifest {
    let mut parameter_snapshot = HashMap::new();
    parameter_snapshot.insert("max_results".to_string(), config.max_results.to_string());
    parameter_snapshot.insert(
        "candidate_multiplier".to_string(),
        config.candidate_multiplier.to_string(),
    );
    parameter_snapshot.insert("memory_quota".to_string(), config.memory_quota.to_string());
    parameter_snapshot.insert("evidence_quota".to_string(), config.evidence_quota.to_string());
    parameter_snapshot.insert("skill_quota".to_string(), config.skill_quota.to_string());

    EvalManifest {
        manifest_id: format!("manifest-{}", from.timestamp()),
        scope: scope.to_string(),
        from,
        to,
        criteria: HashMap::from([("source".to_string(), "rollout_replay".to_string())]),
        redaction_rules: vec!["hash_query".to_string()],
        parameter_snapshot,
        sample_ids,
    }
}

/// Format EvalMetrics as a human-readable report for CLI output.
pub fn format_metrics_report(metrics: &EvalMetrics) -> String {
    let pct = |v: f64| format!("{:.1}%", v * 100.0);
    let f = |v: f64| format!("{:.3}", v);

    format!(
        r#"═══ Memory Retrieval Eval Report ═══

Samples: {sample_count}

── Router ──
  Miss rate:              {miss}
  Unnecessary rate:       {unnec}
  Empty retrieval rate:   {empty}

── Memory (Recall@K / Precision@K) ──
  Recall@K:     {mem_recall}
  Precision@K:  {mem_prec}

── Evidence (Recall@K / Precision@K) ──
  Recall@K:     {ev_recall}
  Precision@K:  {ev_prec}

── Skill ──
  Top-1 hit:    {s1}
  Top-2 hit:    {s2}

── Quality ──
  Superseded misinjection:  {sup}
  Avg repeated empties:      {rep}
  Parameter version:         {pv}
═══ End Report ═══"#,
        sample_count = metrics.sample_count,
        miss = pct(metrics.router_miss_rate),
        unnec = pct(metrics.router_unnecessary_rate),
        empty = pct(metrics.router_empty_retrieval_rate),
        mem_recall = f(metrics.memory_recall_at_k),
        mem_prec = f(metrics.memory_precision_at_k),
        ev_recall = f(metrics.evidence_recall_at_k),
        ev_prec = f(metrics.evidence_precision_at_k),
        s1 = pct(metrics.skill_top1_hit_rate),
        s2 = pct(metrics.skill_top2_hit_rate),
        sup = pct(metrics.superseded_misinjection_rate),
        rep = f(metrics.avg_repeated_empty_retrievals),
        pv = metrics.parameter_version,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use chrono::Utc;

    fn make_db_with_data() -> MemoryDatabase {
        let db = MemoryDatabase::open_in_memory().unwrap();

        // Insert a few memory units.
        let now = Utc::now();
        db.upsert_memory_unit(&MemoryUnit {
            id: "mem_1".to_string(),
            path: "MEMORY.md".to_string(),
            section: "## Architecture".to_string(),
            kind: MemoryKind::Decision,
            scope: MemoryScope::Workspace,
            status: UnitStatus::Active,
            content: "The project uses a three-layer loop architecture".to_string(),
            content_hash: "h1".to_string(),
            updated_at: now,
            created_at: now,
        }).unwrap();

        db.upsert_memory_unit(&MemoryUnit {
            id: "mem_2".to_string(),
            path: "MEMORY.md".to_string(),
            section: "## Constraints".to_string(),
            kind: MemoryKind::Constraint,
            scope: MemoryScope::Workspace,
            status: UnitStatus::Active,
            content: "CredentialBroker must never expose master tokens".to_string(),
            content_hash: "h2".to_string(),
            updated_at: now,
            created_at: now,
        }).unwrap();

        db
    }

    #[test]
    fn extract_queries_filters_user_input() {
        let now = Utc::now();
        let e1 = serde_json::json!({"text": "hello"});
        let e2 = serde_json::json!({"text": "hi"});
        let e3 = serde_json::json!({"text": "what is the architecture?"});
        let e4 = serde_json::json!({"text": ""});
        let events: Vec<(u64, &str, &serde_json::Value, chrono::DateTime<Utc>, Option<String>)> = vec![
            (1, "UserInputAccepted", &e1, now, None),
            (2, "ModelItemProduced", &e2, now, None),
            (3, "UserInputAccepted", &e3, now, None),
            (4, "UserInputAccepted", &e4, now, None),
        ];

        let queries = extract_queries_from_events(&events);
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].text, "hello");
        assert_eq!(queries[1].text, "what is the architecture?");
    }

    #[test]
    fn run_eval_produces_samples() {
        let db = make_db_with_data();
        let config = RetrievalConfig::default();
        let now = Utc::now();
        let payload = serde_json::json!({"text": "architecture loop"});

        let events: Vec<(u64, &str, &serde_json::Value, chrono::DateTime<Utc>, Option<String>)> = vec![
            (1, "UserInputAccepted", &payload, now, None),
        ];

        let (samples, metrics) = run_eval_cycle(&db, &config, &events, None);
        assert_eq!(samples.len(), 1);
        assert_eq!(metrics.sample_count, 1);
        assert!(!samples[0].actual_memory_ids.is_empty());
    }

    #[test]
    fn labels_loaded_from_json() {
        let json = r#"{
            "labels": {
                "architecture": {
                    "expected_memory_ids": ["mem_1"],
                    "expected_evidence_ids": [],
                    "expected_skill_ids": []
                }
            }
        }"#;
        let labels = EvalLabels::from_json(json).unwrap();
        assert!(labels.lookup("architecture").is_some());
        assert!(labels.lookup("nonexistent").is_none());
    }

    #[test]
    fn eval_with_labels_computes_recall() {
        let db = make_db_with_data();
        let config = RetrievalConfig::default();
        let now = Utc::now();
        let payload = serde_json::json!({"text": "architecture loop"});

        let events: Vec<(u64, &str, &serde_json::Value, chrono::DateTime<Utc>, Option<String>)> = vec![
            (1, "UserInputAccepted", &payload, now, None),
        ];

        let labels_json = r#"{
            "labels": {
                "architecture loop": {
                    "expected_memory_ids": ["mem_1"],
                    "expected_evidence_ids": [],
                    "expected_skill_ids": []
                }
            }
        }"#;
        let labels = EvalLabels::from_json(labels_json).unwrap();

        let (samples, metrics) = run_eval_cycle(&db, &config, &events, Some(&labels));
        assert_eq!(samples[0].expected_memory_ids, vec!["mem_1"]);
        // mem_1 should be in the actual results since it matches "architecture".
        assert!(metrics.memory_recall_at_k > 0.0);
    }

    #[test]
    fn format_report_is_human_readable() {
        let metrics = EvalMetrics {
            sample_count: 10,
            router_miss_rate: 0.1,
            router_unnecessary_rate: 0.2,
            router_empty_retrieval_rate: 0.05,
            memory_recall_at_k: 0.85,
            memory_precision_at_k: 0.90,
            evidence_recall_at_k: 0.70,
            evidence_precision_at_k: 0.80,
            skill_top1_hit_rate: 0.60,
            skill_top2_hit_rate: 0.85,
            superseded_misinjection_rate: 0.0,
            avg_irrelevant_injection_count: 0.0,
            avg_repeated_empty_retrievals: 0.1,
            parameter_version: "v1_fts_only".to_string(),
        };
        let report = format_metrics_report(&metrics);
        assert!(report.contains("Samples: 10"));
        assert!(report.contains("Memory (Recall@K / Precision@K)"));
    }

    #[test]
    fn build_manifest_captures_config() {
        let config = RetrievalConfig::default();
        let now = Utc::now();
        let manifest = build_manifest("test", now, now, vec!["s1".to_string()], &config);
        assert_eq!(manifest.scope, "test");
        assert!(!manifest.parameter_snapshot.is_empty());
        assert!(manifest.parameter_snapshot.contains_key("max_results"));
    }

    #[test]
    fn empty_events_produce_empty_samples() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let config = RetrievalConfig::default();
        let events: Vec<(u64, &str, &serde_json::Value, chrono::DateTime<Utc>, Option<String>)> = vec![];
        let (samples, metrics) = run_eval_cycle(&db, &config, &events, None);
        assert!(samples.is_empty());
        assert_eq!(metrics.sample_count, 0);
    }
}
