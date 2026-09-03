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
use std::sync::Arc;

use crate::database::MemoryDatabase;
use crate::query_understanding::{QueryUnderstanding, QueryUnderstandingModel};
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
    ///
    /// **CJK-aware** (fix for review item 二-3, paired with
    /// `enrich_content_for_fts` on the write side):
    /// - ASCII/Latin: split on whitespace, keep whole word (alnum + `_` + `-`).
    /// - CJK runs (Han/Hiragana/Katakana/Hangul): each individual Han char
    ///   becomes its own term. We use **unigrams only** here (not bigrams)
    ///   because the coverage gate computes `ceil(n/2)` as the match
    ///   threshold — bigrams would inflate n and make the gate stricter
    ///   instead of fairer. Unigrams are the minimum unit of Chinese
    ///   semantic overlap, and the parallel FTS leg already handles
    ///   bigram weighting via BM25 scoring.
    ///
    /// Whitespace-only terms and punctuation are dropped. Deduplicated
    /// preserving first-seen order.
    pub fn extract_terms(query: &str) -> Vec<String> {
        let mut terms: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut ascii_buf = String::new();

        let mut flush_ascii = |buf: &mut String, terms: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
            if buf.is_empty() {
                return;
            }
            let lower = buf.to_lowercase();
            let cleaned: String = lower
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if !cleaned.is_empty() && seen.insert(cleaned.clone()) {
                terms.push(cleaned);
            }
            buf.clear();
        };

        for c in query.chars() {
            if is_cjk(c) {
                flush_ascii(&mut ascii_buf, &mut terms, &mut seen);
                // Per-CJK-char term. Chinese Han chars are already
                // lowercased (no case in CJK).
                let s = c.to_string();
                if seen.insert(s.clone()) {
                    terms.push(s);
                }
            } else if c.is_whitespace() {
                flush_ascii(&mut ascii_buf, &mut terms, &mut seen);
            } else {
                ascii_buf.push(c);
            }
        }
        flush_ascii(&mut ascii_buf, &mut terms, &mut seen);
        terms
    }

    /// Check if a candidate qualifies based on term coverage.
    ///
    /// For CJK, we do char-for-char containment on the **verbatim** content
    /// — this parallels the write-side enrichment: if `enrich_content_for_fts`
    /// wrote unigrams "我" "叫" into the FTS index, then checking verbatim
    /// content.contains("我") is equivalent to an FTS match, but keeps the
    /// coverage gate independent of FTS row enrichment side-effects.
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
/// Unicode61 tokenizer behaviour:
/// - Whitespace-separated words are individually tokenized.
/// - Long unspaced CJK runs (> 5 chars?) become ONE token — a "I call what"
///   query has zero overlap with "The user wants to be called ikkk" because
///   the whole CJK sentence is one token, making recall zero even when
///   human semantics clearly overlap.
///
/// Fix (review item 二-3): we split every CJK run into per-char tokens so
/// BM25 term matching is meaningful. Then we also emit bi-grams (2-char
/// windows) as separate OR-joined terms so longer sentences still have
/// structured locality, while keeping per-char ORs for single-character
/// overlap with any hit. The OR operator makes this lenient: any match on
/// any bigram or unigram contributes to the candidate set.
///
/// Example: "我叫什么" →
///   "我" OR "叫" OR "什" OR "么" OR "我叫" OR "叫什" OR "什么"
///
/// Now content "用户希望被称呼为 ikkk" contains "叫" AND "呼" (unigram hit)
/// even though the whole CJK phrase token won't overlap.
pub fn build_fts_query(user_input: &str) -> String {
    let mut terms: Vec<String> = Vec::new();

    // Split the query into segments: a segment is either a CJK run (every
    // char is CJK) or a whitespace-delimited non-CJK word.
    let segments = split_cjk_segments(user_input);
    for seg in segments {
        match seg {
            Segment::Cjk(s) => {
                // Bigrams first (stronger locality signal, higher score).
                // Then unigrams (fallback for partial overlap).
                let chars: Vec<char> = s.chars().collect();
                if chars.len() >= 2 {
                    for w in chars.windows(2) {
                        let bigram: String = w.iter().collect();
                        terms.push(format!("\"{bigram}\""));
                    }
                }
                for c in &chars {
                    terms.push(format!("\"{c}\""));
                }
            }
            Segment::Ascii(w) => {
                let cleaned: String = w
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                    .collect();
                if !cleaned.is_empty() {
                    terms.push(format!("\"{cleaned}\""));
                }
            }
        }
    }
    terms.join(" OR ")
}

enum Segment<'a> {
    /// Contiguous CJK characters (Han / Hiragana / Katakana / Hangul).
    Cjk(&'a str),
    /// A whitespace-delimited word that does not consist primarily of CJK.
    Ascii(&'a str),
}

fn split_cjk_segments<'a>(input: &'a str) -> Vec<Segment<'a>> {
    let mut out: Vec<Segment<'a>> = Vec::new();

    // Use char-wise boundaries; build byte ranges that map to runs of CJK.
    let mut char_spans: Vec<(usize, usize, char)> = Vec::new();
    for (i, c) in input.char_indices() {
        let end = i + c.len_utf8();
        char_spans.push((i, end, c));
    }

    let mut i = 0;
    while i < char_spans.len() {
        let (byte_start, _, c) = char_spans[i];
        if is_cjk(c) {
            // Find end of the CJK run.
            let mut j = i;
            while j < char_spans.len() && is_cjk(char_spans[j].2) {
                j += 1;
            }
            let byte_end = char_spans[j - 1].1;
            out.push(Segment::Cjk(&input[byte_start..byte_end]));
            i = j;
        } else {
            // Skip whitespace; accumulate ASCII/Latin word.
            if c.is_whitespace() {
                i += 1;
                continue;
            }
            let mut j = i;
            while j < char_spans.len()
                && !char_spans[j].2.is_whitespace()
                && !is_cjk(char_spans[j].2)
            {
                j += 1;
            }
            let byte_end = char_spans[j - 1].1;
            out.push(Segment::Ascii(&input[byte_start..byte_end]));
            i = j;
        }
    }
    out
}

/// CJK classifier (shared with router.rs; duplicated here to keep retrievers
/// a self-contained dep and avoid a cross-public coupling on an internal
/// helper).
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x309F | // Hiragana
        0x30A0..=0x30FF | // Katakana
        0x3400..=0x4DBF | // CJK Ext A
        0x4E00..=0x9FFF | // CJK Unified Ideographs
        0xAC00..=0xD7A3 | // Hangul Syllables
        0xF900..=0xFAFF | // CJK Compatibility Ideographs
        0x20000..=0x2A6DF // CJK Ext B
    )
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
                    section: String::new(),
                    memory_kind: None,
                    updated_at: None,
                    rollout_id: String::new(),
                    superseded_by: None,
                    occurred_at: None,
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
///
/// Optional integration: attach a `QueryUnderstandingModel` via
/// `with_query_understanding` to (1) run an FTS rewrite on the query
/// before building the BM25 query, and (2) down-filter the returned
/// result set to `scope_hint` / `kind_hint` when the intent is
/// confident. Both steps are fail-open (default QU → none), so tests
/// and non-LLM deployments are unaffected.
pub struct MemoryRetriever {
    db: MemoryDatabase,
    config: RetrievalConfig,
    query_understanding: Option<Arc<dyn QueryUnderstandingModel>>,
}

impl MemoryRetriever {
    pub fn new(db: MemoryDatabase, config: RetrievalConfig) -> Self {
        Self { db, config, query_understanding: None }
    }

    /// Attach a QueryUnderstandingModel. When present, the async
    /// `retrieve_enhanced` entrypoint uses it for (a) query rewrite
    /// against FTS and (b) post-retrieval scope/kind filtering to
    /// reduce spurious cross-intent hits.
    pub fn with_query_understanding<M>(mut self, model: M) -> Self
    where
        M: QueryUnderstandingModel + 'static,
    {
        self.query_understanding = Some(Arc::new(model));
        self
    }

    /// Retrieve up to `memory_quota` long-term memory results.
    ///
    /// NOTE: sync, no QU involvement. Kept for tests / callers that
    /// just want raw FTS + term coverage behavior. For the W4 QU
    /// wired path, use `retrieve_enhanced`.
    pub fn retrieve(&self, user_input: &str) -> (Vec<RetrievalResult>, RetrievalDiagnostics) {
        self.retrieve_inner(user_input, None)
    }

    /// Retrieve with optional QueryUnderstanding. When a model is
    /// attached:
    ///   1. Run QU (fail-open to sync retrieve on error).
    ///   2. Prefer `rewritten_query` over the raw input for FTS
    ///      indexing (e.g. "我叫什么" → "用户 称呼 名字").
    ///   3. Pass scope/kind hints so `retrieve_inner` can narrow
    ///      the candidate set.
    pub async fn retrieve_enhanced(
        &self,
        user_input: &str,
    ) -> (Vec<RetrievalResult>, RetrievalDiagnostics) {
        let qu = match self.query_understanding.as_ref() {
            Some(q) => q,
            None => return self.retrieve(user_input),
        };
        let understanding: QueryUnderstanding = match qu.understand(user_input).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "query_understanding failed, falling back to sync retrieve"
                );
                return self.retrieve(user_input);
            }
        };
        let fts_input = understanding
            .rewritten_query
            .as_deref()
            .unwrap_or(user_input);
        let mut scope_filter = understanding.intent.scope_hint();
        let mut kind_filter = understanding.intent.kind_hint();
        // Safety valve: if QU wrote a rewrite that doesn't share any
        // tokens with the original query, we'd drop results that
        // would otherwise pass on the raw input. Fall back to raw
        // when the rewrite yields empty.
        let (mut results, diagnostics) = self.retrieve_inner(fts_input, Some((scope_filter, kind_filter)));
        if results.is_empty() && understanding.rewritten_query.is_some() {
            // Strip QU filters for the fallback so a narrow intent
            // can't suppress the last chance match.
            scope_filter = None;
            kind_filter = None;
            let (raw_results, raw_diag) = self.retrieve_inner(user_input, None);
            results = raw_results;
            return (
                results,
                RetrievalDiagnostics {
                    reason_codes: {
                        let mut rc = raw_diag.reason_codes;
                        rc.push("qu_rewrite_fell_back_to_raw".into());
                        rc
                    },
                    ..raw_diag
                },
            );
        }
        let mut codes = diagnostics.reason_codes;
        codes.push(format!(
            "qu_intent:{}",
            understanding.intent.as_str()
        ));
        if understanding.rewritten_query.is_some() {
            codes.push("qu_rewrite_applied".into());
        }
        if scope_filter.is_some() {
            codes.push("qu_scope_filtered".into());
        }
        if kind_filter.is_some() {
            codes.push("qu_kind_filtered".into());
        }
        (
            results,
            RetrievalDiagnostics {
                reason_codes: codes,
                ..diagnostics
            },
        )
    }

    /// Internal: FTS candidates → term coverage gate → ranking → quota.
    ///
    /// When `(scope_hint, kind_hint)` is provided, results that fail
    /// either filter are dropped after term coverage (we apply hints
    /// after BM25 rather than to SQL so the FTS index shape stays
    /// uniform and the db crate doesn't depend on QU types).
    fn retrieve_inner(
        &self,
        user_input: &str,
        filters: Option<(Option<MemoryScope>, Option<MemoryKind>)>,
    ) -> (Vec<RetrievalResult>, RetrievalDiagnostics) {
        let fts_query = build_fts_query(user_input);
        let candidate_limit = self.config.memory_quota * self.config.candidate_multiplier;
        let index_gen = self.db.index_generation().unwrap_or(0);

        let candidates = match self.db.fts5_memory_candidates(&fts_query, candidate_limit) {
            Ok(c) => c,
            Err(_) => Vec::new(),
        };

        let candidate_count = candidates.len();
        let query_terms = TermCoverageGate::extract_terms(user_input);

        let (scope_hint, kind_hint) = filters.unwrap_or((None, None));

        let mut results: Vec<RetrievalResult> = candidates
            .into_iter()
            .filter_map(|(unit_id, content, path, score)| {
                // Apply QU scope/kind hints from the unit metadata.
                if let (Some(want_scope), Some(unit)) = (scope_hint, self.db.get_memory_unit(&unit_id).ok().flatten()) {
                    if unit.scope != want_scope {
                        return None;
                    }
                    if let Some(want_kind) = kind_hint {
                        if unit.kind != want_kind {
                            return None;
                        }
                    }
                }
                let coverage = TermCoverageGate::check(&content, &query_terms);
                if coverage == 0 && !query_terms.is_empty() {
                    return None;
                }
                Some(RetrievalResult {
                    unit_id,
                    path,
                    content,
                    section: String::new(),
                    memory_kind: None,
                    updated_at: None,
                    rollout_id: String::new(),
                    superseded_by: None,
                    occurred_at: None,
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

        // P1-1: bump access counters for the truncated top-K.
        for r in &results {
            let _ = self.db.record_memory_access(&r.unit_id);
        }

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
                    section: String::new(),
                    memory_kind: None,
                    updated_at: None,
                    rollout_id: String::new(),
                    superseded_by: None,
                    occurred_at: None,
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

        for r in &results {
            let _ = self.db.record_evidence_access(&r.unit_id);
        }

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
    /// Empty retrieval — returned when the Router disables all three
    /// pipelines, or as a fail-open fallback when the blocking task
    /// panics.
    pub fn empty() -> Self {
        Self {
            skills: Vec::new(),
            memory: Vec::new(),
            evidence: Vec::new(),
            total_memory_evidence: 0,
            diagnostics: Vec::new(),
        }
    }
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
                section: unit.section,
                memory_kind: Some(unit.kind),
                updated_at: Some(unit.updated_at),
                rollout_id: String::new(),
                superseded_by: None,
                occurred_at: None,
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
                section: unit.section,
                memory_kind: None,
                updated_at: None,
                rollout_id: unit.rollout_id,
                superseded_by: unit.superseded_by,
                occurred_at: Some(unit.occurred_at),
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
                section: String::new(),
                memory_kind: None,
                updated_at: None,
                rollout_id: String::new(),
                superseded_by: None,
                occurred_at: None,
                bm25_score: 0.0,
                term_coverage: 1,
                total_terms: 1,
                source: ResultSource::Skill,
            }],
            memory: vec![RetrievalResult {
                unit_id: "mem_1".to_string(),
                path: "MEMORY.md".to_string(),
                content: "Important fact".to_string(),
                section: String::new(),
                memory_kind: None,
                updated_at: None,
                rollout_id: String::new(),
                superseded_by: None,
                occurred_at: None,
                bm25_score: 0.0,
                term_coverage: 1,
                total_terms: 1,
                source: ResultSource::Memory,
            }],
            evidence: vec![RetrievalResult {
                unit_id: "ev_1".to_string(),
                path: "summary.md".to_string(),
                content: "Past event".to_string(),
                section: String::new(),
                memory_kind: None,
                updated_at: None,
                rollout_id: String::new(),
                superseded_by: None,
                occurred_at: None,
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

    // ── P0: CJK query expansion (review item 二-3) ──

    #[test]
    fn build_fts_query_cjk_single_sentence_bigrams_and_unigrams() {
        // 我叫什么 → 3 bigrams + 4 unigrams OR-joined.
        let q = build_fts_query("我叫什么");
        assert!(q.contains("\"我叫\""), "bigram missing: {q}");
        assert!(q.contains("\"叫什\""), "bigram missing: {q}");
        assert!(q.contains("\"什么\""), "bigram missing: {q}");
        assert!(q.contains("\"我\""), "unigram missing: {q}");
        assert!(q.contains("\"叫\""), "unigram missing: {q}");
        assert!(q.contains("\"什\""), "unigram missing: {q}");
        assert!(q.contains("\"么\""), "unigram missing: {q}");
    }

    #[test]
    fn build_fts_query_mixed_cjk_ascii() {
        let q = build_fts_query("rust 我叫什么 go");
        assert!(q.contains("\"rust\""));
        assert!(q.contains("\"go\""));
        assert!(q.contains("\"我叫\""));
        assert!(q.contains("\"什么\""));
    }

    #[test]
    fn build_fts_query_single_cjk_char() {
        let q = build_fts_query("好");
        assert_eq!(q, "\"好\"");
    }

    #[test]
    fn build_fts_query_two_cjk_bigrams_exist() {
        // 2 CJK chars → one bigram + two unigrams.
        let q = build_fts_query("名称");
        assert!(q.contains("\"名称\""));
        assert!(q.contains("\"名\""));
        assert!(q.contains("\"称\""));
    }

    #[test]
    fn build_fts_query_empty_or_pure_whitespace() {
        assert_eq!(build_fts_query(""), "");
        assert_eq!(build_fts_query("   \t\n"), "");
    }

    // ── End-to-end: CJK query hits CJK memory via bigram expansion ──

    #[test]
    fn cjk_query_hits_memory_via_unigram_overlap() {
        use crate::retrievers::{MemoryRetriever, RetrievalConfig};
        use crate::types::{MemoryKind, MemoryScope, MemoryUnit, UnitStatus};
        use chrono::Utc;
        use sha2::{Digest, Sha256};

        let db = crate::MemoryDatabase::open_in_memory().unwrap();
        let now = Utc::now();
        // NOTE: content intentionally contains both "叫" and "我" so the
        // identity query "我叫什么" shares unigrams {我, 叫} and the bigram
        // {我叫} with the indexed _CJKTOKENS_ block. Without enrichment
        // unicode61 would tokenize the entire unspaced CJK run as a single
        // token, yielding zero candidate overlap even though semantically
        // this is exactly a "my name is" preference memory.
        let content = "用户说：叫我 ikkk，记住我的名字。".to_string();
        let hash = {
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            format!("{:x}", h.finalize())[..16].to_string()
        };
        db.upsert_memory_unit(&MemoryUnit {
            id: "mem_pref_ikkk".into(),
            path: "proposal".into(),
            section: "".into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Global,
            status: UnitStatus::Active,
            content,
            content_hash: hash,
            updated_at: now,
            created_at: now,
        })
        .unwrap();

        let retriever = MemoryRetriever::new(db.clone(), RetrievalConfig::default());
        let (results, _) = retriever.retrieve("我叫什么");
        // Previously 0 results (unicode61: whole CJK sentence = 1 token,
        // no term overlap with the query even though they share Han chars).
        // With the dual fix:
        //   (a) write side enriches FTS rows with _CJKTOKENS_ bigrams+unigrams
        //   (b) query side splits CJK runs into bigrams+unigrams via OR
        // we are guaranteed unigram overlap on 我/叫 and bigram overlap on
        // 我叫, so BM25 surfaces the candidate.
        let fts_q = build_fts_query("我叫什么");
        let candidates = db.fts5_memory_candidates(&fts_q, 20).unwrap();
        assert!(
            !candidates.is_empty(),
            "CJK query should produce at least one FTS candidate via unigram/bigram overlap.\n\
             Query: {fts_q}\n\
             Candidates: {candidates:?}"
        );
        // End-to-end: retriever (which applies term-coverage post-gate)
        // should also surface the preference memory for this query.
        assert!(
            !results.is_empty(),
            "End-to-end retriever should surface the preference memory for '我叫什么'.\n\
             Results: {results:?}\n\
             FTS query: {fts_q}\n\
             FTS candidates: {candidates:?}"
        );
        let _ = db;
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

    // ─── W4-3: QueryUnderstanding integration tests ────

    /// Retrieval `with_query_understanding(MockQueryUnderstanding)` for
    /// identity query must:
    ///   (a) invoke Mock's rewrite "我叫什么" → "… 用户 称呼 名字 name"
    ///   (b) classify intent=user_identity and apply scope=Global,
    ///       kind=Preference filter.
    ///   (c) tag reason_codes as `qu_intent:user_identity`,
    ///       `qu_rewrite_applied`, `qu_scope_filtered`, `qu_kind_filtered`.
    ///   (d) surface the name-preference memory via the rewritten query.
    #[tokio::test]
    async fn retrieve_enhanced_runs_identity_through_mock_qu_pipeline() {
        use crate::llm_extractor::{
            EvidenceExtractor, ExtractionContext, MockEvidenceExtractor, SourceRef,
        };
        use crate::proposal::propose_and_commit;
        use crate::query_understanding::{
            MockQueryUnderstanding, QueryUnderstandingModel, QueryIntent,
        };
        use crate::types::MemoryScope;

        let db = crate::database::MemoryDatabase::open_in_memory().unwrap();
        let extractor = MockEvidenceExtractor::default();
        let extraction_ctx = ExtractionContext {
            user_input: "记住我叫 ikkk。".into(),
            assistant_content: vec!["好的，以后我会叫你 ikkk。".into()],
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            adjacent_events: Vec::new(),
            existing_memory: Vec::new(),
            source: SourceRef {
                rollout_id: "test_session_1".into(),
                seq_start: 1,
                seq_end: 5,
                turn_id: "turn_remember".into(),
                step_id: None,
            },
        };
        let extraction = extractor.extract(&extraction_ctx).await.unwrap();
        let report = propose_and_commit(&db, &extraction, "mock:rule");
        assert_eq!(report.committed, 1);

        let qu = MockQueryUnderstanding;
        // Sanity — the mock must classify this as identity and produce a
        // rewrite (that's the contract this integration test relies on).
        let qu_check = qu.understand("我叫什么").await.unwrap();
        assert_eq!(qu_check.intent, QueryIntent::UserIdentity);
        assert_eq!(qu_check.intent.scope_hint(), Some(MemoryScope::Global));
        assert!(qu_check.rewritten_query.is_some());

        let retriever = MemoryRetriever::new(db.clone(), RetrievalConfig::default())
            .with_query_understanding(qu);
        let (results, diag) = retriever.retrieve_enhanced("我叫什么").await;

        assert!(
            results.iter().any(|r| r.content.contains("ikkk")),
            "retrieve_enhanced should surface the name memory via QU rewrite. Results: {results:#?}"
        );
        assert!(
            diag.reason_codes
                .iter()
                .any(|c| c == "qu_intent:user_identity"),
            "missing intent tag in reason_codes: {:?}",
            diag.reason_codes
        );
        assert!(
            diag.reason_codes.iter().any(|c| c == "qu_rewrite_applied"),
            "missing rewrite tag in reason_codes: {:?}",
            diag.reason_codes
        );
        assert!(
            diag.reason_codes.iter().any(|c| c == "qu_scope_filtered"),
            "missing scope filter tag in reason_codes: {:?}",
            diag.reason_codes
        );
        assert!(
            diag.reason_codes.iter().any(|c| c == "qu_kind_filtered"),
            "missing kind filter tag in reason_codes: {:?}",
            diag.reason_codes
        );
    }

    /// Fail-open: when QU model returns an error, `retrieve_enhanced`
    /// should degrade cleanly to the raw sync retrieve instead of
    /// bubbling the error. This is enforced via an
    /// "always-error" mock.
    #[tokio::test]
    async fn retrieve_enhanced_falls_back_to_raw_when_qu_errors() {
        use chrono::Utc;
        use std::sync::atomic::{AtomicU64, Ordering};

        use crate::database::MemoryDatabase;
        use crate::llm_extractor::{
            EvidenceExtractor, ExtractionContext, MockEvidenceExtractor, SourceRef,
        };
        use crate::proposal::propose_and_commit;
        use crate::query_understanding::{
            QueryUnderstanding, QueryUnderstandingError, QueryUnderstandingModel,
        };

        struct ErrorQU(AtomicU64);
        #[async_trait::async_trait]
        impl QueryUnderstandingModel for ErrorQU {
            async fn understand(&self, _q: &str) -> Result<QueryUnderstanding, QueryUnderstandingError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(QueryUnderstandingError::Provider("boom".into()))
            }
        }

        // Use the propose_and_commit + Mock extractor flow so the memory
        // unit is written with FTS enrichment (same code path as
        // production), and the raw retrieve FTS leg can find it.
        let db = MemoryDatabase::open_in_memory().unwrap();
        let extractor = MockEvidenceExtractor::default();
        let extraction_ctx = ExtractionContext {
            user_input: "记住我喜欢测试偏好。".into(),
            assistant_content: vec!["好的，已记录偏好。".into()],
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            adjacent_events: Vec::new(),
            existing_memory: Vec::new(),
            source: SourceRef {
                rollout_id: "test_session_qu_err".into(),
                seq_start: 1,
                seq_end: 2,
                turn_id: "t1".into(),
                step_id: None,
            },
        };
        let extraction = extractor.extract(&extraction_ctx).await.unwrap();
        let report = propose_and_commit(&db, &extraction, "mock:rule");
        assert!(
            report.committed >= 1,
            "fallback needs at least one memory unit to retrieve: {report:?}"
        );
        drop(extractor);

        let err_qu = ErrorQU(AtomicU64::new(0));
        let retriever = MemoryRetriever::new(db.clone(), RetrievalConfig::default())
            .with_query_understanding(err_qu);
        let (results, diag) = retriever.retrieve_enhanced("测试偏好").await;

        // Retrieve succeeded via the fail-open path: ErrorQU is an
        // always-error model so `retrieve_enhanced` must have taken the
        // `match qu.understand` Err branch → sync retrieve. The sync
        // retrieve has access to the FTS-enriched DB and returns the
        // "我喜欢测试偏好" preference memory.
        assert!(
            !results.is_empty(),
            "QU error must fall back to raw retrieval. Diag: {diag:?}"
        );
        // No QU_* tags when we took the error shortcut.
        assert!(
            !diag.reason_codes.iter().any(|c| c.starts_with("qu_intent")),
            "QU error must not pretend QU pipeline tags ran. reasons: {:?}",
            diag.reason_codes
        );
        // Silence unused import warning for types still needed for
        // fallthrough safety of the `ErrorQU` definition above.
        let _dt = Utc::now();
    }
}
