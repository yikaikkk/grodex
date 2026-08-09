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
//! Vector/embedding search is deferred to V2 (requires Eval evidence).
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
//! | `backfill` | Embedding Backfiller (skeleton; runs after Eval recall baseline established) |
//! | `router` | Multi-label conservative Intent Router with diagnostics |
//! | `negative_cache` | Session-level empty-result cache |
//! | `eval` | Offline replay Eval harness + MemoryEvalCli (with_embedding_model for hybrid delta) |
//! | `entry` | Legacy in-memory MemoryEntry (kept for backward compat) |
//! | `store` | Legacy in-memory MemoryStore (with hybrid stubs) |
//! | `retriever` | Legacy keyword retriever |

// V2 modules
pub mod backfill;
pub mod database;
pub mod embedding;
pub mod eval;
pub mod indexer;
pub mod negative_cache;
pub mod retrievers;
pub mod router;
pub mod sampling;
pub mod schema;
pub mod template;
pub mod types;

// Legacy modules (kept for backward compatibility)
pub mod entry;
pub mod retriever;
pub mod store;

// V2 re-exports
pub use backfill::{backfill_missing_embeddings, batch_chunks, is_backfill_possible};
pub use database::{DbError, MemoryDatabase, RetrievedUnit};
pub use embedding::{
    EmbeddingConfig, EmbeddingError, EmbeddingModel, EmbeddingVector, OpenAiCompatibleModel,
    cosine_similarity,
};
pub use eval::{EvalManifest, EvalMetrics, EvalSample, MemoryEvalCli, compute_metrics};
pub use indexer::{
    apply_deletions, reconcile, scan_directory, ConsolidationState, ConsolidationTx,
    ReconciliationDiff, ScannedFile,
};
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
pub use template::{EvidenceMetadata, EvidenceTemplate, EvidenceValidation};
pub use types::*;

// Legacy re-exports
pub use entry::MemoryEntry;
pub use retriever::MemoryRetriever as LegacyRetriever;
pub use store::MemoryStore;
