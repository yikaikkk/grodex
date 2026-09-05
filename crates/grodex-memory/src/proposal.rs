//! Memory Proposal + Commit flow — turns LLM-extracted claims into
//! validated, persisted memory units.
//!
//! Pipeline (memory-architecture-redesign.md §Phase 5):
//! 1. `ExtractionResult` (from the LLM extractor) → per-claim validation
//! 2. Valid claims → `MemoryProposal` rows (status=pending)
//! 3. `commit_proposal_with_status` → `memory_units` (target status from
//!    gate decision: Candidate OR Active, not always candidate). RC3B:
//!    if a matching Candidate already exists, we UPDATE it to Active in
//!    place (no fresh insert) so the same fact never occupies two rows.
//!
//! Hard-constraint validation (fail-closed):
//! - `should_persist == false` → skipped (logged, not persisted)
//! - content length ≤ 2000 chars
//! - content must NOT match secret patterns (API keys, private keys, passwords)
//! - scope=Global requires certainty=Explicit (no inferred global prefs)

use crate::database::MemoryDatabase;
use crate::llm_extractor::{
    ExtractedClaim, ExtractionResult, MemoryRuleMode, MemoryWriteGateDecision, SourceRef,
    gate_extraction_output,
};
use crate::types::{Certainty, MemoryKind, MemoryProposal, MemoryScope, ProposalStatus, UnitStatus};
use chrono::Utc;
use sha2::{Digest, Sha256};

/// Max allowed content length for a single memory unit.
const MAX_CONTENT_LEN: usize = 2000;

/// How many top-matching evidence rows to scan per claim when filling
/// `source_evidence_ids` (P0-9). Tied to provenance cardinality: keep
/// tight so a single claim never swallows the whole evidence table.
const MAX_EVIDENCE_PER_CLAIM: usize = 5;

/// Options controlling the propose-and-commit gate. Split into a struct
/// so callers (supervisor, runtime, future governance pass) can extend
/// it without propagating 4-ary positional arguments across 3 crates.
#[derive(Debug, Clone)]
pub struct ProposalGateOptions {
    pub rule_mode: MemoryRuleMode,
    /// When true AND `rule_mode=AllowCandidate`, claims tagged
    /// `EvidenceAuthority::UserExplicitStatement` still promote to
    /// Active so identity preferences ("记住我叫 ikkk") stay end-to-end
    /// usable even with rule-only extraction, total provider outage,
    /// or LLM JSON parse failure (P0-2 fail-safe identity gate).
    pub force_user_explicit_active: bool,
    /// Tier label string, used to distinguish rule-tier output from
    /// LLM-tier output inside `gate_extraction_output`.
    pub tier_label: String,
}

impl Default for ProposalGateOptions {
    fn default() -> Self {
        Self {
            rule_mode: MemoryRuleMode::default(),
            force_user_explicit_active: true,
            tier_label: "unknown".into(),
        }
    }
}

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
///
/// `source_evidence_ids` are supplied by the caller — typically
/// populated via `lookup_evidence_ids_for_claim` right before proposal
/// insert (P0-9 fix).
pub fn create_proposal(
    claim: &ExtractedClaim,
    source: &SourceRef,
    extractor_model: &str,
    source_evidence_ids: Vec<String>,
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
        source_evidence_ids,
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
/// Validates each claim, runs the P0 source-boundary / rule-mode gate,
/// writes proposals, and commits valid ones with the correct status.
/// Returns a report of what was proposed / committed / rejected.
///
/// Gate behaviour (see `gate_extraction_output` in `llm_extractor`):
///   - ToolObservation claims → always Skip (P0-6 hard line)
///   - AssistantInference → clamped to Candidate (P0-7)
///   - Rule-tier output respects `gate_opts.rule_mode`:
///       * Disabled → all Skip
///       * AllowCandidate (default) → rule claims default to Candidate,
///         except UserExplicitStatement claims are promoted to Active iff
///         `gate_opts.force_user_explicit_active` (on by default so the
///         identity preference "记住我叫 X" survives total LLM outage).
///       * AllowActive → authority alone decides (for audited deployments).
///   - LLM-tier output → authority alone decides (Active only for
///     UserExplicitStatement / AssistantAcknowledged).
pub fn propose_and_commit(
    db: &MemoryDatabase,
    result: &ExtractionResult,
    extractor_model: &str,
    gate_opts: &ProposalGateOptions,
) -> ProposalCommitReport {
    let mut report = ProposalCommitReport::default();

    for claim in &result.claims {
        // 1. Hard-constraint validation (fail-closed).
        if let Err(reason) = validate_claim(claim) {
            report.rejected.push(RejectedClaim {
                fact: claim.fact.clone(),
                reason,
            });
            continue;
        }

        // 2. P0 write gate — authority + tier + rule-mode decision.
        let gate_decision = gate_extraction_output(
            claim,
            &gate_opts.tier_label,
            gate_opts.rule_mode,
            gate_opts.force_user_explicit_active,
        );
        let (commit_status, gate_reason) = match gate_decision {
            MemoryWriteGateDecision::Skip { reason } => {
                // Skip entirely: don't even persist a proposal row for
                // claims the gate has explicitly dropped (e.g. tool
                // results, rule tier Disabled). This avoids piling up
                // empty Pending proposals that would never be audited.
                report.rejected.push(RejectedClaim {
                    fact: claim.fact.clone(),
                    reason: format!("gate=skip: {reason}"),
                });
                continue;
            }
            MemoryWriteGateDecision::PromoteCandidate { reason } => (UnitStatus::Candidate, reason),
            MemoryWriteGateDecision::PromoteActive { reason } => (UnitStatus::Active, reason),
        };

        // 3. (P0-9) Best-effort evidence_id backfill for provenance.
        let evidence_ids = lookup_evidence_ids_for_claim(
            db,
            claim,
            &result.source,
            MAX_EVIDENCE_PER_CLAIM,
        );

        // 4. Create proposal (carries evidence_ids so the DB insert row
        // is already linked — not left empty like before the fix).
        let mut proposal = create_proposal(
            claim,
            &result.source,
            extractor_model,
            evidence_ids,
        );
        // Stash the gate reason in rejection_reason for diagnostics.
        // Non-Skip gates never reject, but this field is still useful
        // when auditing proposal rows (e.g. "why was this candidate?").
        proposal.rejection_reason = format!(
            "gate={};target_status={}",
            commit_status.as_str(),
            gate_reason
        );

        // 5. Insert proposal (idempotent on proposal_id).
        if let Err(e) = db.insert_proposal(&proposal) {
            report.rejected.push(RejectedClaim {
                fact: claim.fact.clone(),
                reason: format!("insert_proposal failed: {e}"),
            });
            continue;
        }
        report.proposed += 1;

        // 6. Commit → memory_units.
        //
        // RC3B (repetition-promotion path): When the target status is
        // Active, BEFORE inserting a brand new unit, look for an existing
        // Candidate row that matches the same (scope, kind, normalized
        // fact). If found, PROMOTE that row instead of writing a second
        // memory unit — this avoids the "1 candidate + 1 active" dual
        // write that would otherwise occur between RC2b's identity-net
        // injection and a later LLM repetition / confirmation of the
        // same fact.
        //
        // If no such candidate exists, fall through to normal commit.
        if matches!(commit_status, UnitStatus::Active) {
            let norm_key = normalize_fact_for_promotion(&claim.fact);
            if let Some((existing_id, existing_proposal_id)) = db
                .find_candidate_for_promotion(&norm_key, claim.kind, claim.scope)
                .unwrap_or(None)
            {
                match db.promote_candidate_to_active(
                    &existing_id,
                    &existing_proposal_id,
                    Some(&proposal.proposal_id),
                    &proposal.rejection_reason,
                ) {
                    Ok(_) => {
                        report.committed += 1;
                        report.committed_ids.push(existing_id);
                        continue;
                    }
                    Err(e) => {
                        // Fail-open: the normal commit path is still our
                        // fallback. If both fail the catchall bubble logs.
                        tracing::warn!(
                            error = %e,
                            existing = %existing_id,
                            "RC3B: promote_candidate_to_active failed; falling back to fresh insert"
                        );
                    }
                }
            }
        }

        // Skip if already exists to avoid duplicate FTS / edge work on re-runs.
        let memory_id = format!("mem_{}", &proposal.proposal_id[5..]); // strip "prop_"
        if db.get_memory_unit(&memory_id).ok().flatten().is_some() {
            continue;
        }
        match db.commit_proposal_with_status(&proposal.proposal_id, &memory_id, commit_status) {
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

/// (P0-9) Look up evidence IDs most relevant to a claim so
/// `memory_proposals.source_evidence_ids` and the linked memory unit
/// actually point at evidence rows instead of being permanently `[]`.
///
/// Strategy (best-effort, fails silently to avoid blocking extraction):
///   1. Prefer rows whose `rollout_id` exactly matches the claim source
///      (same-session evidence). Narrow to `content` overlap with the
///      claim: search the claim's 4-16 char n-grams in evidence's
///      FTS index via LIKE (we don't rebuild a whole FTS query here —
///      simple substring match + deterministic order by id is cheap
///      and sufficient to link user-input + assistant-summary evidence
///      to the corresponding claim most of the time).
///   2. As a fallback if nothing matches, return any rows from the same
///      rollout/turn so provenance at least points somewhere traceable.
///   3. Empty vector if we still find nothing (downstream commit is
///      tolerant, edges are simply not inserted for unknown ids).
pub fn lookup_evidence_ids_for_claim(
    db: &MemoryDatabase,
    claim: &ExtractedClaim,
    source: &SourceRef,
    limit: usize,
) -> Vec<String> {
    use rusqlite::params;
    let conn = match db.conn.lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    // 1) Match on rollout + content LIKE fingerprint substrings.
    let needles = content_fingerprint_needles(&claim.fact);
    let mut ids: Vec<String> = Vec::new();
    for needle in needles {
        if ids.len() >= limit {
            break;
        }
        let like = format!("%{needle}%");
        let sql = "SELECT id FROM evidence_units \
                   WHERE rollout_id = ?1 AND content LIKE ?2 \
                   ORDER BY id LIMIT ?3";
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rows = stmt.query_map(
            params![source.rollout_id, like, limit as i64],
            |r| r.get::<_, String>(0),
        );
        if let Ok(rows) = rows {
            for r in rows.flatten() {
                if !ids.contains(&r) {
                    ids.push(r);
                    if ids.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    // 2) Fallback: any evidence from the same rollout.
    if ids.len() < limit {
        let remaining = limit - ids.len();
        let sql = "SELECT id FROM evidence_units WHERE rollout_id = ?1 ORDER BY id LIMIT ?2";
        if let Ok(mut stmt) = conn.prepare(sql) {
            if let Ok(rows) = stmt.query_map(
                params![source.rollout_id, remaining as i64],
                |r| r.get::<_, String>(0),
            ) {
                for r in rows.flatten() {
                    if !ids.contains(&r) {
                        ids.push(r);
                        if ids.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
    }
    ids
}

// ─── RC3B: repetition-promotion helpers ─────────────────────────────

/// (RC3B) Normalize a claim fact for candidate↔repetition matching.
/// The goal is to tolerate different phrasings of the SAME underlying
/// semantic (identity/preference specifically). Transformations:
///   - lowercase the ASCII portion.
///   - strip ASCII and common CJK punctuation.
///   - collapse runs of whitespace into a single space.
///   - additionally, if `extract_name` recognizes an embedded name,
///     ALSO append `#name=<name>` to the key so identity claims match
///     across languages (e.g. "用户希望被称呼为 ikkk" vs
///     "The user's name is ikkk.").
///
/// This is intentionally conservative — it MUST NOT produce false
/// positives across non-identical claims; it's OK if it misses a few
/// (those candidates are simply purged after the TTL window by RC3A).
pub fn normalize_fact_for_promotion(fact: &str) -> String {
    use crate::llm_extractor::extract_name;
    use std::collections::BTreeSet;

    // Strip punctuation.
    let mut buf = String::with_capacity(fact.len());
    let mut ascii_run = String::new();
    let mut ascii_tokens: BTreeSet<String> = BTreeSet::new();
    for c in fact.chars() {
        if c.is_ascii_punctuation() {
            // End ascii run on punctuation.
            if ascii_run.len() >= 2 {
                ascii_tokens.insert(std::mem::take(&mut ascii_run).to_lowercase());
            }
            continue;
        }
        if "。！？，、；：「」『』《》〈〉“”‘’…—–·・　".contains(c) {
            if ascii_run.len() >= 2 {
                ascii_tokens.insert(std::mem::take(&mut ascii_run).to_lowercase());
            }
            continue;
        }
        if c.is_whitespace() {
            if ascii_run.len() >= 2 {
                ascii_tokens.insert(std::mem::take(&mut ascii_run).to_lowercase());
            }
            buf.push(' ');
        } else if c.is_ascii_alphanumeric() {
            ascii_run.push(c);
            for low in c.to_lowercase() {
                buf.push(low);
            }
        } else {
            if ascii_run.len() >= 2 {
                ascii_tokens.insert(std::mem::take(&mut ascii_run).to_lowercase());
            }
            for low in c.to_lowercase() {
                buf.push(low);
            }
        }
    }
    if ascii_run.len() >= 2 {
        ascii_tokens.insert(ascii_run.to_lowercase());
    }
    // Collapse whitespace.
    let collapsed: String = buf
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // Anchor 1: embedded identity name recognized by extract_name (user
    // input style phrases). If fact's text happens to begin with a
    // "call me ..." / "我叫..." (unusual in facts, but harmless).
    let mut anchors: Vec<String> = Vec::with_capacity(2);
    if let Some(name) = extract_name(fact) {
        anchors.push(format!("name={}", name.trim().to_lowercase()));
    }
    // Anchor 2: sorted unique ASCII alphanumeric tokens (≥ 2 chars).
    // This is the workhorse for cross-language matching: both
    // "用户希望被称呼为 ikkk" and "The user's name is ikkk." contain
    // the token "ikkk".
    if !ascii_tokens.is_empty() {
        anchors.push(format!(
            "tok={}",
            ascii_tokens.into_iter().collect::<Vec<_>>().join(",")
        ));
    }
    if anchors.is_empty() {
        collapsed
    } else {
        format!("{}#{}", collapsed, anchors.join("|"))
    }
}

/// Build 2-3 stable "fingerprint" substrings from the claim's fact
/// text for a cheap LIKE-based evidence join. Avoids regex, keeps UTF-8
/// bytes intact, and guarantees at least one slice so empty facts fall
/// back to the rollout-only path in `lookup_evidence_ids_for_claim`.
fn content_fingerprint_needles(content: &str) -> Vec<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    // Take head(12) + tail(12) + any contiguous 8-char slice in the middle.
    let chars: Vec<char> = trimmed.chars().collect();
    let mut out: Vec<String> = Vec::with_capacity(3);
    let head: String = chars.iter().take(12).collect();
    if !head.is_empty() {
        out.push(head);
    }
    if chars.len() > 12 {
        let tail: String = chars.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
        if out.first() != Some(&tail) {
            out.push(tail);
        }
    }
    if chars.len() >= 24 {
        let mid: String = chars.iter().skip(chars.len() / 2).take(8).collect();
        out.push(mid);
    }
    out.into_iter().filter(|s| s.chars().count() >= 3).collect()
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
            ..Default::default()
        }
    }

    fn default_gate() -> ProposalGateOptions {
        ProposalGateOptions {
            rule_mode: MemoryRuleMode::AllowActive,
            force_user_explicit_active: true,
            tier_label: "test:mock".into(),
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
            authority: EvidenceAuthority::default(),
            provenance_hint: "test_default".into(),
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
                {
                    let mut c = claim("用户希望被称呼为 ikkk。", MemoryKind::Preference, MemoryScope::Global, Certainty::Explicit);
                    c.authority = EvidenceAuthority::UserExplicitStatement;
                    c.provenance_hint = "user:explicit".into();
                    c
                },
                ExtractedClaim {
                    fact: "borderline".into(),
                    kind: MemoryKind::Fact,
                    scope: MemoryScope::Workspace,
                    certainty: Certainty::Inferred,
                    confidence: 0.3,
                    should_persist: false, // rejected
                    ..Default::default()
                },
            ],
            source: src(),
            strongest_authority_in_context: Default::default(),
        };
        let report = propose_and_commit(&db, &result, "test-model", &default_gate());
        assert_eq!(report.proposed, 1);
        assert_eq!(report.committed, 1);
        assert_eq!(report.rejected.len(), 1);
        assert_eq!(report.committed_ids.len(), 1);
        // Verify memory unit exists in DB.
        let mem = db.get_memory_unit(&report.committed_ids[0]).unwrap().unwrap();
        assert_eq!(mem.content, "用户希望被称呼为 ikkk。");
        assert_eq!(mem.kind, MemoryKind::Preference);
        assert_eq!(mem.scope, MemoryScope::Global);
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
            strongest_authority_in_context: Default::default(),
        };
        // First run commits.
        let r1 = propose_and_commit(&db, &result, "test-model", &default_gate());
        assert_eq!(r1.committed, 1);
        // Second run: same proposal_id → idempotent insert, commit returns same memory.
        let r2 = propose_and_commit(&db, &result, "test-model", &default_gate());
        assert_eq!(r2.committed_ids.len(), 0, "should not create duplicate on re-run");
    }

    // ─── P0-3/4/6/7 gate tests ─────────────────────────────────
    use crate::llm_extractor::EvidenceAuthority;

    #[test]
    fn gate_skips_tool_observation() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let claim = ExtractedClaim {
            fact: "文件大小 12MB".into(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Workspace,
            certainty: Certainty::Explicit,
            confidence: 0.9,
            should_persist: true,
            authority: EvidenceAuthority::ToolObservation,
            provenance_hint: "tool_result".into(),
        };
        let result = ExtractionResult {
            claims: vec![claim],
            source: src(),
            strongest_authority_in_context: EvidenceAuthority::ToolObservation,
        };
        let opts = ProposalGateOptions {
            rule_mode: MemoryRuleMode::AllowActive,
            force_user_explicit_active: true,
            tier_label: "sampling:llm".into(),
        };
        let report = propose_and_commit(&db, &result, "t", &opts);
        assert_eq!(report.committed, 0, "ToolObservation claims must NOT promote to memory (P0-6)");
        assert!(
            report.rejected.iter().any(|r| r.reason.contains("gate=skip")),
            "tool claim should be rejected via gate skip: {report:?}"
        );
    }

    #[test]
    fn rule_mode_disabled_never_writes_active() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let claim = ExtractedClaim {
            fact: "用户偏好：叫我 zxcv。".into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Global,
            certainty: Certainty::Explicit,
            confidence: 0.95,
            should_persist: true,
            authority: EvidenceAuthority::UserExplicitStatement,
            provenance_hint: "rule_keyword_user_voice_only".into(),
        };
        let result = ExtractionResult {
            claims: vec![claim],
            source: src(),
            strongest_authority_in_context: EvidenceAuthority::UserExplicitStatement,
        };
        let opts = ProposalGateOptions {
            rule_mode: MemoryRuleMode::Disabled,
            force_user_explicit_active: true,
            tier_label: "composite:rule-only".into(),
        };
        let report = propose_and_commit(&db, &result, "t", &opts);
        assert_eq!(report.committed, 0, "Disabled rule tier must not write any memory");
    }

    #[test]
    fn rule_mode_allow_candidate_promotes_non_user_to_candidate() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let claim = ExtractedClaim {
            fact: "用户可能在使用 Rust。".into(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Workspace,
            certainty: Certainty::Inferred,
            confidence: 0.7,
            should_persist: true,
            // Unlabelled: Default is AssistantSummary → Candidate.
            authority: EvidenceAuthority::AssistantSummary,
            provenance_hint: "rule_soft_fallback".into(),
        };
        let result = ExtractionResult {
            claims: vec![claim],
            source: src(),
            strongest_authority_in_context: EvidenceAuthority::AssistantSummary,
        };
        let opts = ProposalGateOptions {
            rule_mode: MemoryRuleMode::AllowCandidate,
            force_user_explicit_active: true,
            tier_label: "composite:rule-only".into(),
        };
        let report = propose_and_commit(&db, &result, "t", &opts);
        assert_eq!(report.committed, 1);
        let mem = db.get_memory_unit(&report.committed_ids[0]).unwrap().unwrap();
        // Rule + AllowCandidate without user-explicit → Candidate
        assert_eq!(mem.status, UnitStatus::Candidate);
    }

    #[test]
    fn rule_mode_allow_candidate_identity_still_active() {
        // Regression guard: "记住我叫 ikkk" → identity preference must
        // still write Active memory even with rule-only + AllowCandidate
        // default, because `force_user_explicit_active=true` and the
        // rule tier tagged authority=UserExplicitStatement (P0-2
        // fail-safe identity path).
        let db = MemoryDatabase::open_in_memory().unwrap();
        let claim = ExtractedClaim {
            fact: "用户偏好：叫我 ikkk（希望被称呼为 ikkk）。".into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Global,
            certainty: Certainty::Explicit,
            confidence: 0.95,
            should_persist: true,
            authority: EvidenceAuthority::UserExplicitStatement,
            provenance_hint: "rule_keyword_user_voice_only:name_prefix".into(),
        };
        let result = ExtractionResult {
            claims: vec![claim],
            source: src(),
            strongest_authority_in_context: EvidenceAuthority::UserExplicitStatement,
        };
        let opts = ProposalGateOptions {
            rule_mode: MemoryRuleMode::AllowCandidate,
            force_user_explicit_active: true,
            tier_label: "composite:rule-only".into(),
        };
        let report = propose_and_commit(&db, &result, "t", &opts);
        assert_eq!(report.committed, 1);
        let mem = db.get_memory_unit(&report.committed_ids[0]).unwrap().unwrap();
        assert_eq!(mem.status, UnitStatus::Active,
            "identity preference via rule tier must still be Active under AllowCandidate+force_user_explicit_active");
        // source_evidence_ids is at least not `[]` when no evidence rows exist (still serialized from empty)
        // — we verify it's a valid JSON array by round-tripping:
        let ids: Vec<String> = serde_json::from_str(
            &db.conn.lock().unwrap()
                .query_row(
                    "SELECT source_evidence_ids FROM memory_units WHERE id=?1",
                    rusqlite::params![mem.id],
                    |r| r.get::<_, String>(0),
                ).unwrap(),
        ).unwrap();
        assert!(ids.is_empty(), "no evidence inserted, expect empty array; got {ids:?}");
    }

    // ─── P0-9: source_evidence_ids backfill ────────────────────
    #[test]
    fn proposal_backfills_source_evidence_ids_when_evidence_exists() {
        use crate::types::{EvidenceStatus, EvidenceUnit};
        let db = MemoryDatabase::open_in_memory().unwrap();
        let now = chrono::Utc::now();
        let eu = EvidenceUnit {
            id: "ev_testlink".into(),
            rollout_id: "test_rollout".into(),
            path: "/tmp/rollout.jsonl".into(),
            section: "用户问题".into(),
            scope: MemoryScope::Workspace,
            status: EvidenceStatus::Active,
            content: "记住我叫 ikkk".into(),
            content_hash: "abc".into(),
            occurred_at: now,
            created_at: now,
            superseded_by: None,
            superseded_at: None,
            rollout_available: true,
            rollout_expired_at: None,
            subchunk_index: 0,
            fingerprint: String::new(),
        };
        db.upsert_evidence_unit(&eu).unwrap();

        let claim = ExtractedClaim {
            fact: "用户偏好：叫我 ikkk（希望被称呼为 ikkk）。".into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Global,
            certainty: Certainty::Explicit,
            confidence: 0.95,
            should_persist: true,
            authority: EvidenceAuthority::UserExplicitStatement,
            provenance_hint: "rule_keyword_user_voice_only:name_prefix".into(),
        };
        let result = ExtractionResult {
            claims: vec![claim],
            source: src(),
            strongest_authority_in_context: EvidenceAuthority::UserExplicitStatement,
        };
        let opts = ProposalGateOptions {
            rule_mode: MemoryRuleMode::AllowCandidate,
            force_user_explicit_active: true,
            tier_label: "composite:rule-only".into(),
        };
        let report = propose_and_commit(&db, &result, "t", &opts);
        assert_eq!(report.committed, 1);
        let mid = &report.committed_ids[0];
        let mem = db.get_memory_unit(mid).unwrap().unwrap();
        assert_eq!(mem.status, UnitStatus::Active);
        // Round-trip source_evidence_ids JSON column from memory_units.
        let ids: Vec<String> = serde_json::from_str(
            &db.conn.lock().unwrap()
                .query_row(
                    "SELECT source_evidence_ids FROM memory_units WHERE id=?1",
                    rusqlite::params![mid],
                    |r| r.get::<_, String>(0),
                ).unwrap(),
        ).unwrap();
        assert!(
            ids.iter().any(|i| i == "ev_testlink"),
            "source_evidence_ids must include backfilled evidence row. got {ids:?}"
        );
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
            assistant_segments: Vec::new(),
            strongest_user_authority: EvidenceAuthority::default(),
        };
        let extraction = extractor.extract(&extraction_ctx).await.unwrap();
        assert!(
            !extraction.claims.is_empty(),
            "MockEvidenceExtractor should have produced a name-preference claim"
        );
        let gate = ProposalGateOptions {
            rule_mode: MemoryRuleMode::AllowCandidate,
            force_user_explicit_active: true,
            tier_label: "mock:rule".into(),
        };
        let report = propose_and_commit(&db, &extraction, "mock:rule", &gate);
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
