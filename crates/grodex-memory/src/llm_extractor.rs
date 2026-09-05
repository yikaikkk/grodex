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

// ───────────────────────── Source / Provenance ──────────────────

/// Authority (证据权威级别 P1-10 + P0-5/6/7) — 谁是这个"内容"的原始发声者。
///
/// 用来严格隔离"用户原话"vs"Assistant 推断"vs"工具观察"。
/// Memory 提交门控会根据该级别决定:
///   UserExplicitStatement / AssistantAcknowledged → 允许写 Active；
///   AssistantSummary / AssistantInference → 最多 Candidate；
///   ToolObservation → 只允许作为 Evidence，不得提升为 Active Memory。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Default = AssistantSummary. Conservative safe default: any caller that
/// forgets to tag authority falls into the "最多 Candidate" bucket,
/// preventing silent promotion of tool/inference claims into Active memory
/// (P0-6/P0-7 最后一道安全闸).
pub enum EvidenceAuthority {
    /// 用户明确表达的陈述 / 偏好（"记住我叫 ikkk"、"我喜欢 Rust"）。
    UserExplicitStatement,
    /// Assistant 逐条复述确认过的用户内容（"好的，我会记住你叫 ikkk。"）。
    /// 仍属于用户事实，但权威性比 UserExplicitStatement 低一级。
    AssistantAcknowledged,
    /// Assistant 对本轮的自然总结（没有明确把推断说成用户事实，只是总结过程）。
    AssistantSummary,
    /// Assistant 在没有明确用户输入支撑情况下给出的「你应该…」「以后…」类推断，
    /// 属于 P0-7 风险区，绝不可以直接成为 Active Memory。
    AssistantInference,
    /// Tool 直接返回的原始观察 / 项目临时状态（"文件大小 12.3MB"、"git status 显示
    /// 未提交变更 5 条"）。属于 Evidence，不能直接被记忆为用户长期事实。
    ToolObservation,
}

impl EvidenceAuthority {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserExplicitStatement => "user_explicit_statement",
            Self::AssistantAcknowledged => "assistant_acknowledged",
            Self::AssistantSummary => "assistant_summary",
            Self::AssistantInference => "assistant_inference",
            Self::ToolObservation => "tool_observation",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "user_explicit_statement" => Some(Self::UserExplicitStatement),
            "assistant_acknowledged" => Some(Self::AssistantAcknowledged),
            "assistant_summary" => Some(Self::AssistantSummary),
            "assistant_inference" => Some(Self::AssistantInference),
            "tool_observation" => Some(Self::ToolObservation),
            _ => None,
        }
    }

    /// 这个 authority 级别是否允许作为 Active Memory。
    /// 规则：只有"用户原话"+ "Assistant 显式复述确认"才允许进入 Active；
    /// ToolObservation 永远不允许（P0-6 硬约束）。
    pub fn may_become_active_memory(&self) -> bool {
        matches!(self, Self::UserExplicitStatement | Self::AssistantAcknowledged)
    }

    /// 是否是 Tool 来源（P0-6：工具结果不得直接进入长期 Memory）。
    pub fn is_tool_derived(&self) -> bool {
        matches!(self, Self::ToolObservation)
    }

    /// Short tag used by upstream provenance strings. Mirrors
    /// `as_str` but exposed under a single-ident name so provenance
    /// builders like `format!("llm_extractor:json:{}", authority.as_tag())`
    /// stay readable.
    pub fn as_tag(&self) -> &'static str {
        self.as_str()
    }
}

impl Default for EvidenceAuthority {
    /// Conservative default: unlabelled content is treated as an
    /// Assistant summary, which can at best become Candidate memory.
    fn default() -> Self {
        Self::AssistantSummary
    }
}

// ───────────────────────── Input ─────────────────────────

/// A summary of a tool call within the turn being extracted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallSummary {
    pub name: String,
    pub arguments: String,
    /// Authority: always ToolObservation (hard-coded at construction).
    /// Kept as a field so any downstream filter can rely on it without
    /// branching on type.
    #[serde(default = "tool_default_authority")]
    pub authority: EvidenceAuthority,
}

fn tool_default_authority() -> EvidenceAuthority {
    EvidenceAuthority::ToolObservation
}

impl Default for ToolCallSummary {
    fn default() -> Self {
        Self {
            name: String::new(),
            arguments: String::new(),
            authority: EvidenceAuthority::ToolObservation,
        }
    }
}

/// A summary of a tool result within the turn being extracted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultSummary {
    pub name: String,
    pub is_error: bool,
    /// Truncated result text (caller trims before passing in).
    pub content: String,
    /// Authority: always ToolObservation (hard-coded at construction).
    /// P0-6 gating rejects claims derived only from tool results.
    #[serde(default = "tool_default_authority")]
    pub authority: EvidenceAuthority,
}

impl Default for ToolResultSummary {
    fn default() -> Self {
        Self {
            name: String::new(),
            is_error: false,
            content: String::new(),
            authority: EvidenceAuthority::ToolObservation,
        }
    }
}

/// A single assistant-produced text segment with an explicit authority tag.
/// The caller (supervisor) is responsible for labelling whether this is an
/// explicit acknowledgement of a user statement, a factual summary, or a
/// speculative inference. Defaults to AssistantSummary to avoid silent
/// promotion of P0-7 risky content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantSegment {
    pub text: String,
    #[serde(default)]
    pub authority: EvidenceAuthority,
}

impl From<String> for AssistantSegment {
    fn from(text: String) -> Self {
        Self { text, authority: EvidenceAuthority::AssistantSummary }
    }
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
    ///
    /// Each segment is tagged with an `EvidenceAuthority` so the extractor
    /// (and downstream gating) can tell acknowledgement (safe) from
    /// speculation (never becomes active memory). The legacy `Vec<String>`
    /// field is preserved as `assistant_content_legacy` below for any
    /// callers that haven't migrated.
    pub assistant_segments: Vec<AssistantSegment>,
    /// Legacy assistant string buffer. Will be removed once grodex-loop
    /// fully writes `assistant_segments`. Derived at construction time if
    /// empty.
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
    /// (P0-5/6/7 门控) 这个 turn 中能被认为是"用户显式确认"的最小证据。
    /// 规则和 LLM 提取层都应把 claim 的 authority 建立在这个字段之上，
    /// 避免任何只出现在 assistant 推断或 tool result 里的内容被直接提升。
    pub strongest_user_authority: EvidenceAuthority,
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
    /// (P0-5/6/7) 声明所依据的最直接来源级别。
    /// `may_become_active_memory() == false` 的声明永远不会写 Active，
    /// 即使 extractor/规则/后续流水线出错了——提交门控再次兜底。
    #[serde(default)]
    pub authority: EvidenceAuthority,
    /// (P0-5 审计) 该 claim 的更细粒度来源标签。
    /// 例如"user_input_match" / "assistant_acknowledged_explicit" / "rule_keyword_user_voice_only"。
    #[serde(default)]
    pub provenance_hint: String,
}

impl Default for ExtractedClaim {
    fn default() -> Self {
        Self {
            fact: String::new(),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Workspace,
            certainty: Certainty::Inferred,
            confidence: 0.0,
            should_persist: false,
            // Default 到 AssistantSummary 能保证不会被写成 Active；
            // 只有真正通过 authority=UserExplicitStatement /
            // AssistantAcknowledged 的声明才会进入 Active。这是 P0-7
            // 的最后一道安全闸。
            authority: EvidenceAuthority::AssistantSummary,
            provenance_hint: String::new(),
        }
    }
}

/// The full extraction output for one turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub claims: Vec<ExtractedClaim>,
    pub source: SourceRef,
    /// (P0-5/6/7) 本次抽取整体的最弱/最强来源边界标签。
    /// 下游 propose_and_commit 用它做最后一次兜底：例如 tool-only turn
    /// 中不会出现 user 级声明。
    #[serde(default)]
    pub strongest_authority_in_context: EvidenceAuthority,
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

    /// (P0-2) Which rule-tier promotion policy to use at the
    /// `propose_and_commit` write gate. Defaults to `AllowCandidate`
    /// (P0 fail-closed). CompositeExtractor/CLI override this via
    /// `--memory-rule-mode`.
    fn rule_mode(&self) -> MemoryRuleMode {
        MemoryRuleMode::default()
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

SOURCE ATTRIBUTION RULES (source_hint field, REQUIRED for every claim):
- "user_explicit":
  - The fact was FIRST STATED by the user verbatim in "User Input" — even if the assistant subsequently repeats/acknowledges it.
  - Examples: user says "记住我的名字叫 ikkk" → claim about user's name uses source_hint="user_explicit".
  - Examples: user says "my name is ikkk" → claim uses source_hint="user_explicit".
  - Examples: user says "I prefer dark mode" → claim uses source_hint="user_explicit".
- "assistant_acknowledged":
  - User never explicitly said the fact in this turn, but the assistant says "好的 / 记住了 / noted / I'll remember that" in direct response to a user's explicit request. The original fact is still rooted in user intent; assistant merely acknowledged it.
  - NOTE: if the user ALSO explicitly stated the same fact, prefer "user_explicit" over "assistant_acknowledged".
- "assistant_summary":
  - The assistant summarised, paraphrased or re-stated a prior idea, but the user did NOT utter that exact phrasing or statement in "User Input" of THIS turn.
  - The fact appears only in assistant output sections, never in user input.
- "assistant_inference":
  - The assistant guessed, inferred or speculated ("I think you probably like...", "you seem to want..."). The user never confirmed.
- "tool_observation":
  - Fact taken directly from a tool result / file content. NOT a user statement or assistant belief.

Respond with a JSON object of this exact shape:
{"claims": [{"fact": "...", "kind": "preference|fact|decision|constraint|solution", "scope": "global|workspace", "certainty": "explicit|inferred|hypothesis", "confidence": 0.9, "should_persist": true, "source_hint": "user_explicit|assistant_acknowledged|assistant_summary|assistant_inference|tool_observation"}]}

Few-shot examples:
User Input: 记住我的名字叫 ikkk
Assistant Output: ["好的，以后我会叫你 ikkk。"]
→ {"claims": [{"fact":"The user's name is ikkk.","kind":"preference","scope":"global","certainty":"explicit","confidence":0.95,"should_persist":true,"source_hint":"user_explicit"}]}

User Input: Please remember my name is ikkk. I prefer dark mode.
Assistant Output: ["Noted! I'll call you ikkk and use dark mode for you."]
→ {"claims": [{"fact":"The user's name is ikkk.","kind":"preference","scope":"global","certainty":"explicit","confidence":0.95,"should_persist":true,"source_hint":"user_explicit"},{"fact":"The user prefers dark mode.","kind":"preference","scope":"global","certainty":"explicit","confidence":0.95,"should_persist":true,"source_hint":"user_explicit"}]}
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
                // P0-5/P0-6/P0-7: rule matched only user_input. This is
                // UserExplicitStatement — the only authority the
                // downstream gate will allow into Active memory via the
                // rule tier. Without this field the default
                // AssistantSummary authority would clamp everything to
                // Candidate status, which would break the identity
                // preference end-to-end test.
                authority: EvidenceAuthority::UserExplicitStatement,
                provenance_hint: "rule_keyword_user_voice_only:name_prefix".into(),
            });
        } else if input.contains("记住我喜欢") || input.contains("i prefer") {
            claims.push(ExtractedClaim {
                fact: format!("用户偏好（来自输入: {}）", truncate(&ctx.user_input, 100)),
                kind: MemoryKind::Preference,
                scope: MemoryScope::Global,
                certainty: Certainty::Explicit,
                confidence: 0.9,
                should_persist: true,
                authority: EvidenceAuthority::UserExplicitStatement,
                provenance_hint: "rule_keyword_user_voice_only:preference_keyword".into(),
            });
        }
        // Default: no claims for ordinary questions.
        let strongest_authority_in_context = if claims.is_empty() {
            // No user-explicit claim was produced. Fall back to the
            // context's configured value (or AssistantSummary if the
            // caller didn't set anything stronger) so downstream audit
            // counters can distinguish "tool-only turn" from user input.
            if matches!(
                ctx.strongest_user_authority,
                EvidenceAuthority::UserExplicitStatement | EvidenceAuthority::AssistantAcknowledged
            ) {
                ctx.strongest_user_authority
            } else {
                EvidenceAuthority::AssistantSummary
            }
        } else {
            // At least one claim derived from user_input; propagate to
            // the top-level result so the supervisor can log it.
            EvidenceAuthority::UserExplicitStatement
        };
        Ok(ExtractionResult {
            claims,
            source: ctx.source.clone(),
            strongest_authority_in_context,
        })
    }
}

// ───────────────────────── Write gate (P0-1/2/3/4) ────────────────

/// How aggressive the supervisor should be when promoting rule-tier
/// claims to memory. The CLI exposes this as `--memory-rule-mode`.
///
/// Default is `AllowCandidate`: when the LLM tier is missing or errors,
/// rules can still **candidate** memory (never Active by default unless
/// the rule explicitly tagged the claim as user_explicit_statement AND
/// `force_active_for_user_explicit` is enabled via `MemoryWriteGate`
/// options). This matches the P0 contract: "LLM 失败不要自动回退成规则
/// 抽取的 Active 长期记忆"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryRuleMode {
    /// No rule-tier claims ever write a memory unit. Claims are logged,
    /// but proposals are skipped. Strongest contract — no risk of
    /// extracting a false user preference from regex keyword noise.
    Disabled,
    /// Rule claims default to status=Candidate, except claims tagged
    /// `EvidenceAuthority::UserExplicitStatement` which are allowed to
    /// pass through to Active via a `force_active_for_user_explicit`
    /// toggle on the gate (on by default so "remember my name" still
    /// works end-to-end even without an LLM configured).
    AllowCandidate,
    /// Rule claims follow the same authority rules as LLM claims:
    /// `may_become_active_memory()` → Active, else Candidate.
    ///
    /// This is the most permissive mode and should only be used after
    /// the rule-templates have been manually audited for a specific
    /// user profile.
    AllowActive,
}

impl Default for MemoryRuleMode {
    fn default() -> Self {
        Self::AllowCandidate
    }
}

impl MemoryRuleMode {
    pub fn from_opt(s: Option<&str>) -> Self {
        match s {
            Some("disabled") | Some("silent") => Self::Disabled,
            Some("allow_candidate") | None => Self::AllowCandidate,
            Some("allow_active") => Self::AllowActive,
            Some(_other) => Self::AllowCandidate,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::AllowCandidate => "allow_candidate",
            Self::AllowActive => "allow_active",
        }
    }
}

/// Final decision produced by `gate_extraction_output`. Consumed directly
/// by `propose_and_commit` instead of hard-coding the rule→Active /
/// rule→Candidate distinction inside each proposal branch.
#[derive(Debug, Clone)]
pub enum MemoryWriteGateDecision {
    /// Skip claim entirely — write nothing. The supervisor caller may
    /// optionally increment a "rule_silent_skip" audit counter for UX.
    Skip { reason: String },
    /// Create proposal + create memory unit with status='candidate'.
    /// Candidates are invisible to retrieval (FTS gate filters active
    /// only) so they can be surfaced for a future manual review or a
    /// governance LLM promotion pass — but cannot cause hallucinations
    /// on the very next turn.
    PromoteCandidate { reason: String },
    /// Create proposal + commit into status='active'.  This is the
    /// strongest form and is only granted when both the claim's
    /// authority AND the rule-mode allow it.
    PromoteActive { reason: String },
}

/// Apply P0 source-boundary gating and rule-tier contract to a claim.
///
/// Contract:
/// 1. Tool-derived claims NEVER promote to memory regardless of tier.
///    (P0-6 hard constraint).
/// 2. Claims from AssistantInference are clamped to Candidate at best
///    regardless of tier — inference is not a user statement (P0-7).
/// 3. For rule-tier output:
///    - `Disabled` → all claims skip.
///    - `AllowCandidate` (default) → Candidate for user authority,
///      except `force_user_explicit_active = true` lets user-explicit
///      statements into Active so identity preferences keep working.
///    - `AllowActive` → follow authority normally.
/// 4. For llm-tier output: authority alone decides (Active only for
///    UserExplicitStatement / AssistantAcknowledged).
pub fn gate_extraction_output(
    claim: &ExtractedClaim,
    tier_label_hint: &str,
    rule_mode: MemoryRuleMode,
    force_user_explicit_active: bool,
) -> MemoryWriteGateDecision {
    use EvidenceAuthority::*;
    // P0-6 hard line: tool-derived claims never become memory units.
    if claim.authority.is_tool_derived() {
        return MemoryWriteGateDecision::Skip {
            reason: "claim authority=ToolObservation; tool results must stay in evidence".into(),
        };
    }
    // should_persist guard — extractor can borderline claims off without
    // needing a second downstream check.
    if !claim.should_persist {
        return MemoryWriteGateDecision::Skip {
            reason: format!(
                "claim.should_persist=false (provenance_hint={})",
                claim.provenance_hint
            ),
        };
    }
    // Confidence safety valve — low-confidence claims never enter active.
    if claim.confidence < 0.2 {
        return MemoryWriteGateDecision::Skip {
            reason: format!(
                "claim confidence {:.2} below 0.2 soft floor",
                claim.confidence
            ),
        };
    }

    let from_rule_tier = tier_label_hint.starts_with("mock:")
        || tier_label_hint.starts_with("rule")
        || tier_label_hint.starts_with("composite:rule-only")
        || tier_label_hint.contains("rule-only");

    if from_rule_tier {
        match rule_mode {
            MemoryRuleMode::Disabled => MemoryWriteGateDecision::Skip {
                reason: "rule tier disabled via MemoryRuleMode::Disabled".into(),
            },
            MemoryRuleMode::AllowCandidate => {
                // P0-3 default contract: rule tier cannot unconditionally
                // write Active. Only UserExplicitStatement claims (regex
                // matched against user_input direct voice) are allowed
                // to Active for backwards compatibility.
                if matches!(claim.authority, UserExplicitStatement) && force_user_explicit_active {
                    MemoryWriteGateDecision::PromoteActive {
                        reason: "AllowCandidate + authority=UserExplicitStatement + force_user_explicit_active: identity/preference regex claims must stay end-to-end capable".into(),
                    }
                } else {
                    MemoryWriteGateDecision::PromoteCandidate {
                        reason: format!(
                            "AllowCandidate rule tier (authority={}, prov={})",
                            claim.authority.as_str(), claim.provenance_hint
                        ),
                    }
                }
            }
            MemoryRuleMode::AllowActive => {
                if claim.authority.may_become_active_memory() {
                    MemoryWriteGateDecision::PromoteActive {
                        reason: "AllowActive rule tier + eligible authority".into(),
                    }
                } else {
                    MemoryWriteGateDecision::PromoteCandidate {
                        reason: "AllowActive rule tier but claim authority cannot become active".into(),
                    }
                }
            }
        }
    } else {
        // LLM / composite-with-LLM tier: authority alone decides.
        if claim.authority.may_become_active_memory() {
            MemoryWriteGateDecision::PromoteActive {
                reason: format!(
                    "LLM tier + authority={} may become active",
                    claim.authority.as_str()
                ),
            }
        } else {
            MemoryWriteGateDecision::PromoteCandidate {
                reason: format!(
                    "LLM tier: authority={} clamped to candidate",
                    claim.authority.as_str()
                ),
            }
        }
    }
}

/// Simple heuristic: matches "叫我 X" / "我叫 X" / "叫我X" / "call me X"
/// and returns the captured name. Returns None for non-matching input.
///
/// Enhancement for "记住我的名字叫 ikkk" flow:
/// - Step A: iteratively strip leading polite/vocative phrases
///   (你 / 请 / 麻烦你 / 帮我 / 你帮我 / 麻烦帮我). This is repeated
///   until no more matches, so "你请记住我的名字叫 ikkk" works too.
/// - Step B: expanded prefix table. Chinese adds "我的名字叫",
///   "名字叫", "名字是". English adds "remember my name is".
pub fn extract_name(input: &str) -> Option<String> {
    let mut working = input.trim().to_string();
    // ── Step A: iterative polite/vocative stripping ──
    const POLITE_PREFIXES: &[&str] = &[
        "麻烦你帮我",
        "麻烦帮我",
        "你帮我",
        "麻烦你",
        "麻烦请",
        "帮我",
        "麻烦",
        "请",
        "你",
    ];
    loop {
        let before = working.clone();
        for p in POLITE_PREFIXES {
            if let Some(stripped) = working.strip_prefix(p) {
                // Strip only if next char is whitespace or a CJK character.
                // "please remember ..." starts with different char so this
                // branch naturally only triggers on CJK input.
                let rest = stripped.trim_start();
                working = rest.to_string();
                break;
            }
        }
        if working == before {
            break;
        }
    }
    let trimmed = working.trim();
    // ── Step B: try expanded Chinese prefixes ──
    // Keep the order specific → general so "我的名字是" is tried
    // before "名字是" (which would also match, but the longer one first
    // avoids accidentally stripping partial text).
    for prefix in [
        // Longest-match-first ordering so compound prefixes win over
        // shorter subsets. E.g. "请记住我的名字叫" must beat "记住我的名字叫"
        // which must beat "我的名字叫" — otherwise we extract the wrong
        // segment when polite prefixes were already stripped.
        //
        // RC2 expanded identity patterns (both polite + direct forms)
        "请记住我的名字叫",
        "记住我的名字叫",
        "请记住我叫",
        "记住我叫",
        "我的名字叫",
        "我的名字是",
        "名字叫",
        "名字是",
        "以后叫我",
        "之后叫我",
        "叫我",
        "我叫",
    ] {
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
    for prefix in ["please remember my name is ", "remember my name is ", "call me ", "my name is "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let name = rest.trim_end_matches(|c: char| c.is_ascii_punctuation());
            if !name.is_empty() {
                return Some(name.to_string());
            }
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
                authority: EvidenceAuthority::ToolObservation,
            }],
            tool_results: vec![ToolResultSummary {
                name: "read_file".into(),
                is_error: false,
                content: "file contents here".into(),
                authority: EvidenceAuthority::ToolObservation,
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

    // ─── extract_name: expanded pattern coverage (RC2a) ───
    #[test]
    fn extract_name_new_patterns_rc2a() {
        // Cases mandated by the RC2a spec:
        let cases = [
            // direct new patterns
            ("我的名字叫 ikkk", Some("ikkk")),
            ("我的名字叫Alice。", Some("Alice")),
            ("名字叫 Bob", Some("Bob")),
            ("名字是Carol", Some("Carol")),
            // polite prefix stripping
            ("你记住我的名字叫 ikkk", Some("ikkk")),
            ("请记住我的名字叫 ikkk", Some("ikkk")),
            ("麻烦你记住我的名字叫 ikkk", Some("ikkk")),
            ("帮我记住我的名字叫 Daniel", Some("Daniel")),
            ("你帮我记住我的名字叫 Eve", Some("Eve")),
            // English expanded
            ("remember my name is ikkk", Some("ikkk")),
            ("Please remember my name is Fiona.", Some("fiona")),
            // Legacy patterns should still work
            ("我叫 ikkk", Some("ikkk")),
            ("叫我Gary", Some("Gary")),
            ("call me Helen", Some("helen")),
            ("my name is Ian", Some("ian")),
            // Negative case: ordinary sentence without identity markers
            ("帮我写个 hello world", None),
            ("the file has 42 lines", None),
        ];
        for (input, expected) in cases {
            let got = extract_name(input);
            assert_eq!(
                got.as_deref(),
                expected,
                "extract_name({input:?}) = {got:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn extract_name_iterative_polite_strip_does_not_consume_name() {
        // Repeated polite prefixes should not eat the name itself;
        // and a bare identity sentence with no polite prefix works.
        assert_eq!(extract_name("请请请记住我的名字叫 ikkk").as_deref(), Some("ikkk"));
        assert_eq!(extract_name("你麻烦帮我请记住我的名字叫 Zoe").as_deref(), Some("Zoe"));
        // If the input is ONLY a name with no prefix, extract_name must NOT
        // try to parse a prefix out of it — returns None.
        assert_eq!(extract_name("ikkk"), None);
    }
}
