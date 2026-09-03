//! Memory Proposal + Commit flow — turns LLM-extracted claims into
//! validated, persisted memory units.
//!
//! Pipeline (memory-architecture-redesign.md §Phase 5):
//! 1. `ExtractionResult` (from the LLM extractor) → per-claim validation
//! 2. Valid claims → `MemoryProposal` rows (status=pending)
//! 3. `commit_proposal` → `memory_units` (status=candidate) + FTS + edges
//!
//! Hard-constraint validation (fail-closed):
//! - `should_persist == false` → skipped (logged, not persisted)
//! - content length ≤ 2000 chars
//! - content must NOT match secret patterns (API keys, private keys, passwords)
//! - scope=Global requires certainty=Explicit (no inferred global prefs)

use crate::database::MemoryDatabase;
use crate::llm_extractor::{ExtractedClaim, ExtractionResult, SourceRef};
use crate::types::{Certainty, MemoryKind, MemoryProposal, MemoryScope, ProposalStatus, UnitStatus};
use chrono::Utc;
use sha2::{Digest, Sha256};

/// Max allowed content length for a single memory unit.
const MAX_CONTENT_LEN: usize = 2000;

/// A claim that failed hard-constraint validation and was rejected.
#[derive(Debug, Clone)]
pub struct RejectedClaim {
    pub fact: String,
    pub reason: String,
}

/// Summary of a propose-and-commit batch.
#[derive(Debug, Clone, Default)]
pub struct ProposalCommitReport {
    pub proposed: usize,
    pub committed: usize,
    pub rejected: Vec<RejectedClaim>,
    pub committed_ids: Vec<String>,
}

/// Validate a single claim against hard constraints.
/// Returns `Ok(())` if the claim passes, `Err(reason)` if rejected.
pub fn validate_claim(claim: &ExtractedClaim) -> Result<(), String> {
    // 1. should_persist gate
    if !claim.should_persist {
        return Err("should_persist is false".into());
    }
    // 2. length
    if claim.fact.chars().count() > MAX_CONTENT_LEN {
        return Err(format!("content exceeds {MAX_CONTENT_LEN} chars"));
    }
    if claim.fact.trim().is_empty() {
        return Err("content is empty".into());
    }
    // 3. secret patterns (fail-closed)
    if contains_secret_pattern(&claim.fact) {
        return Err("content matches secret pattern (API key / private key / password)".into());
    }
    // 4. scope=Global requires certainty=Explicit
    if claim.scope == MemoryScope::Global && claim.certainty != Certainty::Explicit {
        return Err("scope=Global requires certainty=explicit".into());
    }
    Ok(())
}

/// Check if the content matches common secret patterns.
/// Conservative: false positives are acceptable (reject + log), false
/// negatives are not (leaking secrets into memory).
fn contains_secret_pattern(content: &str) -> bool {
    let lower = content.to_lowercase();
    // ── API key patterns ──────────────────────────────────────────
    // OpenAI: sk-... (at least 20 chars)
    if lower.contains("sk-") && lower.len() > 20 {
        return true;
    }
    // Generic "api_key=..." / "apikey=..."
    if lower.contains("api_key=") || lower.contains("apikey=") || lower.contains("api-key=") {
        return true;
    }
    // ── Password patterns ─────────────────────────────────────────
    if lower.contains("password=") || lower.contains("passwd=") || lower.contains("pwd=") {
        return true;
    }
    // ── Private key patterns ──────────────────────────────────────
    if lower.contains("private_key") || lower.contains("private key") {
        // Check for BEGIN ... PRIVATE KEY marker or key-like content
        if lower.contains("begin") || lower.contains("-----") {
            return true;
        }
    }
    // AWS access key: AKIA... (20 chars)
    if content.contains("AKIA") && content.len() >= 20 {
        return true;
    }
    // Bearer token
    if lower.contains("bearer ") && lower.contains(".") {
        return true;
    }
    // ── Long hex/base64 blobs that look like secrets ─────────────
    // "token=..." / "secret=..." with a value
    if (lower.contains("token=") || lower.contains("secret="))
        && lower.split('=').nth(1).map(|v| v.trim().len() >= 16).unwrap_or(false)
    {
        return true;
    }
    false
}

/// Create a deterministic proposal from a validated claim.
/// The proposal_id is derived from content + source, so the same claim
/// from the same source produces the same ID (idempotent insert).
pub fn create_proposal(
    claim: &ExtractedClaim,
    source: &SourceRef,
    extractor_model: &str,
) -> MemoryProposal {
    let now = Utc::now();
    let id = proposal_id(&claim.fact, source);
    MemoryProposal {
        proposal_id: id,
        content: claim.fact.clone(),
        kind: claim.kind,
        scope: claim.scope,
        confidence: claim.confidence,
        certainty: claim.certainty,
        source_evidence_ids: Vec::new(), // linked at commit time
        source_rollout_id: source.rollout_id.clone(),
        source_seq_start: source.seq_start,
        source_seq_end: source.seq_end,
        source_turn_id: source.turn_id.clone(),
        extractor_model: extractor_model.to_string(),
        status: ProposalStatus::Pending,
        rejection_reason: String::new(),
        created_at: now,
        resolved_at: None,
    }
}

/// Deterministic proposal ID: sha256(content + rollout_id + seq) truncated.
fn proposal_id(content: &str, source: &SourceRef) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hasher.update(source.rollout_id.as_bytes());
    hasher.update(source.seq_start.to_be_bytes());
    let digest = hasher.finalize();
    let mut s = String::with_capacity(16);
    for b in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    format!("prop_{s}")
}

/// Full propose-and-commit flow for an extraction result.
/// Validates each claim, writes proposals, and commits valid ones.
/// Returns a report of what was proposed / committed / rejected.
pub fn propose_and_commit(
    db: &MemoryDatabase,
    result: &ExtractionResult,
    extractor_model: &str,
) -> ProposalCommitReport {
    let mut report = ProposalCommitReport::default();

    for claim in &result.claims {
        // 1. Validate
        if let Err(reason) = validate_claim(claim) {
            report.rejected.push(RejectedClaim {
                fact: claim.fact.clone(),
                reason,
            });
            continue;
        }
        // 2. Create proposal
        let proposal = create_proposal(claim, &result.source, extractor_model);
        // 3. Insert proposal (idempotent on proposal_id)
        if let Err(e) = db.insert_proposal(&proposal) {
            report.rejected.push(RejectedClaim {
                fact: claim.fact.clone(),
                reason: format!("insert_proposal failed: {e}"),
            });
            continue;
        }
        report.proposed += 1;

        // 4. Commit → memory_units (status=candidate).
        // Idempotent: if the memory unit already exists (from a prior
        // commit of the same proposal), skip to avoid duplicate work.
        let memory_id = format!("mem_{}", &proposal.proposal_id[5..]); // strip "prop_" prefix
        if db.get_memory_unit(&memory_id).ok().flatten().is_some() {
            // Already committed in a previous run — skip.
            continue;
        }
        match db.commit_proposal(&proposal.proposal_id, &memory_id) {
            Ok(_) => {
                report.committed += 1;
                report.committed_ids.push(memory_id);
            }
            Err(e) => {
                report.rejected.push(RejectedClaim {
                    fact: claim.fact.clone(),
                    reason: format!("commit_proposal failed: {e}"),
                });
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_extractor::ExtractionContext;
    use crate::llm_extractor::EvidenceExtractor;

    fn claim(fact: &str, kind: MemoryKind, scope: MemoryScope, certainty: Certainty) -> ExtractedClaim {
        ExtractedClaim {
            fact: fact.into(),
            kind,
            scope,
            certainty,
            confidence: 0.9,
            should_persist: true,
        }
    }

    fn src() -> SourceRef {
        SourceRef {
            rollout_id: "test_rollout".into(),
            seq_start: 1,
            seq_end: 5,
            turn_id: "turn_1".into(),
            step_id: None,
        }
    }

    #[test]
    fn validate_accepts_explicit_global_preference() {
        let c = claim("用户希望被称呼为 ikkk。", MemoryKind::Preference, MemoryScope::Global, Certainty::Explicit);
        assert!(validate_claim(&c).is_ok());
    }

    #[test]
    fn validate_rejects_inferred_global() {
        let c = claim("可能是全局偏好", MemoryKind::Fact, MemoryScope::Global, Certainty::Inferred);
        assert!(validate_claim(&c).is_err());
    }

    #[test]
    fn validate_rejects_should_persist_false() {
        let c = ExtractedClaim {
            fact: "something".into(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Workspace,
            certainty: Certainty::Explicit,
            confidence: 0.5,
            should_persist: false,
        };
        assert!(validate_claim(&c).is_err());
    }

    #[test]
    fn validate_rejects_secret_patterns() {
        let cases = [
            "The API key is sk-1234567890abcdefghijklmnop",
            "My password=hunter2longsecretvalue",
            "token=abcdef0123456789abcdef0123456789",
            "secret=0123456789abcdef0123456789abcdef",
            "-----BEGIN PRIVATE KEY-----",
        ];
        for content in cases {
            let c = claim(content, MemoryKind::Fact, MemoryScope::Workspace, Certainty::Explicit);
            assert!(
                validate_claim(&c).is_err(),
                "should reject secret: {content}"
            );
        }
    }

    #[test]
    fn validate_rejects_overlong_content() {
        let long = "a".repeat(MAX_CONTENT_LEN + 1);
        let c = claim(&long, MemoryKind::Fact, MemoryScope::Workspace, Certainty::Explicit);
        assert!(validate_claim(&c).is_err());
    }

    #[test]
    fn proposal_id_is_deterministic() {
        let s = src();
        let id1 = proposal_id("same content", &s);
        let id2 = proposal_id("same content", &s);
        assert_eq!(id1, id2);
        let id3 = proposal_id("different content", &s);
        assert_ne!(id1, id3);
    }

    #[test]
    fn propose_and_commit_full_flow() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let result = ExtractionResult {
            claims: vec![
                claim("用户希望被称呼为 ikkk。", MemoryKind::Preference, MemoryScope::Global, Certainty::Explicit),
                ExtractedClaim {
                    fact: "borderline".into(),
                    kind: MemoryKind::Fact,
                    scope: MemoryScope::Workspace,
                    certainty: Certainty::Inferred,
                    confidence: 0.3,
                    should_persist: false, // rejected
                },
            ],
            source: src(),
        };
        let report = propose_and_commit(&db, &result, "test-model");
        assert_eq!(report.proposed, 1);
        assert_eq!(report.committed, 1);
        assert_eq!(report.rejected.len(), 1);
        assert_eq!(report.committed_ids.len(), 1);
        // Verify memory unit exists in DB.
        let mem = db.get_memory_unit(&report.committed_ids[0]).unwrap().unwrap();
        assert_eq!(mem.content, "用户希望被称呼为 ikkk。");
        assert_eq!(mem.kind, MemoryKind::Preference);
        assert_eq!(mem.scope, MemoryScope::Global);
        // `fts5_memory_candidates ... WHERE m.status='active'` gate lets
        // retrieval surface the memory immediately. Candidate is reserved
        // for future LLM pre-approval flow.
        assert_eq!(mem.status, UnitStatus::Active);
    }

    #[test]
    fn propose_and_commit_is_idempotent() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let result = ExtractionResult {
            claims: vec![
                claim("用户喜欢 Rust。", MemoryKind::Preference, MemoryScope::Global, Certainty::Explicit),
            ],
            source: src(),
        };
        // First run commits.
        let r1 = propose_and_commit(&db, &result, "test-model");
        assert_eq!(r1.committed, 1);
        // Second run: same proposal_id → idempotent insert, commit returns same memory.
        let r2 = propose_and_commit(&db, &result, "test-model");
        // The proposal was already committed (status=committed), so
        // commit_proposal's SELECT will return nothing → error or no-op.
        // Either way, no duplicate memory unit should be created.
        assert_eq!(r2.committed_ids.len(), 0, "should not create duplicate on re-run");
    }

    // ═══════════════════════════════════════════════════════════════
    // End-to-end W4-5 regression: "记住我叫 ikkk" → 后续问 "我叫什么"
    // 必须能够从 retriever 里召回该偏好。
    //
    // This is the W4-minimum-viable-closure contract:
    //   1. Rule/Mock extractor produces the "称呼为 ikkk" claim from
    //      user input (matches extraction from real supervisor flow).
    //   2. propose_and_commit writes status=Active + CJK-enriches the
    //      FTS row.
    //   3. Router opens the memory leg for "我叫什么" (Fix 1 — identity
    //      signal whitelist).
    //   4. FTS bigram+unigram OR query + enrichment → BM25 candidate.
    //   5. CJK-aware term-coverage gate passes (Fix 3 — per-Han terms).
    //   6. End-to-end retriever returns the preference memory.
    // ═══════════════════════════════════════════════════════════════
    #[tokio::test]
    async fn e2e_remember_name_then_query_hits_memory() {
        use crate::llm_extractor::MockEvidenceExtractor;
        use crate::retrievers::{MemoryRetriever, RetrievalConfig};

        let db = MemoryDatabase::open_in_memory().unwrap();
        let extractor = MockEvidenceExtractor::default();

        // ── Step A: "记住我叫 ikkk" turn is extracted ──────────────
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
        assert!(
            !extraction.claims.is_empty(),
            "MockEvidenceExtractor should have produced a name-preference claim"
        );
        let report = propose_and_commit(&db, &extraction, "mock:rule");
        assert_eq!(
            report.committed, 1,
            "expected 1 committed memory, report: {report:?}"
        );
        assert!(report.rejected.is_empty(), "rejected: {:?}", report.rejected);
        let mem = db
            .get_memory_unit(&report.committed_ids[0])
            .unwrap()
            .expect("committed memory unit must be fetchable");
        assert_eq!(mem.status, crate::types::UnitStatus::Active);
        assert_eq!(mem.scope, MemoryScope::Global);

        // ── Step B: Next turn user asks "我叫什么" → retriever hits ──
        let retriever = MemoryRetriever::new(db.clone(), RetrievalConfig::default());
        let (results, _) = retriever.retrieve("我叫什么");
        let has_name_memory = results.iter().any(|r| {
            r.content.contains("ikkk")
                || r.content.contains("称呼")
                || r.content.contains("叫")
        });
        assert!(
            has_name_memory,
            "Expected '我叫什么' to surface the ikkk preference memory.\n\
             Results: {results:#?}"
        );

        // Also verify via the IntentRouter that this identity query
        // correctly opened the memory leg (Fix 1 regression guard).
        use crate::router::IntentRouter;
        let decision = IntentRouter::route("我叫什么");
        assert!(
            decision.memory_enabled,
            "router must enable memory for identity query '我叫什么'. decision: {decision:?}"
        );
        assert!(
            decision.reason_codes.iter().any(|r| r.starts_with("memory_signal:")),
            "router should hit the identity-signal whitelist not merely the term-count fallback. reasons: {:?}",
            decision.reason_codes
        );
    }
}
