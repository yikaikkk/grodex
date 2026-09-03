//! LLM Query Understanding — classifies a user query into a structured
//! intent that narrows memory retrieval (scope/kind filters).
//!
//! Design (memory-architecture-redesign.md §Phase 6):
//! - Trait defined here in grodex-memory (no provider dependency).
//! - Implemented in grodex-loop with a real provider client.
//! - The [`MockQueryUnderstanding`] uses rule-based pattern matching so
//!   tests can verify the retrieval pipeline without a live LLM.
//!
//! Example: "我叫什么" → intent=user_identity → scope=Global, kind=Preference

use crate::types::{MemoryKind, MemoryScope};
use serde::{Deserialize, Serialize};

/// The category of information the user is asking about. Drives scope/kind
/// filters in the retrieval pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    /// "who am I / what's my name" → scope=Global, kind=Preference
    UserIdentity,
    /// "what do I prefer" → scope=Global, kind=Preference
    UserPreference,
    /// "what did we decide about X" → scope=Workspace, kind=Decision
    ProjectDecision,
    /// "how does X work / what's the command" → scope=Workspace, kind=Fact/Solution
    ProjectFact,
    /// "what are the rules / constraints" → scope=Workspace, kind=Constraint
    ProjectConstraint,
    /// No specific intent — broad search, no filters.
    General,
}

impl QueryIntent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserIdentity => "user_identity",
            Self::UserPreference => "user_preference",
            Self::ProjectDecision => "project_decision",
            Self::ProjectFact => "project_fact",
            Self::ProjectConstraint => "project_constraint",
            Self::General => "general",
        }
    }

    /// The scope filter this intent implies (None = no filter).
    pub fn scope_hint(&self) -> Option<MemoryScope> {
        match self {
            Self::UserIdentity | Self::UserPreference => Some(MemoryScope::Global),
            Self::ProjectDecision | Self::ProjectFact | Self::ProjectConstraint => {
                Some(MemoryScope::Workspace)
            }
            Self::General => None,
        }
    }

    /// The kind filter this intent implies (None = no filter).
    pub fn kind_hint(&self) -> Option<MemoryKind> {
        match self {
            Self::UserIdentity | Self::UserPreference => Some(MemoryKind::Preference),
            Self::ProjectDecision => Some(MemoryKind::Decision),
            Self::ProjectFact => Some(MemoryKind::Fact),
            Self::ProjectConstraint => Some(MemoryKind::Constraint),
            Self::General => None,
        }
    }
}

/// Structured output of query understanding.
#[derive(Debug, Clone, Default)]
pub struct QueryUnderstanding {
    pub intent: QueryIntent,
    /// Optional rewrite of the query for FTS (e.g. "我叫什么" → "用户 称呼 名字").
    pub rewritten_query: Option<String>,
}

impl Default for QueryIntent {
    fn default() -> Self {
        Self::General
    }
}

/// Errors during query understanding.
#[derive(Debug, thiserror::Error)]
pub enum QueryUnderstandingError {
    #[error("provider call failed: {0}")]
    Provider(String),
    #[error("response parse failed: {0}")]
    Parse(String),
}

/// Abstraction over the LLM that classifies a user query.
#[async_trait::async_trait]
pub trait QueryUnderstandingModel: Send + Sync {
    async fn understand(&self, query: &str) -> Result<QueryUnderstanding, QueryUnderstandingError>;
}

/// System prompt for the query understanding LLM call.
pub const QUERY_UNDERSTANDING_PROMPT: &str = r#"You are a query understanding assistant for a memory retrieval system. Given a user's query, classify it into one of these intents:

- user_identity: the user is asking about their own identity (name, nickname, how to address them). e.g. "我叫什么", "what's my name", "你记得我叫什么吗"
- user_preference: the user is asking about their preferences. e.g. "我喜欢什么", "what do I prefer", "我的偏好"
- project_decision: asking about a decision made on this project. e.g. "我们决定了什么", "what did we decide about the schema"
- project_fact: asking about a project fact or how-to. e.g. "构建命令是什么", "how does X work"
- project_constraint: asking about rules or constraints. e.g. "有什么约束", "what are the invariants"
- general: none of the above — a broad query that needs no scope/kind filter.

Respond with JSON: {"intent": "user_identity|user_preference|project_decision|project_fact|project_constraint|general", "rewritten_query": "optional FTS-optimized rewrite or null"}
"#;

// ───────────────────────── Mock ─────────────────────────

/// Rule-based mock for tests. Uses keyword matching to classify intent.
#[derive(Debug, Default)]
pub struct MockQueryUnderstanding;

#[async_trait::async_trait]
impl QueryUnderstandingModel for MockQueryUnderstanding {
    async fn understand(&self, query: &str) -> Result<QueryUnderstanding, QueryUnderstandingError> {
        let q = query.to_lowercase();
        let intent = if matches_identity(&q) {
            QueryIntent::UserIdentity
        } else if matches_preference(&q) {
            QueryIntent::UserPreference
        } else if matches_decision(&q) {
            QueryIntent::ProjectDecision
        } else if matches_constraint(&q) {
            QueryIntent::ProjectConstraint
        } else if matches_fact(&q) {
            QueryIntent::ProjectFact
        } else {
            QueryIntent::General
        };

        // Rewrite: for identity queries, broaden to include "称呼/名字/name".
        let rewritten = if intent == QueryIntent::UserIdentity {
            Some(format!("{query} 用户 称呼 名字 name"))
        } else {
            None
        };

        Ok(QueryUnderstanding { intent, rewritten_query: rewritten })
    }
}

fn matches_identity(q: &str) -> bool {
    let cn = ["我叫什么", "我叫啥", "我叫什么名字", "你记得我", "我是谁", "我的名字", "怎么称呼我"];
    let en = ["what's my name", "what is my name", "who am i", "my name", "call me"];
    cn.iter().any(|p| q.contains(p)) || en.iter().any(|p| q.contains(p))
}

fn matches_preference(q: &str) -> bool {
    let cn = ["我喜欢什么", "我的偏好", "我偏好", "我讨厌什么"];
    let en = ["what do i prefer", "my preference", "what i like"];
    cn.iter().any(|p| q.contains(p)) || en.iter().any(|p| q.contains(p))
}

fn matches_decision(q: &str) -> bool {
    let cn = ["我们决定", "决定了什么", "决策", "为什么选择"];
    let en = ["what did we decide", "the decision", "why did we choose"];
    cn.iter().any(|p| q.contains(p)) || en.iter().any(|p| q.contains(p))
}

fn matches_constraint(q: &str) -> bool {
    let cn = ["约束", "限制", "规则是什么", "不变量"];
    let en = ["constraint", "invariant", "what are the rules"];
    cn.iter().any(|p| q.contains(p)) || en.iter().any(|p| q.contains(p))
}

fn matches_fact(q: &str) -> bool {
    let cn = ["怎么", "如何", "是什么", "构建命令", "怎么启动"];
    let en = ["how to", "how does", "what is the", "build command"];
    cn.iter().any(|p| q.contains(p)) || en.iter().any(|p| q.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn identity_query_classified() {
        let m = MockQueryUnderstanding;
        let r = m.understand("我叫什么").await.unwrap();
        assert_eq!(r.intent, QueryIntent::UserIdentity);
        assert_eq!(r.intent.scope_hint(), Some(MemoryScope::Global));
        assert_eq!(r.intent.kind_hint(), Some(MemoryKind::Preference));
        assert!(r.rewritten_query.is_some());
    }

    #[tokio::test]
    async fn english_identity_query() {
        let m = MockQueryUnderstanding;
        let r = m.understand("what's my name").await.unwrap();
        assert_eq!(r.intent, QueryIntent::UserIdentity);
    }

    #[tokio::test]
    async fn preference_query_classified() {
        let m = MockQueryUnderstanding;
        let r = m.understand("我喜欢什么").await.unwrap();
        assert_eq!(r.intent, QueryIntent::UserPreference);
        assert_eq!(r.intent.scope_hint(), Some(MemoryScope::Global));
    }

    #[tokio::test]
    async fn decision_query_classified() {
        let m = MockQueryUnderstanding;
        let r = m.understand("我们决定了什么").await.unwrap();
        assert_eq!(r.intent, QueryIntent::ProjectDecision);
        assert_eq!(r.intent.kind_hint(), Some(MemoryKind::Decision));
    }

    #[tokio::test]
    async fn general_query_no_filter() {
        let m = MockQueryUnderstanding;
        let r = m.understand("帮我写个函数").await.unwrap();
        assert_eq!(r.intent, QueryIntent::General);
        assert_eq!(r.intent.scope_hint(), None);
        assert_eq!(r.intent.kind_hint(), None);
    }
}
