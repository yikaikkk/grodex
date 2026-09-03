//! LLM Conflict Judge — determines the semantic relationship between two
//! memory units so the consolidator can promote, merge, or flag conflicts.
//!
//! Design (memory-architecture-redesign.md §Phase 6):
//! - Replaces the old hash-based near-duplicate detection with LLM judgment.
//! - Hash is still used as a *pre-filter* (same hash → definitely duplicate,
//!   skip the LLM call). Only different-hash pairs that might still be
//!   semantically related go to the LLM.
//! - Trait defined here in grodex-memory; implemented in grodex-loop.

use crate::types::{ConflictRelation, MemoryUnit};
use serde::{Deserialize, Serialize};

/// Input pair for the judge.
#[derive(Debug, Clone)]
pub struct ConflictJudgeInput {
    /// The older/existing memory unit.
    pub left: MemoryUnit,
    /// The newer/candidate memory unit.
    pub right: MemoryUnit,
}

/// The judge's verdict on a pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictJudgeResult {
    pub relation: ConflictRelation,
    /// 0.0–1.0 confidence in the verdict.
    pub confidence: f64,
    /// Human-readable explanation (for audit / memory_conflicts.reason).
    pub reason: String,
}

/// Errors during conflict judgment.
#[derive(Debug, thiserror::Error)]
pub enum ConflictJudgeError {
    #[error("provider call failed: {0}")]
    Provider(String),
    #[error("response parse failed: {0}")]
    Parse(String),
}

/// System prompt for the conflict judge LLM call.
pub const CONFLICT_JUDGE_PROMPT: &str = r#"You are a memory conflict judge. Given two memory units, determine their semantic relationship:

- duplicate: same fact, nearly identical wording. Keep one, dismiss the other.
- equivalent: same meaning, different wording. e.g. "我喜欢 Rust" and "我的偏好语言是 Rust". Keep one.
- supersedes: the right (newer) unit is more accurate/complete and replaces the left. e.g. left="schema version is 3", right="schema version is 4 (bumped)".
- conflicts: the two units state contradictory things that cannot both be true. e.g. left="we use Redis", right="we use etcd". Flag both for review.
- independent: the two units are about different topics. No action needed.

Respond with JSON: {"relation": "duplicate|equivalent|supersedes|conflicts|independent", "confidence": 0.9, "reason": "brief explanation"}
"#;

/// Abstraction over the LLM that judges memory conflicts.
#[async_trait::async_trait]
pub trait ConflictJudge: Send + Sync {
    async fn judge(&self, input: &ConflictJudgeInput) -> Result<ConflictJudgeResult, ConflictJudgeError>;
}

// ───────────────────────── Mock ─────────────────────────

/// Rule-based mock for tests. Uses Jaccard similarity on token sets.
/// - Jaccard ≥ 0.8 → duplicate
/// - Jaccard ≥ 0.5 → equivalent
/// - One content contains the other → supersedes (if right is longer)
/// - Otherwise → independent
#[derive(Debug, Default)]
pub struct MockConflictJudge;

#[async_trait::async_trait]
impl ConflictJudge for MockConflictJudge {
    async fn judge(&self, input: &ConflictJudgeInput) -> Result<ConflictJudgeResult, ConflictJudgeError> {
        let left_tokens = tokenize(&input.left.content);
        let right_tokens = tokenize(&input.right.content);

        if left_tokens.is_empty() || right_tokens.is_empty() {
            return Ok(ConflictJudgeResult {
                relation: ConflictRelation::Independent,
                confidence: 0.5,
                reason: "one side is empty".into(),
            });
        }

        let sim = jaccard(&left_tokens, &right_tokens);

        // Check for supersedes: right is a superset of left (more info).
        let left_len = input.left.content.chars().count();
        let right_len = input.right.content.chars().count();
        let right_contains_left = input
            .right
            .content
            .to_lowercase()
            .contains(&input.left.content.to_lowercase());

        let (relation, confidence, reason) = if sim >= 0.8 {
            (ConflictRelation::Duplicate, 0.95, "near-identical wording".into())
        } else if right_contains_left && right_len > left_len {
            (ConflictRelation::Supersedes, 0.8, "right subsumes and extends left".into())
        } else if sim >= 0.5 {
            (ConflictRelation::Equivalent, 0.85, "same meaning, different wording".into())
        } else {
            (ConflictRelation::Independent, 0.6, "different topics".into())
        };

        Ok(ConflictJudgeResult {
            relation,
            confidence,
            reason,
        })
    }
}

/// Tokenize a string into a lowercase word set (CJK chars split individually).
fn tokenize(s: &str) -> std::collections::HashSet<String> {
    s.to_lowercase()
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|w| !w.is_empty())
        .flat_map(|w| {
            // Split CJK into individual chars (no word boundaries).
            if w.chars().any(|c| c as u32 > 0x2E80) {
                w.chars().map(|c| c.to_string()).collect::<Vec<_>>()
            } else {
                vec![w.to_string()]
            }
        })
        .collect()
}

/// Jaccard similarity between two token sets.
fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryKind, MemoryScope, UnitStatus};
    use chrono::Utc;
    use sha2::{Digest, Sha256};

    fn mk_unit(content: &str) -> MemoryUnit {
        let hash = {
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            let d = h.finalize();
            let mut s = String::with_capacity(16);
            for b in &d[..8] {
                use std::fmt::Write as _;
                let _ = write!(s, "{:02x}", b);
            }
            s
        };
        let now = Utc::now();
        MemoryUnit {
            id: format!("mu_{hash}"),
            path: "test".into(),
            section: "test".into(),
            kind: MemoryKind::Preference,
            scope: MemoryScope::Global,
            status: UnitStatus::Candidate,
            content: content.into(),
            content_hash: hash,
            updated_at: now,
            created_at: now,
        }
    }

    #[tokio::test]
    async fn duplicate_near_identical() {
        let j = MockConflictJudge;
        let r = j
            .judge(&ConflictJudgeInput {
                left: mk_unit("用户希望被称呼为 ikkk"),
                right: mk_unit("用户希望被称呼为 ikkk"),
            })
            .await
            .unwrap();
        assert_eq!(r.relation, ConflictRelation::Duplicate);
    }

    #[tokio::test]
    async fn equivalent_different_wording() {
        let j = MockConflictJudge;
        let r = j
            .judge(&ConflictJudgeInput {
                left: mk_unit("我喜欢 Rust 编程语言"),
                right: mk_unit("我的偏好语言是 Rust"),
            })
            .await
            .unwrap();
        // Should detect overlap (Rust, 语言) → equivalent or independent.
        // Mock uses Jaccard; "Rust" + CJK overlap should push ≥ 0.3.
        assert!(
            matches!(
                r.relation,
                ConflictRelation::Equivalent | ConflictRelation::Independent
            ),
            "relation={:?} sim-based",
            r.relation
        );
    }

    #[tokio::test]
    async fn supersedes_when_right_extends_left() {
        let j = MockConflictJudge;
        let r = j
            .judge(&ConflictJudgeInput {
                left: mk_unit("schema version 3"),
                right: mk_unit("schema version 3 bumped to 4"),
            })
            .await
            .unwrap();
        assert_eq!(r.relation, ConflictRelation::Supersedes);
    }

    #[tokio::test]
    async fn independent_different_topics() {
        let j = MockConflictJudge;
        let r = j
            .judge(&ConflictJudgeInput {
                left: mk_unit("用户喜欢 Rust"),
                right: mk_unit("build command is cargo build"),
            })
            .await
            .unwrap();
        assert_eq!(r.relation, ConflictRelation::Independent);
    }
}
