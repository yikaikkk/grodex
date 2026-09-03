//! LLM Evidence Extractor — extracts stable, long-term-memory-worthy
//! facts from rollout events using a language model.
//!
//! Design (memory-architecture-redesign.md §Phase 4):
//! - Types live here in grodex-memory (no provider dependency).
//! - [`EvidenceExtractor`] trait is defined here, implemented in
//!   grodex-loop where the provider client is available.
//! - The extractor is called on `TurnCompleted` (real-time), with the
//!   raw [`crate::rollout_extractor`] serving as a shutdown fallback.
//! - SubAgent events are excluded by construction (the caller assembles
//!   [`ExtractionContext`] only from the main agent's user input +
//!   assistant turns + tool results).

use crate::types::{Certainty, MemoryKind, MemoryScope};
use serde::{Deserialize, Serialize};

// ───────────────────────── Input ─────────────────────────

/// A summary of a tool call within the turn being extracted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub name: String,
    pub arguments: String,
}

/// A summary of a tool result within the turn being extracted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultSummary {
    pub name: String,
    pub is_error: bool,
    /// Truncated result text (caller trims before passing in).
    pub content: String,
}

/// A raw rollout event rendered as text for the LLM context window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutEventSummary {
    pub seq: i64,
    pub event_type: String,
    pub content: String,
}

/// Everything the extractor needs to decide what (if anything) to
/// persist as long-term memory from a single turn.
#[derive(Debug, Clone, Default)]
pub struct ExtractionContext {
    /// The user's raw input for this turn (`UserInputAccepted.text`).
    pub user_input: String,
    /// Assistant text outputs produced during the turn.
    pub assistant_content: Vec<String>,
    /// Tool calls made during the turn.
    pub tool_calls: Vec<ToolCallSummary>,
    /// Tool results returned during the turn.
    pub tool_results: Vec<ToolResultSummary>,
    /// Adjacent rollout events (for temporal context).
    pub adjacent_events: Vec<RolloutEventSummary>,
    /// Existing memory contents that overlap with this turn (to avoid
    /// extracting duplicates). Plain text, one per line.
    pub existing_memory: Vec<String>,
    /// Identifies the rollout segment for provenance.
    pub source: SourceRef,
}

// ───────────────────────── Output ─────────────────────────

/// Provenance pointer back into the rollout journal.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceRef {
    pub rollout_id: String,
    pub seq_start: i64,
    pub seq_end: i64,
    pub turn_id: String,
    pub step_id: Option<String>,
}

/// A single memory-worthy fact extracted by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedClaim {
    /// Normalized, human-readable statement of the fact.
    pub fact: String,
    pub kind: MemoryKind,
    pub scope: MemoryScope,
    pub certainty: Certainty,
    /// 0.0–1.0; how confident the model is this is a stable, persistent
    /// fact rather than a transient observation.
    pub confidence: f64,
    /// If false, the claim is logged but NOT persisted (the model can
    /// flag borderline observations without writing them).
    pub should_persist: bool,
}

/// The full extraction output for one turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub claims: Vec<ExtractedClaim>,
    pub source: SourceRef,
}

// ───────────────────────── Trait ─────────────────────────

/// Errors that can arise during extraction.
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    #[error("provider call failed: {0}")]
    Provider(String),
    #[error("response was not valid JSON: {0}")]
    Parse(String),
    #[error("response schema invalid: {0}")]
    Schema(String),
    #[error("extraction skipped: {0}")]
    Skipped(String),
}

/// Abstraction over the LLM that turns an [`ExtractionContext`] into an
/// [`ExtractionResult`]. Implemented in grodex-loop with a real provider
/// client; mocked in tests.
///
/// The `'static` bound is required so the trait can be boxed as
/// `Arc<dyn EvidenceExtractor>` (object-safe) and shared across tasks.
///
/// `tier_label` lets the supervisor write a provenance tag to
/// `memory_units.extractor_model` so later governance passes can tell
/// whether a given memory unit came from the LLM tier, the rule tier,
/// or the composite. Implementations should override the default to
/// report their own label.
#[async_trait::async_trait]
pub trait EvidenceExtractor: Send + Sync + 'static {
    async fn extract(&self, ctx: &ExtractionContext) -> Result<ExtractionResult, ExtractionError>;

    /// Human-readable tier label, written to the `extractor_model`
    /// column of any memory units produced by this extractor. The
    /// default value is intentionally generic so missing overrides are
    /// trivially detectable during governance audits.
    fn tier_label(&self) -> &'static str {
        "unknown"
    }
}

// ───────────────────────── Prompt ─────────────────────────

/// The system prompt instructing the model to extract stable facts.
/// Kept here so the prompt version is co-located with the extractor
/// logic and can be surfaced in `memory_units.prompt_version`.
pub const EXTRACTOR_SYSTEM_PROMPT: &str = r#"You are a memory extraction assistant. Your job is to read the following turn context and extract STABLE, LONG-TERM-MEMORY-WORTHY facts that the user would want remembered across sessions.

EXTRACT only:
- User preferences (e.g. "remember to call me X", "I prefer dark mode")
- Stable facts about the project or environment (e.g. "the build command is cargo build --release")
- Architectural or process decisions (e.g. "we decided to use SQLite for the index")
- Long-term constraints or invariants (e.g. "the schema version must be bumped on DDL changes")
- Confirmed problems and their solutions

DO NOT extract:
- Plans or intentions ("I will do X next")
- Transient state ("the file currently has 42 lines")
- Model speculation or hypotheses
- Intermediate reasoning steps
- Code snippets or tool output verbatim
- Anything that is not user-directed or user-confirmed

RULES:
- scope=global ONLY for preferences the user explicitly stated should apply everywhere (e.g. "remember my name is X"). Default to scope=workspace.
- certainty=explicit when the user directly stated the fact. certainty=inferred when you deduce it. certainty=hypothesis for tentative conclusions.
- should_persist=false for borderline observations — log them but do not persist.
- confidence is 0.0–1.0: how sure you are this is a stable, persistent fact.
- It is perfectly valid to return an empty claims array if nothing is worth remembering.

Respond with a JSON object of this exact shape:
{"claims": [{"fact": "...", "kind": "preference|fact|decision|constraint|solution", "scope": "global|workspace", "certainty": "explicit|inferred|hypothesis", "confidence": 0.9, "should_persist": true}]}
"#;

/// The user-message body: renders the ExtractionContext as text the
/// model can read. This is the "given context" the system prompt refers to.
pub fn render_context_for_llm(ctx: &ExtractionContext) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("## User Input\n");
    out.push_str(&ctx.user_input);
    out.push_str("\n\n## Assistant Output\n");
    if ctx.assistant_content.is_empty() {
        out.push_str("(none)\n");
    } else {
        for (i, a) in ctx.assistant_content.iter().enumerate() {
            out.push_str(&format!("[{}]\n{}\n", i + 1, a));
        }
    }
    out.push_str("\n## Tool Calls\n");
    if ctx.tool_calls.is_empty() {
        out.push_str("(none)\n");
    } else {
        for c in &ctx.tool_calls {
            out.push_str(&format!("- {} ({})\n", c.name, truncate(&c.arguments, 200)));
        }
    }
    out.push_str("\n## Tool Results\n");
    if ctx.tool_results.is_empty() {
        out.push_str("(none)\n");
    } else {
        for r in &ctx.tool_results {
            out.push_str(&format!(
                "- {} {}: {}\n",
                r.name,
                if r.is_error { "ERROR" } else { "ok" },
                truncate(&r.content, 200)
            ));
        }
    }
    if !ctx.existing_memory.is_empty() {
        out.push_str("\n## Existing Memory (avoid duplicates)\n");
        for m in &ctx.existing_memory {
            out.push_str(&format!("- {}\n", truncate(m, 200)));
        }
    }
    out.push_str("\n## Source\n");
    out.push_str(&format!(
        "rollout_id={}, seq_start={}, seq_end={}, turn_id={}\n",
        ctx.source.rollout_id, ctx.source.seq_start, ctx.source.seq_end, ctx.source.turn_id
    ));
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let taken: String = s.chars().take(max).collect();
        format!("{taken}…")
    }
}

// ───────────────────────── Mock ─────────────────────────

/// A deterministic mock extractor for tests. It pattern-matches the user
/// input to produce canned claims, so tests can verify the downstream
/// proposal/commit flow without a real LLM.
#[derive(Debug, Default)]
pub struct MockEvidenceExtractor {
    /// If non-empty, always returns this error instead of extracting.
    pub force_error: Option<String>,
}

#[async_trait::async_trait]
impl EvidenceExtractor for MockEvidenceExtractor {
    fn tier_label(&self) -> &'static str {
        "mock:rule"
    }
    async fn extract(&self, ctx: &ExtractionContext) -> Result<ExtractionResult, ExtractionError> {
        if let Some(ref e) = self.force_error {
            return Err(ExtractionError::Provider(e.clone()));
        }
        let mut claims = Vec::new();
        let input = ctx.user_input.to_lowercase();

        // Pattern 1: "记住我叫 X" / "叫我 X" / "call me X"
        if let Some(name) = extract_name(&ctx.user_input) {
            // NOTE: keep the colloquial verb "叫我 X" in the fact alongside
            // the formal wording. Without it, pure-char FTS queries like
            // "我叫什么" share zero CJK characters with "用户希望被称呼
            // 为 X" and the retriever returns empty — even though the
            // underlying claim is semantically identical. Parallel
            // phrasing is robust to users mixing formal / colloquial
            // Chinese in identity queries.
            claims.push(ExtractedClaim {
                fact: format!("用户偏好：叫我 {name}（希望被称呼为 {name}）。"),
                kind: MemoryKind::Preference,
                scope: MemoryScope::Global,
                certainty: Certainty::Explicit,
                confidence: 0.95,
                should_persist: true,
            });
        } else if input.contains("记住我喜欢") || input.contains("i prefer") {
            claims.push(ExtractedClaim {
                fact: format!("用户偏好（来自输入: {}）", truncate(&ctx.user_input, 100)),
                kind: MemoryKind::Preference,
                scope: MemoryScope::Global,
                certainty: Certainty::Explicit,
                confidence: 0.9,
                should_persist: true,
            });
        }
        // Default: no claims for ordinary questions.
        Ok(ExtractionResult {
            claims,
            source: ctx.source.clone(),
        })
    }
}

/// Simple heuristic: matches "叫我 X" / "我叫 X" / "叫我X" / "call me X"
/// and returns the captured name. Returns None for non-matching input.
fn extract_name(input: &str) -> Option<String> {
    let trimmed = input.trim();
    // Chinese patterns
    for prefix in ["以后叫我", "之后叫我", "请记住我叫", "记住我叫", "叫我", "我叫", "我的名字是"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let name = rest
                .trim()
                .trim_end_matches(|c: char| c.is_ascii_punctuation() || "。！？，".contains(c));
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // English patterns
    let lower = trimmed.to_lowercase();
    if let Some(rest) = lower.strip_prefix("call me ") {
        let name = rest.trim_end_matches(|c: char| c.is_ascii_punctuation());
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    if let Some(rest) = lower.strip_prefix("my name is ") {
        let name = rest.trim_end_matches(|c: char| c.is_ascii_punctuation());
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> SourceRef {
        SourceRef {
            rollout_id: "test_rollout".into(),
            seq_start: 1,
            seq_end: 5,
            turn_id: "turn_1".into(),
            step_id: None,
        }
    }

    #[tokio::test]
    async fn mock_extracts_name_preference() {
        let ext = MockEvidenceExtractor::default();
        let ctx = ExtractionContext {
            user_input: "以后叫我 ikkk".into(),
            source: src(),
            ..Default::default()
        };
        let res = ext.extract(&ctx).await.unwrap();
        assert_eq!(res.claims.len(), 1);
        assert_eq!(res.claims[0].kind, MemoryKind::Preference);
        assert_eq!(res.claims[0].scope, MemoryScope::Global);
        assert!(res.claims[0].fact.contains("ikkk"));
        assert!(res.claims[0].should_persist);
    }

    #[tokio::test]
    async fn mock_extracts_english_name() {
        let ext = MockEvidenceExtractor::default();
        let ctx = ExtractionContext {
            user_input: "call me bob".into(),
            source: src(),
            ..Default::default()
        };
        let res = ext.extract(&ctx).await.unwrap();
        assert_eq!(res.claims.len(), 1);
        assert!(res.claims[0].fact.to_lowercase().contains("bob"));
    }

    #[tokio::test]
    async fn mock_returns_no_claims_for_ordinary_question() {
        let ext = MockEvidenceExtractor::default();
        let ctx = ExtractionContext {
            user_input: "帮我写个 hello world".into(),
            source: src(),
            ..Default::default()
        };
        let res = ext.extract(&ctx).await.unwrap();
        assert!(res.claims.is_empty(), "ordinary questions yield no claims");
    }

    #[tokio::test]
    async fn mock_returns_error_when_forced() {
        let ext = MockEvidenceExtractor {
            force_error: Some("simulated outage".into()),
        };
        let ctx = ExtractionContext {
            user_input: "以后叫我 test".into(),
            source: src(),
            ..Default::default()
        };
        let res = ext.extract(&ctx).await;
        assert!(res.is_err());
    }

    #[test]
    fn render_context_includes_user_input_and_tools() {
        let ctx = ExtractionContext {
            user_input: "记住我喜欢 Rust".into(),
            assistant_content: vec!["好的，已记住。".into()],
            tool_calls: vec![ToolCallSummary {
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
            tool_results: vec![ToolResultSummary {
                name: "read_file".into(),
                is_error: false,
                content: "file contents here".into(),
            }],
            existing_memory: vec!["用户偏好: Vim".into()],
            source: src(),
            ..Default::default()
        };
        let rendered = render_context_for_llm(&ctx);
        assert!(rendered.contains("记住我喜欢 Rust"));
        assert!(rendered.contains("read_file"));
        assert!(rendered.contains("Existing Memory"));
        assert!(rendered.contains("rollout_id=test_rollout"));
    }
}
