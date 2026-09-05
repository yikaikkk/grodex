//! Grodex Memory — long-term memory management with V2 three-way retrieval.
//!
//! ## Architecture (Design 08)
//!
//! The memory system separates three retrieval pipelines:
//! - **Skill**: intent → workflow selection (metadata only, not full text)
//! - **Long-term Memory**: stable facts, preferences, decisions
//! - **Evidence**: historical session records for verification
//!
//! V1 uses SQLite FTS5 with term coverage qualification gates.
//! Vector/embedding search is opt-in via `[memory.embedding]` config:
//! disabled by default, startup backfill writes vectors incrementally,
//! and any failure degrades silently to pure FTS5.
//!
//! ## Module layout
//!
//! | Module | Responsibility |
//! |--------|---------------|
//! | `schema` | 12-table SQLite DDL + index_generation (v2: +document_embeddings + embedding_metadata) |
//! | `types` | Core unit types: MemoryUnit, EvidenceUnit, SkillCatalogEntry, ProvenanceEdge |
//! | `database` | SQLite-backed CRUD store + async vector write / brute-force cosine kNN / hybrid RRF entry |
//! | `embedding` | EmbeddingModel trait + OpenAI-compatible HTTP implementation + cosine_similarity |
//! | `retrievers` | Three-way FTS5 pipelines + term coverage gate + reciprocal_rank_fusion helpers |
//! | `backfill` | Embedding Backfiller: incremental startup backfill of missing vectors |
//! | `router` | Multi-label conservative Intent Router with diagnostics |
//! | `negative_cache` | Session-level empty-result cache |
//! | `eval` | Offline replay Eval harness + MemoryEvalCli (with_embedding_model for hybrid delta) |
//! | `governance` | Conflict detection, rollout TTL expiry, stale-memory decay, embedding model rotation hooks |
//! | `entry` | Legacy in-memory MemoryEntry (kept for backward compat) |
//! | `store` | Legacy in-memory MemoryStore (with hybrid stubs) |
//! | `retriever` | Legacy keyword retriever |

// V2 modules
pub mod backfill;
pub mod conflict_judge;
pub mod consolidator;
pub mod database;
pub mod embedding;
pub mod eval;
pub mod governance;
pub mod indexer;
pub mod llm_extractor;
pub mod negative_cache;
pub mod parser;
pub mod proposal;
pub mod query_understanding;
pub mod retrievers;
pub mod rollout_extractor;
pub mod router;
pub mod sampling;
pub mod schema;
pub mod static_context;
pub mod template;
pub mod types;

// Legacy modules (kept for backward compatibility)
pub mod entry;
pub mod retriever;
pub mod store;

// V2 re-exports
pub use backfill::{backfill_missing_embeddings, batch_chunks, is_backfill_possible};
pub use database::{
    DbError, MemoryDatabase, RetrievedUnit, doc_ref_to_unit_id, evidence_doc_ref, memory_doc_ref,
};
pub use embedding::{
    EmbeddingConfig, EmbeddingError, EmbeddingModel, EmbeddingVector, OpenAiCompatibleModel,
    cosine_similarity,
};
pub use eval::{EvalManifest, EvalMetrics, EvalSample, MemoryEvalCli, compute_metrics};
pub use indexer::{
    apply_deletions, reconcile, scan_directory, ConsolidationState, ConsolidationTx,
    ReconciliationDiff, ScannedFile,
};
pub use llm_extractor::{
    AssistantSegment, EvidenceAuthority, EvidenceExtractor, ExtractionContext, ExtractionError,
    ExtractionResult, ExtractedClaim, MockEvidenceExtractor, RolloutEventSummary, SourceRef,
    ToolCallSummary, ToolResultSummary, EXTRACTOR_SYSTEM_PROMPT, render_context_for_llm,
    gate_extraction_output, MemoryRuleMode, MemoryWriteGateDecision, extract_name,
};
pub use consolidator::ConsolidationReport;
pub use governance::{GovernanceReport, format_governance_banner, run_conflict_resolution_pass};
pub use parser::{ParsedMemoryChunk, ParsedMemoryFile};
pub use proposal::{ProposalCommitReport, ProposalGateOptions, RejectedClaim, propose_and_commit, validate_claim, create_proposal, lookup_evidence_ids_for_claim};
pub use conflict_judge::{
    ConflictJudge, ConflictJudgeError, ConflictJudgeInput, ConflictJudgeResult,
    MockConflictJudge, CONFLICT_JUDGE_PROMPT,
};
pub use query_understanding::{
    MockQueryUnderstanding, QueryUnderstanding, QueryUnderstandingError, QueryUnderstandingModel,
    QueryIntent, QUERY_UNDERSTANDING_PROMPT,
};
pub use rollout_extractor::ExtractionReport;
pub use negative_cache::{CacheEntry, NegativeCache};
pub use retrievers::{
    CombinedRetrieval, EvidenceRetriever, MemoryRetriever, RetrievalConfig, RetrievalDiagnostics,
    SkillRetriever, TermCoverageGate, reciprocal_rank_fusion, retrieve_all,
    retrieve_fts_evidence_ids_only, retrieve_fts_memory_ids_only,
};
pub use router::{IntentRouter, QueryFingerprint, RouterDecision};
pub use sampling::{
    build_manifest, extract_queries_from_events, format_metrics_report, run_eval_against_db,
    run_eval_cycle, EvalLabels, ExtractedQuery, QueryLabels,
};
pub use schema::{SCHEMA_VERSION, apply_schema, bump_index_generation, read_index_generation};
pub use static_context::{StaticContext, StaticContextLoader};
pub use template::{EvidenceMetadata, EvidenceTemplate, EvidenceValidation};
pub use types::*;

// Legacy re-exports
pub use entry::MemoryEntry;
pub use retriever::MemoryRetriever as LegacyRetriever;
pub use store::MemoryStore;
