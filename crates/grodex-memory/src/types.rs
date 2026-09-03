//! Core unit types for the Memory V2 three-way separation.
//!
//! Design 08 §2.1: Skill, Long-term Memory, and Evidence must be separated
//! into independent pipelines with their own candidate pools, Top N, and
//! relevance formulas. These types model the structured projections stored
//! in SQLite; the Markdown files with HTML comment IDs remain the source of
//! truth.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ───────────────────────── Memory Unit ─────────────────────────

/// The kind of long-term memory a unit represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// A stable fact about the project or environment.
    Fact,
    /// A user preference (global or workspace).
    Preference,
    /// An architectural or process decision.
    Decision,
    /// A long-term constraint or invariant.
    Constraint,
    /// A confirmed problem and its solution.
    Solution,
}

impl MemoryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Decision => "decision",
            Self::Constraint => "constraint",
            Self::Solution => "solution",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "fact" => Some(Self::Fact),
            "preference" => Some(Self::Preference),
            "decision" => Some(Self::Decision),
            "constraint" => Some(Self::Constraint),
            "solution" => Some(Self::Solution),
            _ => None,
        }
    }
}

/// The scope at which a memory unit applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// Applies to the current workspace/project.
    Workspace,
    /// Applies globally across all projects.
    Global,
}

impl MemoryScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Global => "global",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "workspace" => Some(Self::Workspace),
            "global" => Some(Self::Global),
            _ => None,
        }
    }
}

/// The lifecycle status of a memory unit.
///
/// v4 state machine: candidate → active → superseded → conflicted
///                     → needs_review → dismissed / orphaned
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    /// Model just extracted; not yet promoted to active.
    Candidate,
    /// Active and eligible for retrieval.
    Active,
    /// Replaced by a newer memory unit.
    Superseded,
    /// Conflicts with another memory unit; pending resolution.
    Conflicted,
    /// Needs human confirmation before promotion.
    NeedsReview,
    /// Explicitly rejected; kept for audit.
    Dismissed,
    /// Source file or section disappeared; kept for provenance/diagnostics.
    Orphaned,
}

impl UnitStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Conflicted => "conflicted",
            Self::NeedsReview => "needs_review",
            Self::Dismissed => "dismissed",
            Self::Orphaned => "orphaned",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "candidate" => Some(Self::Candidate),
            "active" => Some(Self::Active),
            "superseded" => Some(Self::Superseded),
            "conflicted" => Some(Self::Conflicted),
            "needs_review" => Some(Self::NeedsReview),
            "dismissed" => Some(Self::Dismissed),
            "orphaned" => Some(Self::Orphaned),
            _ => None,
        }
    }
}

/// A long-term memory unit — the stable knowledge atom.
///
/// The `id` is written into the Markdown source as an HTML comment
/// (e.g. `<!-- memory-unit: {"id":"mem_x","kind":"fact",...} -->`)
/// and is the identity of this unit across rewrites and moves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryUnit {
    /// Stable identifier from the Markdown HTML comment.
    pub id: String,
    /// Source file path.
    pub path: String,
    /// Derived display locator (e.g. "MEMORY.md#release-workflow").
    pub section: String,
    /// What kind of knowledge this is.
    pub kind: MemoryKind,
    /// Scope of applicability.
    pub scope: MemoryScope,
    /// Lifecycle status.
    pub status: UnitStatus,
    /// The indexed text content.
    pub content: String,
    /// SHA-256 hash of the source section content.
    pub content_hash: String,
    /// When the source was last updated.
    pub updated_at: DateTime<Utc>,
    /// When this unit was first indexed.
    pub created_at: DateTime<Utc>,
}

// ───────────────────────── Evidence Unit ─────────────────────────

/// The lifecycle status of an evidence unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// Active evidence available for retrieval.
    Active,
    /// Superseded by a memory unit; excluded from normal retrieval.
    Superseded,
    /// Source file disappeared; kept for provenance/diagnostics.
    Orphaned,
}

impl EvidenceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Orphaned => "orphaned",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "superseded" => Some(Self::Superseded),
            "orphaned" => Some(Self::Orphaned),
            _ => None,
        }
    }
}

/// An evidence unit extracted from a rollout by Phase 1.
///
/// Evidence answers "what happened last time" and "why was this conclusion
/// reached". It is separate from long-term memory and only retrieved when
/// historical verification is needed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceUnit {
    /// Stable identifier from the Markdown HTML comment.
    pub id: String,
    /// The rollout this evidence was extracted from.
    pub rollout_id: String,
    /// Source file path (e.g. summary file).
    pub path: String,
    /// Derived display locator.
    pub section: String,
    /// Scope of this evidence.
    pub scope: MemoryScope,
    /// Lifecycle status.
    pub status: EvidenceStatus,
    /// The indexed text content.
    pub content: String,
    /// SHA-256 hash of the source section content.
    pub content_hash: String,
    /// When the original event occurred.
    pub occurred_at: DateTime<Utc>,
    /// When this evidence unit was first indexed.
    pub created_at: DateTime<Utc>,
    /// The memory unit that supersedes this evidence, if any.
    pub superseded_by: Option<String>,
    /// When this evidence was superseded.
    pub superseded_at: Option<DateTime<Utc>>,
    /// Whether the original rollout is still available for deep-dive.
    pub rollout_available: bool,
    /// When the rollout expired (was deleted by TTL), if applicable.
    pub rollout_expired_at: Option<DateTime<Utc>>,
    /// Sub-chunk index for units exceeding max_chunk_chars (0 = whole unit).
    pub subchunk_index: i64,
}

// ───────────────────────── Skill Catalog Entry ─────────────────────────

/// A skill catalog entry — discoverability metadata only.
///
/// The full `SKILL.md` text is never indexed; only name, description,
/// and triggers participate in retrieval. This prevents skill body text
/// from causing false matches on fact-oriented queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCatalogEntry {
    /// Unique skill identifier.
    pub skill_id: String,
    /// Human-readable name.
    pub name: String,
    /// What this skill does (must contain an action intent).
    pub description: String,
    /// When to use this skill.
    pub when_to_use: String,
    /// Trigger keywords/phrases (JSON array in DB).
    pub triggers: Vec<String>,
    /// Scope of this skill.
    pub scope: MemoryScope,
    /// Whether this skill is currently enabled.
    pub enabled: bool,
    /// Capabilities required to use this skill.
    pub required_capabilities: Vec<String>,
    /// Path to the SKILL.md entry file.
    pub entry_path: String,
    /// SHA-256 hash of the entry file content.
    pub content_hash: String,
    /// When this skill was added to the catalog.
    pub created_at: DateTime<Utc>,
    /// When the catalog entry was last updated.
    pub updated_at: DateTime<Utc>,
}

// ───────────────────────── Provenance Edge ─────────────────────────

/// The type of relationship between a memory unit and an evidence unit.
///
/// Design 08 §8.1: at least supports, derived_from, supersedes, conflicts_with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRelation {
    /// The memory unit is supported by this evidence.
    Supports,
    /// The memory unit was derived from this evidence.
    DerivedFrom,
    /// The memory unit supersedes (replaces) this evidence.
    Supersedes,
    /// The memory unit conflicts with this evidence (unresolved).
    ConflictsWith,
}

impl EdgeRelation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supports => "supports",
            Self::DerivedFrom => "derived_from",
            Self::Supersedes => "supersedes",
            Self::ConflictsWith => "conflicts_with",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "supports" => Some(Self::Supports),
            "derived_from" => Some(Self::DerivedFrom),
            "supersedes" => Some(Self::Supersedes),
            "conflicts_with" => Some(Self::ConflictsWith),
            _ => None,
        }
    }
}

/// A provenance edge connecting a memory unit to an evidence unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceEdge {
    /// The memory unit that digests the evidence.
    pub memory_id: String,
    /// The evidence unit being referenced.
    pub evidence_id: String,
    /// The type of relationship.
    pub relation: EdgeRelation,
    /// When this edge was created.
    pub created_at: DateTime<Utc>,
}

// ───────────────────────── Certainty (v4) ─────────────────────────

/// How confident we are that a claim is a stable fact vs. a hypothesis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Certainty {
    /// User explicitly stated this; no inference needed.
    Explicit,
    /// Inferred from context but not directly stated.
    Inferred,
    /// A model hypothesis or observation; not yet a stable fact.
    Hypothesis,
}

impl Certainty {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Inferred => "inferred",
            Self::Hypothesis => "hypothesis",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "explicit" => Some(Self::Explicit),
            "inferred" => Some(Self::Inferred),
            "hypothesis" => Some(Self::Hypothesis),
            _ => None,
        }
    }
}

// ───────────────────────── Memory Proposal (v4) ─────────────────────────

/// A proposed memory unit awaiting validation and commit.
///
/// Flow: LLM Extractor → hard-constraint validation → memory_proposals table
///       → conflict judgment → MemoryCommitted → memory_units
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProposal {
    /// Unique proposal identifier.
    pub proposal_id: String,
    /// The fact content (human-readable, normalized).
    pub content: String,
    /// Kind of memory.
    pub kind: MemoryKind,
    /// Scope of applicability.
    pub scope: MemoryScope,
    /// Model confidence (0.0–1.0).
    pub confidence: f64,
    /// How certain the model is about this being a stable fact.
    pub certainty: Certainty,
    /// JSON array of evidence IDs supporting this proposal.
    pub source_evidence_ids: Vec<String>,
    /// Originating rollout ID.
    pub source_rollout_id: String,
    /// Start seq in the rollout.
    pub source_seq_start: i64,
    /// End seq in the rollout.
    pub source_seq_end: i64,
    /// Turn ID if available.
    pub source_turn_id: String,
    /// Which extractor model produced this.
    pub extractor_model: String,
    /// Proposal lifecycle: pending / committed / dismissed.
    pub status: ProposalStatus,
    /// If dismissed, why (validation failure reason).
    pub rejection_reason: String,
    /// When the proposal was created.
    pub created_at: DateTime<Utc>,
    /// When it was resolved (committed or dismissed).
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Lifecycle status of a memory proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Awaiting validation and conflict check.
    Pending,
    /// Successfully committed as a memory unit.
    Committed,
    /// Rejected by hard constraints or conflict resolution.
    Dismissed,
}

impl ProposalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "committed" => Some(Self::Committed),
            "dismissed" => Some(Self::Dismissed),
            _ => None,
        }
    }
}

// ───────────────────────── Memory Conflict (v4) ─────────────────────────

/// The semantic relationship between two memory units, as judged by the LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictRelation {
    /// Exactly the same content.
    Duplicate,
    /// Semantically equivalent but different wording.
    Equivalent,
    /// New fact replaces the old one (e.g., name change).
    Supersedes,
    /// Contradicts the other; both cannot be true.
    Conflicts,
    /// Unrelated; both can coexist.
    Independent,
}

impl ConflictRelation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Equivalent => "equivalent",
            Self::Supersedes => "supersedes",
            Self::Conflicts => "conflicts",
            Self::Independent => "independent",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "duplicate" => Some(Self::Duplicate),
            "equivalent" => Some(Self::Equivalent),
            "supersedes" => Some(Self::Supersedes),
            "conflicts" => Some(Self::Conflicts),
            "independent" => Some(Self::Independent),
            _ => None,
        }
    }
}

/// A conflict record between two memory units.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConflict {
    /// Unique conflict identifier.
    pub conflict_id: String,
    /// One side of the conflict.
    pub left_memory_id: String,
    /// The other side.
    pub right_memory_id: String,
    /// The semantic relationship.
    pub relation: ConflictRelation,
    /// Model confidence in this judgment (0.0–1.0).
    pub confidence: f64,
    /// Model's explanation.
    pub reason: String,
    /// Pending or resolved.
    pub status: ConflictStatus,
    /// When resolved.
    pub resolved_at: Option<DateTime<Utc>>,
    /// How it was resolved.
    pub resolution: String,
    /// When the conflict was recorded.
    pub created_at: DateTime<Utc>,
}

/// Lifecycle status of a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictStatus {
    /// Awaiting resolution.
    Pending,
    /// Resolved (e.g., old memory superseded).
    Resolved,
}

impl ConflictStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "resolved" => Some(Self::Resolved),
            _ => None,
        }
    }
}

// ───────────────────────── Indexed File ─────────────────────────

/// The kind of source file tracked in `indexed_files`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Skill,
    Memory,
    Evidence,
}

impl SourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Memory => "memory",
            Self::Evidence => "evidence",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "skill" => Some(Self::Skill),
            "memory" => Some(Self::Memory),
            "evidence" => Some(Self::Evidence),
            _ => None,
        }
    }
}

/// A tracked source file for incremental indexing correctness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedFile {
    /// File system path.
    pub path: String,
    /// What kind of source file this is.
    pub source_kind: SourceKind,
    /// File modification time (epoch seconds).
    pub mtime: i64,
    /// File size in bytes.
    pub size: i64,
    /// SHA-256 content hash.
    pub content_hash: String,
    /// The index generation when this file was last indexed.
    pub index_generation: u64,
    /// When this file was last indexed.
    pub last_indexed_at: DateTime<Utc>,
}

// ───────────────────────── Retrieval Result ─────────────────────────

/// A single retrieval result from any of the three pipelines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalResult {
    /// The stable unit ID (memory unit id, evidence unit id, or skill id).
    pub unit_id: String,
    /// The source file path.
    pub path: String,
    /// The matched text content.
    pub content: String,
    /// Section title inside the source file (empty if not applicable).
    #[serde(default)]
    pub section: String,
    /// Memory unit kind (Fact / Preference / …). Populated for memory
    /// results only.
    #[serde(skip)]
    pub memory_kind: Option<MemoryKind>,
    /// Memory: when the source was last updated.
    /// Evidence: when the original event occurred.
    #[serde(skip)]
    pub updated_at: Option<chrono::DateTime<Utc>>,
    /// Evidence-only: the originating rollout session id.
    #[serde(skip)]
    pub rollout_id: String,
    /// Evidence-only: memory id that supersedes this evidence, if any.
    #[serde(skip)]
    pub superseded_by: Option<String>,
    /// Evidence-only alias — populated by load_evidence_results_in_order.
    #[serde(skip)]
    pub occurred_at: Option<chrono::DateTime<Utc>>,
    /// BM25 score from FTS5 (for diagnostics; not used as an absolute threshold).
    #[serde(default)]
    pub bm25_score: f64,
    /// Number of query terms that matched this candidate.
    #[serde(default)]
    pub term_coverage: usize,
    /// Total distinct query terms.
    #[serde(default)]
    pub total_terms: usize,
    /// Which pipeline produced this result.
    pub source: ResultSource,
}

/// Which retrieval pipeline produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultSource {
    Skill,
    Memory,
    Evidence,
    GlobalUserPreference,
}

impl ResultSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Memory => "memory",
            Self::Evidence => "evidence",
            Self::GlobalUserPreference => "global_user_preference",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_kind_roundtrip() {
        for kind in [
            MemoryKind::Fact,
            MemoryKind::Preference,
            MemoryKind::Decision,
            MemoryKind::Constraint,
            MemoryKind::Solution,
        ] {
            assert_eq!(MemoryKind::from_str(kind.as_str()), Some(kind));
        }
        assert_eq!(MemoryKind::from_str("unknown"), None);
    }

    #[test]
    fn evidence_status_roundtrip() {
        for status in [
            EvidenceStatus::Active,
            EvidenceStatus::Superseded,
            EvidenceStatus::Orphaned,
        ] {
            assert_eq!(EvidenceStatus::from_str(status.as_str()), Some(status));
        }
    }

    #[test]
    fn edge_relation_roundtrip() {
        for rel in [
            EdgeRelation::Supports,
            EdgeRelation::DerivedFrom,
            EdgeRelation::Supersedes,
            EdgeRelation::ConflictsWith,
        ] {
            assert_eq!(EdgeRelation::from_str(rel.as_str()), Some(rel));
        }
    }

    #[test]
    fn source_kind_roundtrip() {
        for kind in [SourceKind::Skill, SourceKind::Memory, SourceKind::Evidence] {
            assert_eq!(SourceKind::from_str(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn unit_status_v4_roundtrip() {
        for status in [
            UnitStatus::Candidate,
            UnitStatus::Active,
            UnitStatus::Superseded,
            UnitStatus::Conflicted,
            UnitStatus::NeedsReview,
            UnitStatus::Dismissed,
            UnitStatus::Orphaned,
        ] {
            assert_eq!(UnitStatus::from_str(status.as_str()), Some(status));
        }
        assert_eq!(UnitStatus::from_str("unknown"), None);
    }

    #[test]
    fn certainty_roundtrip() {
        for c in [Certainty::Explicit, Certainty::Inferred, Certainty::Hypothesis] {
            assert_eq!(Certainty::from_str(c.as_str()), Some(c));
        }
    }

    #[test]
    fn conflict_relation_roundtrip() {
        for r in [
            ConflictRelation::Duplicate,
            ConflictRelation::Equivalent,
            ConflictRelation::Supersedes,
            ConflictRelation::Conflicts,
            ConflictRelation::Independent,
        ] {
            assert_eq!(ConflictRelation::from_str(r.as_str()), Some(r));
        }
    }

    #[test]
    fn proposal_status_roundtrip() {
        for s in [ProposalStatus::Pending, ProposalStatus::Committed, ProposalStatus::Dismissed] {
            assert_eq!(ProposalStatus::from_str(s.as_str()), Some(s));
        }
    }
}
