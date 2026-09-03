//! Production-grade [`grodex_memory::EvidenceExtractor`] for the Agent Loop.
//!
//! # Two-tier architecture
//!
//! To keep extraction working even when there is no LLM budget, no
//! reachable provider, or when the JSON output malforms, extraction runs
//! in a **two-tier** stack:
//!
//! 1. [`SamplingBackedExtractor`] — the *real* extractor. It calls the
//!    same `SamplingActor` used by interactive turns, sending the
//!    extraction system prompt + a JSON-schema response format so the
//!    model returns `{"claims":[...]}` deterministically. Requires a
//!    live `SamplingActor` + `ModelBinding` to be injected at startup.
//!
//! 2. [`MockEvidenceExtractor`] (re-exported from grodex-memory) — the
//!    *rule-based fallback*. It recognises regex-parseable preferences
//!    ("call me X", "记住我叫 X", …) without LLM calls.
//!
//! [`CompositeExtractor`] wraps both and implements the trait:
//!   * If a LLM extractor is configured AND returns `Ok`, use it
//!     (preferred path).
//!   * If anything fails (provider error, JSON parse, schema) OR no
//!     LLM extractor is configured, transparently fall back to the
//!     rule-based tier.
//!
//! This mirrors W3's "raw rollout extractor" fallback and keeps memory
//! ingestion fail-open, with two guarantees:
//!   * Global-preference identity claims ("my name is ikkk") always have
//!     a path to Active memory (through the regex tier), even under
//!     total provider outage.
//!   * Higher-signal facts (architectural decisions, project rules, …)
//!     benefit from the LLM path whenever the provider is healthy.

use std::sync::Arc;

use grodex_core::id::{SessionId, StepId, TurnId};
use grodex_memory::{
    EvidenceExtractor, ExtractionContext, ExtractionError, ExtractionResult,
    MockEvidenceExtractor, SourceRef, EXTRACTOR_SYSTEM_PROMPT, render_context_for_llm,
};
use grodex_provider::binding::ModelBinding;
use grodex_provider::canonical_request::{
    CanonicalModelRequest, InstructionBlock, InstructionRole, ResponseFormat, ToolChoice,
};
use grodex_sampler::{SamplingActor, SamplingError};
use serde::Deserialize;

/// The LLM-backed extractor — calls the shared SamplingActor with the
/// extraction system prompt + JSON schema response format.
///
/// Callers should only construct this when the user actually has a
/// reachable provider configured (e.g. only after `SamplingActor::new`
/// succeeded AND the route is non-empty).
pub struct SamplingBackedExtractor {
    sampler: Arc<SamplingActor>,
    binding: ModelBinding,
}

impl SamplingBackedExtractor {
    pub fn new(sampler: Arc<SamplingActor>, binding: ModelBinding) -> Self {
        Self { sampler, binding }
    }

    async fn extract_inner(&self, ctx: &ExtractionContext) -> Result<ExtractionResult, ExtractionError> {
        // ── Build request ──────────────────────────────────────────
        // We DON'T re-use the conversation context here: extraction only
        // sees the turn-scoped ExtractionContext (already filtered to
        // exclude sub-agent noise by the caller). This avoids pulling
        // unrelated 20k-token history into an already-expensive call.
        let instructions = vec![
            InstructionBlock {
                role: InstructionRole::System,
                content: EXTRACTOR_SYSTEM_PROMPT.to_string(),
                priority: 0,
            },
            InstructionBlock {
                role: InstructionRole::Developer,
                content: render_context_for_llm(ctx),
                priority: 1,
            },
        ];

        // Constrain output to valid `ExtractionResult` JSON shape.
        // If the provider doesn't support json_schema we rely on the
        // prompt + try_from error → fallback to rule tier anyway.
        let response_format = Some(ResponseFormat {
            json_schema: Some(extraction_result_json_schema()),
        });

        let binding_id = self.binding.binding_id;
        let request = CanonicalModelRequest {
            request_id: format!("mem-ext-{}", ctx.source.turn_id),
            session_id: SessionId::default(),
            turn_id: TurnId::default(),
            step_id: StepId::default(),
            model_binding_id: binding_id,
            prompt_snapshot_hash: None,
            instructions,
            context_items: Vec::new(),
            tool_specs: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tool_calls: false,
            reasoning_request: None,
            response_format,
            max_output_tokens: Some(1600), // extraction is short-form
            provider_state_in: None,
        };

        // ── Run sampling (non-streaming) ───────────────────────────
        let outcome = self.sampler.sample(&self.binding, &request).await;

        // ── Extract assistant text from outcome ────────────────────
        // Preferred path: use CanonicalModelResponse.assistant_text()
        // (aggregates all assistant items). Fallback: walk events and
        // concatenate TextDelta fragments. If neither produces text,
        // inspect outcome.error or return a generic "no text produced"
        // error, which triggers the rule-based fallback through
        // CompositeExtractor.
        let response_text = {
            let mut buf = String::with_capacity(1024);
            if let Some(ref resp) = outcome.response {
                if let Some(t) = resp.assistant_text() {
                    buf.push_str(t);
                }
            }
            if buf.trim().is_empty() {
                use grodex_provider::CanonicalModelEvent;
                for ev in &outcome.events {
                    if let CanonicalModelEvent::TextDelta { text, .. } = ev {
                        buf.push_str(text);
                    }
                }
            }
            if buf.trim().is_empty() {
                let msg = outcome
                    .error
                    .as_ref()
                    .map(|e: &SamplingError| format!("sampling error: {e}"))
                    .unwrap_or_else(|| "no text produced by model".into());
                return Err(ExtractionError::Provider(msg));
            }
            buf
        };

        // ── Parse JSON into ExtractionResult ───────────────────────
        // Try strict parse first; if that fails try to recover by
        // stripping a ```json … ``` code fence (common failure mode).
        let parsed = parse_extraction_payload(&response_text)?;
        Ok(parsed.with_source(ctx.source.clone()))
    }
}

#[async_trait::async_trait]
impl EvidenceExtractor for SamplingBackedExtractor {
    fn tier_label(&self) -> &'static str {
        "sampling:llm"
    }
    async fn extract(&self, ctx: &ExtractionContext) -> Result<ExtractionResult, ExtractionError> {
        self.extract_inner(ctx).await
    }
}

/// Two-tier composite: LLM tier first, rule tier on any failure.
///
/// Cloned cheaply (Arc shared contents).
#[derive(Clone)]
pub struct CompositeExtractor {
    /// If `Some`, we attempt the LLM tier before the rule tier.
    /// If `None`, only the rule tier runs.
    llm_tier: Option<Arc<dyn EvidenceExtractor + Send + Sync>>,
    rule_tier: Arc<MockEvidenceExtractor>,
}

impl CompositeExtractor {
    pub fn new(llm_tier: Option<Arc<dyn EvidenceExtractor + Send + Sync>>) -> Self {
        Self {
            llm_tier,
            rule_tier: Arc::new(MockEvidenceExtractor::default()),
        }
    }

    /// No LLM tier, rule tier only. Used by tests and when the user
    /// hasn't configured a model for extraction (fail-open).
    pub fn with_mock() -> Self {
        Self::default()
    }
}

impl Default for CompositeExtractor {
    fn default() -> Self {
        Self {
            llm_tier: None,
            rule_tier: Arc::new(MockEvidenceExtractor::default()),
        }
    }
}

#[async_trait::async_trait]
impl EvidenceExtractor for CompositeExtractor {
    fn tier_label(&self) -> &'static str {
        match self.llm_tier {
            Some(_) => "composite:llm+rule",
            None => "composite:rule-only",
        }
    }
    async fn extract(&self, ctx: &ExtractionContext) -> Result<ExtractionResult, ExtractionError> {
        // Tier 1: LLM (if configured).
        if let Some(ref llm) = self.llm_tier {
            match llm.extract(ctx).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    // Fail-open: do not propagate LLM errors. Fall through
                    // to rule tier; the error is only surfaced in logs.
                    tracing::debug!(
                        error = %e,
                        turn_id = %ctx.source.turn_id,
                        "memory extractor LLM tier failed, falling back to rule tier"
                    );
                }
            }
        }
        // Tier 2: rule-based fallback (deterministic, no network).
        self.rule_tier.extract(ctx).await
    }
}

// ═══════════════════════════════════════════════════════════════
// QueryUnderstanding implementation (W4-3).
// ═══════════════════════════════════════════════════════════════

/// LLM-backed `QueryUnderstandingModel`. Uses the same `SamplingActor`
/// JSON extraction pipeline as `SamplingBackedExtractor` to minimize
/// new surface area. Failures do not halt retrieval — `retrieve_enhanced`
/// treats a provider/parse error as "no QU attached, run raw retrieve".
pub struct SamplingBackedQueryUnderstanding {
    sampler: Arc<SamplingActor>,
    binding: ModelBinding,
    /// Optional second-level override route (e.g. "lighter" model for QU).
    /// When `None`, falls back to whatever `SamplingActor` resolves via
    /// the `ModelBinding`.
    sampling_route_override: Option<String>,
}

impl SamplingBackedQueryUnderstanding {
    pub fn new(
        sampler: Arc<SamplingActor>,
        binding: ModelBinding,
        sampling_route_override: Option<String>,
    ) -> Self {
        Self { sampler, binding, sampling_route_override }
    }

    /// Parse a QU JSON response with the same 3-tier recovery as the
    /// extractor payload parser (strict → fenced → substring search).
    fn parse_payload(
        text: &str,
    ) -> Result<grodex_memory::QueryUnderstanding, grodex_memory::QueryUnderstandingError> {
        use grodex_memory::{QueryIntent, QueryUnderstanding};
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Wire {
            intent: String,
            #[serde(default)]
            rewritten_query: Option<String>,
        }
        fn try_str(s: &str) -> Option<Wire> {
            serde_json::from_str::<Wire>(s).ok()
        }
        let parsed = try_str(text)
            .or_else(|| try_str(&strip_code_fence(text)))
            .or_else(|| {
                let (lo, hi) = find_outer_json_braces(text)?;
                try_str(&text[lo..hi])
            })
            .ok_or_else(|| {
                grodex_memory::QueryUnderstandingError::Parse(format!(
                    "could not parse QU response as JSON: {}",
                    truncate_for_err(text)
                ))
            })?;
        let intent = match parsed.intent.as_str() {
            "user_identity" => QueryIntent::UserIdentity,
            "user_preference" => QueryIntent::UserPreference,
            "project_decision" => QueryIntent::ProjectDecision,
            "project_fact" => QueryIntent::ProjectFact,
            "project_constraint" => QueryIntent::ProjectConstraint,
            _ => QueryIntent::General,
        };
        Ok(QueryUnderstanding {
            intent,
            rewritten_query: parsed.rewritten_query.filter(|s| !s.trim().is_empty()),
        })
    }
}

#[async_trait::async_trait]
impl grodex_memory::QueryUnderstandingModel for SamplingBackedQueryUnderstanding {
    async fn understand(
        &self,
        query: &str,
    ) -> Result<grodex_memory::QueryUnderstanding, grodex_memory::QueryUnderstandingError> {
        use grodex_memory::{QUERY_UNDERSTANDING_PROMPT, QueryUnderstandingError};
        use serde_json::json;

        let schema = json!({
            "name": "QueryUnderstanding",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "intent": {
                        "type": "string",
                        "enum": ["user_identity","user_preference","project_decision","project_fact","project_constraint","general"]
                    },
                    "rewritten_query": {"type": ["string","null"]}
                },
                "required": ["intent","rewritten_query"]
            }
        });
        let user_block = format!(
            "{}\n\nRespond STRICTLY as JSON matching the schema. Do not add prose, code fences, or commentary.\n\nUser query: {}",
            QUERY_UNDERSTANDING_PROMPT, query
        );
        let instructions = vec![
            InstructionBlock {
                role: InstructionRole::System,
                content: "You are a query understanding engine. Respond ONLY with the exact JSON schema requested — no markdown, no prose, no commentary.".into(),
                priority: 0,
            },
            InstructionBlock {
                role: InstructionRole::Developer,
                content: user_block,
                priority: 1,
            },
        ];
        let response_format = Some(ResponseFormat { json_schema: Some(schema) });
        let binding_id = self.binding.binding_id;
        // NOTE: QU is stateless and short-form. It does not need to
        // participate in the current session/turn rollout; binding IDs
        // are required by the type but the sampler handles routing.
        let request = CanonicalModelRequest {
            // Short, non-colliding request id: QU is stateless so the
            // request_id is only used for logging/correlation (not
            // durable rollout).
            request_id: format!(
                "mem-qu-{}",
                grodex_core::id::SessionId::default()
            ),
            session_id: SessionId::default(),
            turn_id: TurnId::default(),
            step_id: StepId::default(),
            model_binding_id: binding_id,
            prompt_snapshot_hash: self.sampling_route_override.clone(),
            instructions,
            context_items: Vec::new(),
            tool_specs: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tool_calls: false,
            reasoning_request: None,
            response_format,
            max_output_tokens: Some(300),
            provider_state_in: None,
        };
        let outcome = self.sampler.sample(&self.binding, &request).await;
        let response_text = {
            let mut buf = String::with_capacity(512);
            if let Some(ref resp) = outcome.response {
                if let Some(t) = resp.assistant_text() {
                    buf.push_str(t);
                }
            }
            if buf.trim().is_empty() {
                use grodex_provider::CanonicalModelEvent;
                for ev in &outcome.events {
                    if let CanonicalModelEvent::TextDelta { text, .. } = ev {
                        buf.push_str(text);
                    }
                }
            }
            if buf.trim().is_empty() {
                let msg = outcome
                    .error
                    .as_ref()
                    .map(|e| format!("sampling error: {e}"))
                    .unwrap_or_else(|| "no text produced by QU model".into());
                return Err(QueryUnderstandingError::Provider(msg));
            }
            buf
        };
        Self::parse_payload(&response_text)
    }
}

// ═══════════════════════════════════════════════════════════════
// ConflictJudge implementation (W4-4).
// ═══════════════════════════════════════════════════════════════

/// LLM-backed `ConflictJudge`. Mirrors the QU/Extractor pipeline:
/// shared `SamplingActor`, JSON-schema response, 3-tier JSON recovery.
/// Failures surface as `Err(ConflictJudgeError)` which the caller's
/// fail-open loop skips without resolving the conflict row.
pub struct SamplingBackedConflictJudge {
    sampler: Arc<SamplingActor>,
    binding: ModelBinding,
    sampling_route_override: Option<String>,
}

impl SamplingBackedConflictJudge {
    pub fn new(
        sampler: Arc<SamplingActor>,
        binding: ModelBinding,
        sampling_route_override: Option<String>,
    ) -> Self {
        Self { sampler, binding, sampling_route_override }
    }

    fn parse_payload(
        text: &str,
    ) -> Result<grodex_memory::ConflictJudgeResult, grodex_memory::ConflictJudgeError> {
        use grodex_memory::{ConflictJudgeResult, ConflictRelation};
        use serde::Deserialize;
        #[derive(Deserialize)]
        struct Wire {
            relation: String,
            #[serde(default)]
            confidence: Option<f64>,
            #[serde(default)]
            reason: Option<String>,
        }
        fn try_str(s: &str) -> Option<Wire> {
            serde_json::from_str::<Wire>(s).ok()
        }
        let parsed = try_str(text)
            .or_else(|| try_str(&strip_code_fence(text)))
            .or_else(|| {
                let (lo, hi) = find_outer_json_braces(text)?;
                try_str(&text[lo..hi])
            })
            .ok_or_else(|| {
                grodex_memory::ConflictJudgeError::Parse(format!(
                    "could not parse conflict-judge response as JSON: {}",
                    truncate_for_err(text)
                ))
            })?;
        let relation = match parsed.relation.as_str() {
            "duplicate" => ConflictRelation::Duplicate,
            "equivalent" => ConflictRelation::Equivalent,
            "supersedes" => ConflictRelation::Supersedes,
            "conflicts" => ConflictRelation::Conflicts,
            _ => ConflictRelation::Independent,
        };
        Ok(ConflictJudgeResult {
            relation,
            confidence: parsed.confidence.unwrap_or(0.6).clamp(0.0, 1.0),
            reason: parsed.reason.unwrap_or_default(),
        })
    }
}

#[async_trait::async_trait]
impl grodex_memory::ConflictJudge for SamplingBackedConflictJudge {
    async fn judge(
        &self,
        input: &grodex_memory::ConflictJudgeInput,
    ) -> Result<grodex_memory::ConflictJudgeResult, grodex_memory::ConflictJudgeError> {
        use grodex_memory::{CONFLICT_JUDGE_PROMPT, ConflictJudgeError};
        use serde_json::json;

        let schema = json!({
            "name": "ConflictJudgeVerdict",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "relation": {"type":"string","enum":["duplicate","equivalent","supersedes","conflicts","independent"]},
                    "confidence": {"type":"number","minimum":0.0,"maximum":1.0},
                    "reason": {"type":"string"}
                },
                "required": ["relation","confidence","reason"]
            }
        });
        let user_block = format!(
            "{CONFLICT_JUDGE_PROMPT}\n\nRespond STRICTLY as JSON matching the schema. Do not add prose, code fences, or commentary.\n\nLeft (existing):\n  id: {}\n  kind: {:?}\n  scope: {:?}\n  content: {}\n\nRight (candidate/newer):\n  id: {}\n  kind: {:?}\n  scope: {:?}\n  content: {}",
            input.left.id, input.left.kind, input.left.scope, input.left.content,
            input.right.id, input.right.kind, input.right.scope, input.right.content,
        );
        let instructions = vec![
            InstructionBlock {
                role: InstructionRole::System,
                content: "You are a memory conflict judge. Respond ONLY with the exact JSON schema requested — no markdown, no prose, no commentary.".into(),
                priority: 0,
            },
            InstructionBlock {
                role: InstructionRole::Developer,
                content: user_block,
                priority: 1,
            },
        ];
        let response_format = Some(ResponseFormat { json_schema: Some(schema) });
        let binding_id = self.binding.binding_id;
        let request = CanonicalModelRequest {
            request_id: format!(
                "mem-cj-{}-{}",
                input.left.id.chars().take(8).collect::<String>(),
                input.right.id.chars().take(8).collect::<String>()
            ),
            session_id: SessionId::default(),
            turn_id: TurnId::default(),
            step_id: StepId::default(),
            model_binding_id: binding_id,
            prompt_snapshot_hash: self.sampling_route_override.clone(),
            instructions,
            context_items: Vec::new(),
            tool_specs: Vec::new(),
            tool_choice: ToolChoice::None,
            parallel_tool_calls: false,
            reasoning_request: None,
            response_format,
            max_output_tokens: Some(500),
            provider_state_in: None,
        };
        let outcome = self.sampler.sample(&self.binding, &request).await;
        let response_text = {
            let mut buf = String::with_capacity(768);
            if let Some(ref resp) = outcome.response {
                if let Some(t) = resp.assistant_text() {
                    buf.push_str(t);
                }
            }
            if buf.trim().is_empty() {
                use grodex_provider::CanonicalModelEvent;
                for ev in &outcome.events {
                    if let CanonicalModelEvent::TextDelta { text, .. } = ev {
                        buf.push_str(text);
                    }
                }
            }
            if buf.trim().is_empty() {
                let msg = outcome
                    .error
                    .as_ref()
                    .map(|e| format!("sampling error: {e}"))
                    .unwrap_or_else(|| "no text produced by conflict-judge model".into());
                return Err(ConflictJudgeError::Provider(msg));
            }
            buf
        };
        Self::parse_payload(&response_text)
    }
}

// ───────────────────────── JSON helpers ──────────────────────────

/// Response-shape envelope used by `parse_extraction_payload`. The
/// `ExtractionResult` struct (in grodex-memory) lives in a
/// no-provider crate and therefore can't implement Deserialize bounds
/// that depend on provider specifics. We keep a local mirror here.
#[derive(Debug, Deserialize)]
struct ClaimEnvelope {
    claims: Vec<ClaimRaw>,
}

#[derive(Debug, Deserialize)]
struct ClaimRaw {
    fact: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    certainty: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default = "default_true")]
    should_persist: bool,
}

fn default_true() -> bool {
    true
}

fn parse_extraction_payload(text: &str) -> Result<ExtractionResultViaMirror, ExtractionError> {
    // 1) Try direct parse.
    if let Ok(v) = serde_json::from_str::<ClaimEnvelope>(text) {
        return Ok(ExtractionResultViaMirror(v));
    }
    // 2) Try stripping code fence ```json ... ```.
    let stripped = strip_code_fence(text);
    if let Ok(v) = serde_json::from_str::<ClaimEnvelope>(&stripped) {
        return Ok(ExtractionResultViaMirror(v));
    }
    // 3) Try to find the largest JSON object substring (the model may
    //    have appended natural-language prose).
    if let Some((lo, hi)) = find_outer_json_braces(text) {
        if let Ok(v) = serde_json::from_str::<ClaimEnvelope>(&text[lo..hi]) {
            return Ok(ExtractionResultViaMirror(v));
        }
    }
    Err(ExtractionError::Parse(format!(
        "could not parse extraction response as JSON: {}",
        truncate_for_err(text)
    )))
}

/// Local adapter mirror — converts the loose JSON-wire types into the
/// canonical `ExtractedClaim` enum values with defaults.
struct ExtractionResultViaMirror(ClaimEnvelope);

impl ExtractionResultViaMirror {
    fn with_source(
        self,
        source: SourceRef,
    ) -> ExtractionResult {
        use grodex_memory::{Certainty, ExtractedClaim, MemoryKind, MemoryScope};
        let claims = self
            .0
            .claims
            .into_iter()
            .map(|c| ExtractedClaim {
                fact: c.fact,
                kind: c
                    .kind
                    .as_deref()
                    .and_then(|s| MemoryKind::from_str(s))
                    .unwrap_or(MemoryKind::Fact),
                scope: c
                    .scope
                    .as_deref()
                    .and_then(|s| MemoryScope::from_str(s))
                    .unwrap_or(MemoryScope::Workspace),
                certainty: c
                    .certainty
                    .as_deref()
                    .and_then(|s| Certainty::from_str(s))
                    .unwrap_or(Certainty::Inferred),
                confidence: c.confidence.unwrap_or(0.7).clamp(0.0, 1.0),
                should_persist: c.should_persist,
            })
            .collect();
        ExtractionResult { claims, source }
    }
}

fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("```json").unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

fn find_outer_json_braces(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let lo = bytes.iter().position(|&b| b == b'{')?;
    let hi = bytes.iter().rposition(|&b| b == b'}')?;
    if hi <= lo {
        return None;
    }
    Some((lo, hi + 1))
}

fn truncate_for_err(s: &str) -> String {
    const MAX: usize = 240;
    let chars: Vec<char> = s.chars().take(MAX).collect();
    let mut out: String = chars.into_iter().collect();
    if s.chars().count() > MAX {
        out.push('…');
    }
    out
}

/// Build the JSON Schema the `response_format` field requests.
/// Mirrors `ClaimEnvelope` + `ClaimRaw` shape.
fn extraction_result_json_schema() -> serde_json::Value {
    use serde_json::json;
    json!({
        "name": "MemoryExtractionResult",
        "strict": true,
        "schema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "claims": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "fact": {"type": "string"},
                            "kind": {"type": "string", "enum": ["preference", "fact", "decision", "constraint", "solution"]},
                            "scope": {"type": "string", "enum": ["global", "workspace"]},
                            "certainty": {"type": "string", "enum": ["explicit", "inferred", "hypothesis"]},
                            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                            "should_persist": {"type": "boolean"}
                        },
                        "required": ["fact"]
                    }
                }
            },
            "required": ["claims"]
        }
    })
}

// ───────────────────────── Helper: assemble SourceRef helpers ─────

#[allow(dead_code)]
fn _typeck_source_ref_default() -> SourceRef {
    // Compile-time proof that SourceRef implements Default (used by
    // callers in the unit tests, plus the turn-completion assembly path
    // which constructs it with explicit fields).
    SourceRef::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_strict_json() {
        let body = r#"{"claims":[{"fact":"用户希望被称呼为 ikkk。","kind":"preference","scope":"global","certainty":"explicit","confidence":0.95,"should_persist":true}]}"#;
        let parsed = parse_extraction_payload(body).unwrap();
        let r = parsed.0;
        assert_eq!(r.claims.len(), 1);
        assert_eq!(r.claims[0].fact, "用户希望被称呼为 ikkk。");
        assert_eq!(r.claims[0].kind.as_deref(), Some("preference"));
        assert_eq!(r.claims[0].scope.as_deref(), Some("global"));
        assert!(r.claims[0].should_persist);
    }

    #[test]
    fn parse_accepts_code_fenced_json() {
        let body = "Sure, here is the result:\n```json\n{\"claims\":[{\"fact\":\"f1\",\"should_persist\":true}]}\n```\nThanks.";
        let parsed = parse_extraction_payload(body).unwrap();
        assert_eq!(parsed.0.claims[0].fact, "f1");
        assert!(parsed.0.claims[0].should_persist);
    }

    #[test]
    fn parse_accepts_defaults_for_partial_fields() {
        // When the model omits optional fields, fallback values must
        // match the documented defaults (scope=workspace, kind=fact,
        // certainty=inferred, confidence=0.7, should_persist=true).
        let body = r#"{"claims":[{"fact":"partial"}]}"#;
        let parsed = parse_extraction_payload(body).unwrap();
        let r = parsed.with_source(SourceRef::default());
        assert_eq!(r.claims[0].kind, grodex_memory::MemoryKind::Fact);
        assert_eq!(r.claims[0].scope, grodex_memory::MemoryScope::Workspace);
        assert_eq!(r.claims[0].certainty, grodex_memory::Certainty::Inferred);
        assert!(
            (r.claims[0].confidence - 0.7).abs() < 1e-9,
            "confidence default should be 0.7, got {}",
            r.claims[0].confidence
        );
        assert!(r.claims[0].should_persist);
    }

    #[test]
    fn parse_rejects_bare_text() {
        assert!(parse_extraction_payload("there is no json here").is_err());
    }

    #[test]
    fn strip_fence_handles_trim_and_suffix() {
        assert_eq!(
            strip_code_fence("  ```json\n{\"a\":1}\n```  "),
            "{\"a\":1}"
        );
    }
}
