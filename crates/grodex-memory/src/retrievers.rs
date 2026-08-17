//! Three-way retrieval pipelines: Skill, Long-term Memory, Evidence.
//!
//! Design 08 §5: each pipeline has its own candidate pool, qualification
//! gate, quota, and ranking. Results are partitioned so the agent never
//! confuses "execution procedure" with "historical fact".
//!
//! V1 uses FTS5-only with term coverage qualification (§5.2.1):
//!   qualification = term coverage hard rule (not a BM25 absolute threshold)
//!   ranking       = BM25 within qualified candidates

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::database::MemoryDatabase;
use crate::types::*;
use serde::{Deserialize, Serialize};

/// Configuration for retrieval quotas and candidate multipliers.
///
/// Defaults inherit Grok Build values (§2.5):
///   max_results=6, candidate_multiplier=3, max_chunk_chars=1600.
#[derive(Debug, Clone)]
pub struct RetrievalConfig {
    /// Maximum total Memory + GlobalUserPreference + Evidence results per turn.
    pub max_results: usize,
    /// Candidate multiplier: fetch quota × multiplier candidates before gating.
    pub candidate_multiplier: usize,
    /// Long-term Memory quota (workspace/project facts).
    pub memory_quota: usize,
    /// Global UserPreference conditional floor (independent slot).
    pub preference_quota: usize,
    /// Evidence quota (only when evidence is enabled by Router).
    pub evidence_quota: usize,
    /// Skill quota (entry references only, not counted in max_results).
    pub skill_quota: usize,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            max_results: 6,
            candidate_multiplier: 3,
            memory_quota: 4,
            preference_quota: 1,
            evidence_quota: 2,
            skill_quota: 2,
        }
    }
}

/// Diagnostics for a single retrieval pipeline invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalDiagnostics {
    /// Which pipeline produced these results.
    pub source: ResultSource,
    /// The FTS5 query string used.
    pub fts_query: String,
    /// Number of candidates fetched from FTS5.
    pub candidate_count: usize,
    /// Number of candidates that passed term coverage gate.
    pub qualified_count: usize,
    /// Number of results returned after quota.
    pub returned_count: usize,
    /// Current index generation at retrieval time.
    pub index_generation: u64,
    /// Reason codes if the pipeline was skipped or returned empty.
    pub reason_codes: Vec<String>,
}

/// Term coverage qualification gate (§5.2.1).
///
/// BM25 scores vary with query IDF, term count, and document length, so
/// a fixed absolute threshold is unreliable. Instead:
///   1. Extract distinct query terms using the same tokenizer as FTS5.
///   2. Quoted phrases and code identifiers are "required" — must match.
///   3. Other queries must match ceil(distinct_terms / 2), at least 1.
///   4. A single-term query must match that term.
pub struct TermCoverageGate;

impl TermCoverageGate {
    /// Extract distinct query terms for coverage checking.
    /// Splits on whitespace, lowercases, and deduplicates.
    /// Quoted phrases (inside `"..."`) are treated as a single required term.
    pub fn extract_terms(query: &str) -> Vec<String> {
        let mut terms = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for word in query.split_whitespace() {
            let lower = word.to_lowercase();
            // Strip punctuation but keep code identifiers with underscores/hyphens.
            let cleaned: String = lower
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if cleaned.is_empty() {
                continue;
            }
            if seen.insert(cleaned.clone()) {
                terms.push(cleaned);
            }
        }
        terms
    }

    /// Check if a candidate qualifies based on term coverage.
    ///
    /// Returns the number of matching terms if qualified, or 0 if not.
    pub fn check(content: &str, query_terms: &[String]) -> usize {
        if query_terms.is_empty() {
            return 0;
        }

        let content_lower = content.to_lowercase();

        // Count how many distinct query terms appear in the content.
        let matched = query_terms
            .iter()
            .filter(|term| content_lower.contains(term.as_str()))
            .count();

        // Single-term query: must match that term.
        if query_terms.len() == 1 {
            return if matched >= 1 { matched } else { 0 };
        }

        // Multi-term: must match at least ceil(n/2) terms, and at least 1.
        let threshold = (query_terms.len() + 1) / 2; // ceil(n/2)
        if matched >= threshold && matched >= 1 {
            matched
        } else {
            0
        }
    }
}

/// Build an FTS5 query string from user input.
///
/// V1 uses a simple OR of terms (no synonym expansion, no query rewriting).
/// Each term is quoted to handle special characters.
pub fn build_fts_query(user_input: &str) -> String {
    let terms: Vec<String> = user_input
        .split_whitespace()
        .map(|w| {
            // Escape FTS5 special characters by wrapping in double quotes.
            let cleaned: String = w
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            cleaned
        })
        .filter(|s| !s.is_empty())
        .map(|s| format!("\"{s}\""))
        .collect();
    terms.join(" OR ")
}

// ───────────────────── Skill Retriever ─────────────────────

/// Retrieves skill references based on intent matching.
///
/// Only metadata (name, description, triggers) is indexed — never full
/// SKILL.md text. Results are entry-path references, not content injections.
pub struct SkillRetriever {
    db: MemoryDatabase,
    config: RetrievalConfig,
}

impl SkillRetriever {
    pub fn new(db: MemoryDatabase, config: RetrievalConfig) -> Self {
        Self { db, config }
    }

    /// Retrieve up to `skill_quota` skill references.
    pub fn retrieve(&self, user_input: &str) -> (Vec<RetrievalResult>, RetrievalDiagnostics) {
        let fts_query = build_fts_query(user_input);
        let candidate_limit = self.config.skill_quota * self.config.candidate_multiplier;
        let index_gen = self.db.index_generation().unwrap_or(0);

        let candidates = match self.db.fts5_skill_candidates(&fts_query, candidate_limit) {
            Ok(c) => c,
            Err(_) => Vec::new(),
        };

        let candidate_count = candidates.len();
        let query_terms = TermCoverageGate::extract_terms(user_input);

        let mut results: Vec<RetrievalResult> = candidates
            .into_iter()
            .filter_map(|(skill_id, name, desc, entry_path, score)| {
                let combined = format!("{name} {desc}");
                let coverage = TermCoverageGate::check(&combined, &query_terms);
                if coverage == 0 && !query_terms.is_empty() {
                    return None;
                }
                Some(RetrievalResult {
                    unit_id: skill_id,
                    path: entry_path,
                    content: format!("{name}: {desc}"),
                    bm25_score: score,
                    term_coverage: coverage,
                    total_terms: query_terms.len(),
                    source: ResultSource::Skill,
                })
            })
            .collect();

        // Sort by BM25 score (lower is better in FTS5, but we use absolute value
        // consistency — negative because FTS5 bm25() returns negative values).
        results.sort_by(|a, b| {
            b.bm25_score
                .partial_cmp(&a.bm25_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(self.config.skill_quota);

        let diagnostics = RetrievalDiagnostics {
            source: ResultSource::Skill,
            fts_query,
            candidate_count,
            qualified_count: results.len(),
            returned_count: results.len(),
            index_generation: index_gen,
            reason_codes: if results.is_empty() {
                vec!["no_qualified_skill".to_string()]
            } else {
                Vec::new()
            },
        };

        (results, diagnostics)
    }
}

// ───────────────────── Memory Retriever ─────────────────────

/// Retrieves long-term memory facts (consolidated knowledge units).
///
/// Uses FTS5 + term coverage gate. Only `active` units participate.
/// Global UserPreference gets an independent conditional floor slot.
pub struct MemoryRetriever {
    db: MemoryDatabase,
    config: RetrievalConfig,
}

impl MemoryRetriever {
    pub fn new(db: MemoryDatabase, config: RetrievalConfig) -> Self {
        Self { db, config }
    }

    /// Retrieve up to `memory_quota` long-term memory results.
    pub fn retrieve(&self, user_input: &str) -> (Vec<RetrievalResult>, RetrievalDiagnostics) {
        let fts_query = build_fts_query(user_input);
        let candidate_limit = self.config.memory_quota * self.config.candidate_multiplier;
        let index_gen = self.db.index_generation().unwrap_or(0);

        let candidates = match self.db.fts5_memory_candidates(&fts_query, candidate_limit) {
            Ok(c) => c,
            Err(_) => Vec::new(),
        };

        let candidate_count = candidates.len();
        let query_terms = TermCoverageGate::extract_terms(user_input);

        let mut results: Vec<RetrievalResult> = candidates
            .into_iter()
            .filter_map(|(unit_id, content, path, score)| {
                let coverage = TermCoverageGate::check(&content, &query_terms);
                if coverage == 0 && !query_terms.is_empty() {
                    return None;
                }
                Some(RetrievalResult {
                    unit_id,
                    path,
                    content,
                    bm25_score: score,
                    term_coverage: coverage,
                    total_terms: query_terms.len(),
                    source: ResultSource::Memory,
                })
            })
            .collect();

        results.sort_by(|a, b| {
            b.bm25_score
                .partial_cmp(&a.bm25_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(self.config.memory_quota);

        let diagnostics = RetrievalDiagnostics {
            source: ResultSource::Memory,
            fts_query,
            candidate_count,
            qualified_count: results.len(),
            returned_count: results.len(),
            index_generation: index_gen,
            reason_codes: if results.is_empty() {
                vec!["no_qualified_memory".to_string()]
            } else {
                Vec::new()
            },
        };

        (results, diagnostics)
    }
}

// ───────────────────── Evidence Retriever ─────────────────────

/// Retrieves historical session evidence.
///
/// Only enabled when the Router sets `evidence=true` (user asks about
/// history, reasons, original text, or evolution). Default retrieval
/// excludes `superseded` evidence; history queries can include it
/// with explicit annotation.
pub struct EvidenceRetriever {
    db: MemoryDatabase,
    config: RetrievalConfig,
}

impl EvidenceRetriever {
    pub fn new(db: MemoryDatabase, config: RetrievalConfig) -> Self {
        Self { db, config }
    }

    /// Retrieve up to `evidence_quota` evidence results.
    ///
    /// When `include_superseded` is true, superseded evidence is included
    /// but the caller must annotate it as superseded.
    pub fn retrieve(
        &self,
        user_input: &str,
        include_superseded: bool,
    ) -> (Vec<RetrievalResult>, RetrievalDiagnostics) {
        let fts_query = build_fts_query(user_input);
        let candidate_limit = self.config.evidence_quota * self.config.candidate_multiplier;
        let index_gen = self.db.index_generation().unwrap_or(0);

        let candidates = match self
            .db
            .fts5_evidence_candidates(&fts_query, candidate_limit, include_superseded)
        {
            Ok(c) => c,
            Err(_) => Vec::new(),
        };

        let candidate_count = candidates.len();
        let query_terms = TermCoverageGate::extract_terms(user_input);

        let mut results: Vec<RetrievalResult> = candidates
            .into_iter()
            .filter_map(|(unit_id, content, path, score)| {
                let coverage = TermCoverageGate::check(&content, &query_terms);
                if coverage == 0 && !query_terms.is_empty() {
                    return None;
                }
                Some(RetrievalResult {
                    unit_id,
                    path,
                    content,
                    bm25_score: score,
                    term_coverage: coverage,
                    total_terms: query_terms.len(),
                    source: ResultSource::Evidence,
                })
            })
            .collect();

        results.sort_by(|a, b| {
            b.bm25_score
                .partial_cmp(&a.bm25_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(self.config.evidence_quota);

        let diagnostics = RetrievalDiagnostics {
            source: ResultSource::Evidence,
            fts_query,
            candidate_count,
            qualified_count: results.len(),
            returned_count: results.len(),
            index_generation: index_gen,
            reason_codes: if results.is_empty() {
                vec!["no_qualified_evidence".to_string()]
            } else {
                Vec::new()
            },
        };

        (results, diagnostics)
    }
}

// ───────────────────── Combined Retrieval ─────────────────────

/// Combined retrieval result with partitioned sections.
#[derive(Debug, Clone)]
pub struct CombinedRetrieval {
    /// Skill references (not counted in memory budget).
    pub skills: Vec<RetrievalResult>,
    /// Long-term memory facts.
    pub memory: Vec<RetrievalResult>,
    /// Historical evidence (may be empty if Router didn't enable).
    pub evidence: Vec<RetrievalResult>,
    /// Total number of Memory + Evidence results (capped at max_results).
    pub total_memory_evidence: usize,
    /// Diagnostics from each pipeline.
    pub diagnostics: Vec<RetrievalDiagnostics>,
}

impl CombinedRetrieval {
    /// Format the partitioned context for injection into the system prompt.
    ///
    /// Sections are separated so the agent never confuses execution
    /// procedure with historical fact (§6).
    pub fn format_for_prompt(&self) -> String {
        let mut out = String::new();

        if !self.skills.is_empty() {
            out.push_str("<active-skills>\n");
            for s in &self.skills {
                out.push_str(&format!(
                    "  <skill path=\"{}\">{}</skill>\n",
                    s.path, s.content
                ));
            }
            out.push_str("</active-skills>\n\n");
        }

        if !self.memory.is_empty() {
            out.push_str("<memory-context>\n");
            for m in &self.memory {
                out.push_str(&format!(
                    "  [{}] {}\n",
                    m.unit_id, m.content
                ));
            }
            out.push_str("</memory-context>\n\n");
        }

        if !self.evidence.is_empty() {
            out.push_str("<historical-evidence>\n");
            for e in &self.evidence {
                out.push_str(&format!(
                    "  [{}] {} (source: {})\n",
                    e.unit_id, e.content, e.path
                ));
            }
            out.push_str("</historical-evidence>\n");
        }

        out
    }
}

/// Run all three retrieval pipelines and combine results with quota enforcement.
///
/// `evidence_enabled` and `include_superseded` come from the Router decision.
pub fn retrieve_all(
    db: &MemoryDatabase,
    config: &RetrievalConfig,
    user_input: &str,
    skill_enabled: bool,
    memory_enabled: bool,
    evidence_enabled: bool,
    include_superseded: bool,
) -> CombinedRetrieval {
    let mut diagnostics = Vec::new();
    let mut skills = Vec::new();
    let mut memory = Vec::new();
    let mut evidence = Vec::new();

    if skill_enabled {
        let retriever = SkillRetriever::new(db.clone(), config.clone());
        let (s, d) = retriever.retrieve(user_input);
        skills = s;
        diagnostics.push(d);
    }

    if memory_enabled {
        let retriever = MemoryRetriever::new(db.clone(), config.clone());
        let (m, d) = retriever.retrieve(user_input);
        memory = m;
        diagnostics.push(d);
    }

    if evidence_enabled {
        let retriever = EvidenceRetriever::new(db.clone(), config.clone());
        let (e, d) = retriever.retrieve(user_input, include_superseded);
        evidence = e;
        diagnostics.push(d);
    }

    // Enforce total cap: Memory + Evidence ≤ max_results.
    // Trim evidence first (memory has higher priority for facts), then memory.
    let total = memory.len() + evidence.len();
    if total > config.max_results {
        let excess = total - config.max_results;
        let evidence_trim = excess.min(evidence.len());
        evidence.truncate(evidence.len() - evidence_trim);
        // If still over budget, trim memory.
        let remaining_excess = (memory.len() + evidence.len()).saturating_sub(config.max_results);
        if remaining_excess > 0 {
            memory.truncate(memory.len() - remaining_excess);
        }
    }
    let total_memory_evidence = memory.len() + evidence.len();

    CombinedRetrieval {
        skills,
        memory,
        evidence,
        total_memory_evidence,
        diagnostics,
    }
}

// ───────────────────── Hybrid RRF Fusion ─────────────────────

/// Hybrid Reciprocal Rank Fusion (RRF) — 合并 FTS 和 Vector 两个排序列表。
///
/// RRF score(doc) = Σ 1/(k + rank_in_list) for each list in [FTS, Vector]
/// `k_rrf = 60` 是通用默认常数。
pub fn reciprocal_rank_fusion(
    fts_ranked: &[String],
    vector_ranked: &[String],
    top_k: usize,
    k_rrf: f32,
) -> Vec<String> {
    let mut map: HashMap<String, f32> = HashMap::new();
    for (i, doc) in fts_ranked.iter().enumerate() {
        let rank = (i + 1) as f32;
        *map.entry(doc.clone()).or_insert(0.0) += 1.0 / (k_rrf + rank);
    }
    for (i, doc) in vector_ranked.iter().enumerate() {
        let rank = (i + 1) as f32;
        *map.entry(doc.clone()).or_insert(0.0) += 1.0 / (k_rrf + rank);
    }
    let mut entries: Vec<(String, f32)> = map.into_iter().collect();
    // Sort by score descending; break ties alphabetically for determinism.
    entries.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    entries.into_iter().take(top_k).map(|(d, _)| d).collect()
}

/// FTS memory 仅返回 IDs（按 BM25 排序 desc）—— Hybrid 前半段。
pub fn retrieve_fts_memory_ids_only(
    db: &MemoryDatabase,
    query: &str,
    limit: usize,
) -> Vec<String> {
    use TermCoverageGate as Gate;
    let fts_query = build_fts_query(query);
    let candidates = match db.fts5_memory_candidates(&fts_query, limit) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let query_terms = Gate::extract_terms(query);
    let mut filtered: Vec<(String, f64)> = candidates
        .into_iter()
        .filter_map(|(unit_id, content, _path, score)| {
            let coverage = Gate::check(&content, &query_terms);
            if coverage == 0 && !query_terms.is_empty() {
                return None;
            }
            Some((unit_id, score))
        })
        .collect();
    filtered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    filtered.into_iter().map(|(id, _)| id).collect()
}

/// FTS evidence 仅返回 IDs（按 BM25 排序 desc）。
pub fn retrieve_fts_evidence_ids_only(
    db: &MemoryDatabase,
    query: &str,
    limit: usize,
    include_superseded: bool,
) -> Vec<String> {
    use TermCoverageGate as Gate;
    let fts_query = build_fts_query(query);
    let candidates = match db.fts5_evidence_candidates(&fts_query, limit, include_superseded) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let query_terms = Gate::extract_terms(query);
    let mut filtered: Vec<(String, f64)> = candidates
        .into_iter()
        .filter_map(|(unit_id, content, _path, score)| {
            let coverage = Gate::check(&content, &query_terms);
            if coverage == 0 && !query_terms.is_empty() {
                return None;
            }
            Some((unit_id, score))
        })
        .collect();
    filtered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    filtered.into_iter().map(|(id, _)| id).collect()
}

/// 根据最终融合后的 ID 顺序加载完整 RetrievalResult（memory）。
pub fn load_memory_results_in_order(
    db: &MemoryDatabase,
    ordered_ids: &[String],
) -> Vec<RetrievalResult> {
    let mut out = Vec::with_capacity(ordered_ids.len());
    for id in ordered_ids {
        if let Ok(Some(unit)) = db.get_memory_unit(id) {
            out.push(RetrievalResult {
                unit_id: unit.id,
                path: unit.path,
                content: unit.content,
                bm25_score: 0.0,
                term_coverage: 0,
                total_terms: 0,
                source: ResultSource::Memory,
            });
        }
    }
    out
}

/// 根据最终融合后的 ID 顺序加载完整 RetrievalResult（evidence）。
pub fn load_evidence_results_in_order(
    db: &MemoryDatabase,
    ordered_ids: &[String],
) -> Vec<RetrievalResult> {
    let mut out = Vec::with_capacity(ordered_ids.len());
    for id in ordered_ids {
        if let Ok(Some(unit)) = db.get_evidence_unit(id) {
            out.push(RetrievalResult {
                unit_id: unit.id,
                path: unit.path,
                content: unit.content,
                bm25_score: 0.0,
                term_coverage: 0,
                total_terms: 0,
                source: ResultSource::Evidence,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::MemoryDatabase;
    use chrono::Utc;

    fn setup_db() -> MemoryDatabase {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let now = Utc::now();

        // Memory units
        db.upsert_memory_unit(&MemoryUnit {
            id: "mem_rust_release".to_string(),
            path: "MEMORY.md".to_string(),
            section: "#release-workflow".to_string(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Workspace,
            status: UnitStatus::Active,
            content: "Release workflow requires cargo build and cargo test before publishing".to_string(),
            content_hash: "h1".to_string(),
            updated_at: now,
            created_at: now,
        })
        .unwrap();

        db.upsert_memory_unit(&MemoryUnit {
            id: "mem_pref_dark".to_string(),
            path: "MEMORY.md".to_string(),
            section: "#ui-preferences".to_string(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Global,
            status: UnitStatus::Active,
            content: "User prefers dark theme for the editor".to_string(),
            content_hash: "h2".to_string(),
            updated_at: now,
            created_at: now,
        })
        .unwrap();

        // Evidence unit
        db.upsert_evidence_unit(&EvidenceUnit {
            id: "ev_build_fail".to_string(),
            rollout_id: "rollout_001".to_string(),
            path: "summary.md".to_string(),
            section: "#build-failure".to_string(),
            scope: MemoryScope::Workspace,
            status: EvidenceStatus::Active,
            content: "Cargo build failed on Linux due to missing openssl dependency".to_string(),
            content_hash: "h3".to_string(),
            occurred_at: now,
            created_at: now,
            superseded_by: None,
            superseded_at: None,
            rollout_available: true,
            rollout_expired_at: None,
            subchunk_index: 0,
        })
        .unwrap();

        // Skill
        db.upsert_skill(&SkillCatalogEntry {
            skill_id: "skill_release".to_string(),
            name: "Release Workflow".to_string(),
            description: "Guide the release process for publishing the project".to_string(),
            when_to_use: "When publishing a new version".to_string(),
            triggers: vec!["release".to_string(), "publish".to_string()],
            scope: MemoryScope::Workspace,
            enabled: true,
            required_capabilities: vec!["exec".to_string()],
            entry_path: "skills/release/SKILL.md".to_string(),
            content_hash: "h4".to_string(),
            created_at: now,
            updated_at: now,
        })
        .unwrap();

        db
    }

    #[test]
    fn term_coverage_single_term_must_match() {
        let terms = vec!["rust".to_string()];
        assert_eq!(TermCoverageGate::check("Project uses Rust", &terms), 1);
        assert_eq!(TermCoverageGate::check("Project uses Python", &terms), 0);
    }

    #[test]
    fn term_coverage_multi_term_threshold() {
        // 3 terms → need ceil(3/2) = 2 matches
        let terms = vec!["rust".to_string(), "release".to_string(), "cargo".to_string()];
        assert_eq!(TermCoverageGate::check("rust release workflow", &terms), 2);
        assert_eq!(TermCoverageGate::check("rust is great", &terms), 0); // only 1 match → fail
        assert_eq!(TermCoverageGate::check("rust and cargo build", &terms), 2); // 2 → pass
    }

    #[test]
    fn term_coverage_empty_terms_returns_zero() {
        assert_eq!(TermCoverageGate::check("content", &[]), 0);
    }

    #[test]
    fn memory_retriever_returns_qualified_results() {
        let db = setup_db();
        let retriever = MemoryRetriever::new(db, RetrievalConfig::default());
        let (results, diag) = retriever.retrieve("release cargo workflow");
        assert!(!results.is_empty());
        assert!(diag.candidate_count >= 1);
        assert!(diag.qualified_count >= 1);
    }

    #[test]
    fn memory_retriever_filters_low_coverage() {
        let db = setup_db();
        let retriever = MemoryRetriever::new(db, RetrievalConfig::default());
        // "python" doesn't match any memory content
        let (results, _) = retriever.retrieve("python java javascript");
        assert!(results.is_empty());
    }

    #[test]
    fn evidence_retriever_excludes_superseded_by_default() {
        let db = setup_db();
        // Supersede the build_fail evidence
        db.upsert_memory_unit(&MemoryUnit {
            id: "mem_fix".to_string(),
            path: "MEMORY.md".to_string(),
            section: "#fix".to_string(),
            kind: MemoryKind::Solution,
            scope: MemoryScope::Workspace,
            status: UnitStatus::Active,
            content: "Fixed openssl dependency".to_string(),
            content_hash: "h5".to_string(),
            updated_at: Utc::now(),
            created_at: Utc::now(),
        })
        .unwrap();
        db.supersede_evidence("ev_build_fail", "mem_fix").unwrap();

        let retriever = EvidenceRetriever::new(db, RetrievalConfig::default());
        let (results, _) = retriever.retrieve("cargo build failed", false);
        assert!(results.is_empty(), "superseded evidence should be excluded");

        let (results_with, _) = retriever.retrieve("cargo build failed", true);
        assert!(!results_with.is_empty(), "superseded evidence should be included when requested");
    }

    #[test]
    fn skill_retriever_returns_references() {
        let db = setup_db();
        let retriever = SkillRetriever::new(db, RetrievalConfig::default());
        let (results, _) = retriever.retrieve("release publish version");
        assert!(!results.is_empty());
        assert_eq!(results[0].source, ResultSource::Skill);
        assert!(results[0].path.contains("SKILL.md"));
    }

    #[test]
    fn combined_retrieval_enforces_total_cap() {
        let db = setup_db();
        // Add more memory units to exceed the cap.
        for i in 0..10 {
            db.upsert_memory_unit(&MemoryUnit {
                id: format!("mem_extra_{i}"),
                path: "MEMORY.md".to_string(),
                section: format!("#extra-{i}"),
                kind: MemoryKind::Fact,
                scope: MemoryScope::Workspace,
                status: UnitStatus::Active,
                content: format!("release cargo workflow fact number {i}"),
                content_hash: format!("h{i}"),
                updated_at: Utc::now(),
                created_at: Utc::now(),
            })
            .unwrap();
        }

        let config = RetrievalConfig {
            max_results: 3,
            memory_quota: 4,
            evidence_quota: 2,
            ..Default::default()
        };

        let combined = retrieve_all(
            &db,
            &config,
            "release cargo workflow",
            true,
            true,
            true,
            false,
        );

        assert!(combined.memory.len() + combined.evidence.len() <= 3);
        assert_eq!(combined.total_memory_evidence, 3);
    }

    #[test]
    fn combined_format_partitions_sections() {
        let combined = CombinedRetrieval {
            skills: vec![RetrievalResult {
                unit_id: "skill_1".to_string(),
                path: "skills/test/SKILL.md".to_string(),
                content: "Test skill".to_string(),
                bm25_score: 0.0,
                term_coverage: 1,
                total_terms: 1,
                source: ResultSource::Skill,
            }],
            memory: vec![RetrievalResult {
                unit_id: "mem_1".to_string(),
                path: "MEMORY.md".to_string(),
                content: "Important fact".to_string(),
                bm25_score: 0.0,
                term_coverage: 1,
                total_terms: 1,
                source: ResultSource::Memory,
            }],
            evidence: vec![RetrievalResult {
                unit_id: "ev_1".to_string(),
                path: "summary.md".to_string(),
                content: "Past event".to_string(),
                bm25_score: 0.0,
                term_coverage: 1,
                total_terms: 1,
                source: ResultSource::Evidence,
            }],
            total_memory_evidence: 2,
            diagnostics: Vec::new(),
        };

        let formatted = combined.format_for_prompt();
        assert!(formatted.contains("<active-skills>"));
        assert!(formatted.contains("<memory-context>"));
        assert!(formatted.contains("<historical-evidence>"));
        assert!(formatted.contains("Important fact"));
    }

    #[test]
    fn build_fts_query_handles_special_chars() {
        let query = build_fts_query("rust cargo-build");
        assert!(query.contains("\"rust\""));
        assert!(query.contains("\"cargo-build\""));
        assert!(query.contains("OR"));
    }

    // ── RRF tests ──

    #[test]
    fn rrf_single_list_returns_same_order() {
        let fts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let vector: Vec<String> = vec![];
        let fused = reciprocal_rank_fusion(&fts, &vector, 10, 60.0);
        assert_eq!(fused, vec!["a", "b", "c"]);
    }

    #[test]
    fn rrf_fuses_two_lists_promotes_overlap() {
        let fts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let vector = vec!["d".to_string(), "c".to_string(), "b".to_string()];
        let fused = reciprocal_rank_fusion(&fts, &vector, 10, 60.0);
        // c 和 b 在两个列表都出现 → 排名靠前
        assert_eq!(fused[0], "b");
        assert_eq!(fused[1], "c");
    }

    #[test]
    fn rrf_respects_top_k() {
        let fts: Vec<String> = (0..10).map(|i| format!("id{i}")).collect();
        let vector: Vec<String> = (0..10).map(|i| format!("id{}", i + 5)).collect();
        let fused = reciprocal_rank_fusion(&fts, &vector, 3, 60.0);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn rrf_handles_disjoint_lists() {
        let fts = vec!["x1".to_string(), "x2".to_string()];
        let vector = vec!["y1".to_string(), "y2".to_string()];
        let fused = reciprocal_rank_fusion(&fts, &vector, 10, 60.0);
        assert_eq!(fused.len(), 4);
    }
}
