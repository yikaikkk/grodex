//! Intent Router — multi-label conservative routing for retrieval.
//!
//! Design 08 §4: The router runs *before* constructing the model context.
//! It is a deterministic rule system, not a model call. The default
//! strategy is "miss cost > empty retrieval cost" — when in doubt, enable.
//!
//! Multi-label: a request can enable Skill, Memory, and Evidence
//! simultaneously (e.g. "redo the last failed release" needs all three).

use serde::{Deserialize, Serialize};

/// The Router's decision for a single user request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterDecision {
    /// Whether to run the Skill retrieval pipeline.
    pub skill_enabled: bool,
    /// Whether to run the Long-term Memory retrieval pipeline.
    pub memory_enabled: bool,
    /// Whether to run the Evidence retrieval pipeline.
    /// Evidence=true implies Memory=true (but not Skill=true).
    pub evidence_enabled: bool,
    /// Whether to include superseded evidence in the search.
    /// Only true for explicit history/evolution queries.
    pub include_superseded: bool,
    /// Structured reason codes explaining each decision.
    pub reason_codes: Vec<String>,
    /// If the router hard-skipped (self-contained request), why.
    pub hard_skip_reason: Option<String>,
}

impl RouterDecision {
    /// Default decision: everything enabled (conservative).
    pub fn all_enabled() -> Self {
        Self {
            skill_enabled: true,
            memory_enabled: true,
            evidence_enabled: false,
            include_superseded: false,
            reason_codes: vec!["default_conservative".to_string()],
            hard_skip_reason: None,
        }
    }
}

/// A fingerprint of the query for diagnostics (not the raw text).
///
/// Also used as a cache key for `MemoryContextSnapshot` (invariant #10),
/// so it derives `Hash` + `Eq` for use in `HashMap`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryFingerprint {
    /// Normalized query (whitespace-folded, trimmed).
    pub normalized: String,
    /// Term count.
    pub term_count: usize,
    /// Whether the query contains history signals.
    pub has_history_signal: bool,
    /// Whether the query contains action/skill signals.
    pub has_action_signal: bool,
}

impl QueryFingerprint {
    /// Create a fingerprint from a raw user query string.
    ///
    /// The normalization is whitespace-fold + trimmed + lowercased,
    /// matching the negative cache's normalization so the same query
    /// produces the same fingerprint across layers.
    pub fn from_query(query: &str) -> Self {
        let normalized: String = query
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        let term_count = normalized.split_whitespace().count();

        let history_signals = [
            "last", "before", "why", "failed", "original", "evidence",
            "evolution", "history", "previous", "prior",
        ];
        let action_signals = [
            "action", "release", "deploy", "test", "build",
            "发布", "部署", "构建", "测试",
        ];

        let has_history_signal = history_signals.iter().any(|s| normalized.contains(s));
        let has_action_signal = action_signals.iter().any(|s| normalized.contains(s));

        Self {
            normalized,
            term_count,
            has_history_signal,
            has_action_signal,
        }
    }
}

/// The conservative multi-label Intent Router.
///
/// Rules (§4.2):
/// + Only time, simple translation, single-sentence rewrite, or pure
///   formatting requests can set Memory=false.
/// + Requests involving workspace, module, path, history, preference,
///   convention, architecture, or non-trivial tasks set Memory=true.
/// + When unsure whether historical info is needed, set Memory=true.
/// + Requests with action intent, explicit Skill name, or matching
///   Skill trigger set Skill=true.
/// + Requests with "last time, before, when, why failed, original,
///   evidence, evolution" signals set Evidence=true.
/// + Evidence=true implies Memory=true, but not Skill=true.
pub struct IntentRouter;

impl IntentRouter {
    /// Route a user request to pipeline enable flags.
    pub fn route(user_input: &str) -> RouterDecision {
        let lower = user_input.to_lowercase();
        let mut reasons = Vec::new();
        let mut skill = false;
        let mut memory = false;
        let mut evidence = false;
        let mut include_superseded = false;

        // ── Check for self-contained requests (hard skip) ──
        if Self::is_self_contained(&lower) {
            return RouterDecision {
                skill_enabled: false,
                memory_enabled: false,
                evidence_enabled: false,
                include_superseded: false,
                reason_codes: vec!["self_contained".to_string()],
                hard_skip_reason: Some("clearly self-contained request".to_string()),
            };
        }

        // ── Evidence signals (history, why, original, evolution) ──
        let evidence_signals = [
            "last time", "before", "previously", "when", "why", "failed",
            "original", "evidence", "evolution", "last", "上次", "之前", "当时",
            "为什么", "原文", "证据", "演变", "历史",
        ];
        for signal in &evidence_signals {
            if lower.contains(signal) {
                evidence = true;
                reasons.push(format!("evidence_signal:{signal}"));
                break;
            }
        }

        // ── Evolution/history queries include superseded ──
        let evolution_signals = ["evolution", "evolve", "演变", "how did", "how does", "changed from", "superseded"];
        for signal in &evolution_signals {
            if lower.contains(signal) {
                include_superseded = true;
                evidence = true;
                reasons.push(format!("evolution_signal:{signal}"));
                break;
            }
        }

        // ── Skill signals (action intent, skill names, triggers) ──
        let action_signals = [
            "release", "publish", "deploy", "build", "test", "lint", "format",
            "run", "install", "setup", "configure", "create", "delete", "update",
            "发布", "部署", "构建", "测试", "安装", "配置", "创建", "删除", "更新",
        ];
        for signal in &action_signals {
            if lower.contains(signal) {
                skill = true;
                reasons.push(format!("action_signal:{signal}"));
                break;
            }
        }

        // ── Memory signals (workspace, module, path, preference, etc.) ──
        let memory_signals = [
            "workspace", "module", "path", "file", "project", "config",
            "preference", "convention", "architecture", "dependency", "crate",
            "工作区", "模块", "路径", "文件", "项目", "配置", "偏好", "约定", "架构", "依赖",
        ];
        for signal in &memory_signals {
            if lower.contains(signal) {
                memory = true;
                reasons.push(format!("memory_signal:{signal}"));
                break;
            }
        }

        // ── Conservative default: when unsure, enable Memory ──
        // (miss cost > empty retrieval cost)
        if !memory && !evidence && !skill {
            // If the request is non-trivial (more than a few words), enable memory.
            let word_count = user_input.split_whitespace().count();
            if word_count > 3 {
                memory = true;
                reasons.push("conservative_default_non_trivial".to_string());
            }
        }

        // Evidence=true implies Memory=true.
        if evidence && !memory {
            memory = true;
            reasons.push("evidence_implies_memory".to_string());
        }

        RouterDecision {
            skill_enabled: skill,
            memory_enabled: memory,
            evidence_enabled: evidence,
            include_superseded,
            reason_codes: reasons,
            hard_skip_reason: None,
        }
    }

    /// Check if a request is clearly self-contained (no retrieval needed).
    ///
    /// Only time queries, simple translations, single-sentence rewrites,
    /// or pure formatting requests qualify.
    fn is_self_contained(lower: &str) -> bool {
        let trimmed = lower.trim();

        // Time queries
        let time_patterns = [
            "what time", "current time", "today's date", "what day",
            "现在几点", "今天", "当前时间",
        ];
        for p in &time_patterns {
            if trimmed.contains(p) {
                return true;
            }
        }

        // Simple translation requests (very short)
        if (trimmed.starts_with("translate ") || trimmed.starts_with("翻译"))
            && trimmed.split_whitespace().count() <= 5
        {
            return true;
        }

        // Pure formatting / rewrite (very short, no domain words)
        let formatting_patterns = [
            "rewrite", "rephrase", "capitalize", "lowercase", "uppercase",
            "重写", "改写",
        ];
        for p in &formatting_patterns {
            if trimmed.starts_with(p) && trimmed.split_whitespace().count() <= 4 {
                return true;
            }
        }

        false
    }

    /// Compute a query fingerprint for diagnostics (not the raw text).
    pub fn fingerprint(user_input: &str) -> QueryFingerprint {
        let normalized: String = user_input
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let lower = normalized.to_lowercase();

        let history_signals = [
            "last", "before", "previously", "why", "failed", "original",
            "evidence", "evolution", "上次", "之前", "为什么", "原文", "证据",
        ];
        let action_signals = [
            "release", "publish", "deploy", "build", "test", "run", "create",
            "发布", "部署", "构建", "测试",
        ];

        QueryFingerprint {
            term_count: normalized.split_whitespace().count(),
            has_history_signal: history_signals.iter().any(|s| lower.contains(s)),
            has_action_signal: action_signals.iter().any(|s| lower.contains(s)),
            normalized,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_contained_time_query_hard_skips() {
        let decision = IntentRouter::route("What time is it?");
        assert!(!decision.memory_enabled);
        assert!(decision.hard_skip_reason.is_some());
    }

    #[test]
    fn workspace_query_enables_memory() {
        let decision = IntentRouter::route("How do we configure the workspace module?");
        assert!(decision.memory_enabled);
        assert!(!decision.evidence_enabled);
    }

    #[test]
    fn action_query_enables_skill() {
        let decision = IntentRouter::route("Publish a new release version");
        assert!(decision.skill_enabled);
    }

    #[test]
    fn history_query_enables_evidence_and_memory() {
        let decision = IntentRouter::route("Why did the build fail last time?");
        assert!(decision.evidence_enabled);
        assert!(decision.memory_enabled, "evidence implies memory");
    }

    #[test]
    fn evolution_query_includes_superseded() {
        let decision = IntentRouter::route("How did the release workflow evolve?");
        assert!(decision.evidence_enabled);
        assert!(decision.include_superseded);
    }

    #[test]
    fn conservative_default_for_non_trivial() {
        let decision = IntentRouter::route("help me understand the database connection pooling");
        assert!(decision.memory_enabled, "non-trivial query should enable memory");
    }

    #[test]
    fn short_trivial_query_may_skip() {
        let decision = IntentRouter::route("hello");
        assert!(!decision.memory_enabled);
    }

    #[test]
    fn combined_skill_memory_evidence() {
        let decision = IntentRouter::route("Redo the last failed release using the previous workflow");
        assert!(decision.skill_enabled);
        assert!(decision.memory_enabled);
        assert!(decision.evidence_enabled);
    }

    #[test]
    fn fingerprint_does_not_expose_raw_text() {
        let fp = IntentRouter::fingerprint("What is the Rust release workflow?");
        assert!(fp.term_count > 0);
        assert!(fp.has_action_signal);
        assert!(!fp.has_history_signal);
    }

    #[test]
    fn chinese_history_signal() {
        let decision = IntentRouter::route("上次发布为什么失败");
        assert!(decision.evidence_enabled);
        assert!(decision.memory_enabled);
    }
}
