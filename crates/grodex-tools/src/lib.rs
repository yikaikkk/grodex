//! Grodex Tools — built-in tool implementations.
//!
//! Provides the fundamental built-in tools:
//!   - ReadFileTool: read files with line numbers
//!   - WriteFileTool: create/overwrite files
//!   - EditTool: exact string replacement in files
//!   - ExecTool: run shell commands
//!   - ProcessIoTool: interact with background processes (§11.2)
//!   - ReadArtifactTool: retrieve offloaded large tool results (§12)
//!
//! V2 cross-tool primitives (§7–12 of Design Doc 15) live in `common`:
//!   - `BuiltInTool` two-phase trait (prepare + execute), `PreparedCall`
//!   - FileSnapshot / ReadRange / HashlineAnchor (Read stability)
//!   - ToolResultEnvelope + HeadTailBuffer + TruncationInfo
//!   - ProcessHandle + ExecStatus (long-running Exec)
//!   - PatchPlan + PatchFile (atomic apply_patch)
//!   - StaleFile + AtomicityLevel (version fence + atomic writes)

pub mod blob_refs;
pub mod blob_store;
pub mod blocking;
pub mod cancel;
pub mod common;
pub mod edit;
pub mod exec;
pub mod load_skill;
pub mod fsutil;
pub mod patch;
pub mod process_io;
pub mod read;
pub mod read_artifact;
pub mod registry;
pub mod write;

pub use blob_refs::{BlobOwnerKind, BlobRefLedger, BlobRefKind, BlobRefRecord};
pub use blob_store::{BlobRef, BlobStore, BoundedView, FileBlobStore, InMemoryBlobStore, ManagedBlobStore};
pub use cancel::{CancelPipeline, CancelRegistry, CancelResult, CancellationToken};
pub use blocking::run_blocking_io;
pub use common::{
    ArtifactRef, AtomicityLevel, BuiltInTool, ChangedResource, ChangeType, ExecOutput, ExecStatus,
    FileSnapshot, FileType, HashlineAnchor, HeadTailBuffer, LineEnding, ModelContent,
    PatchFile, PatchHunk, PatchOperation, PatchPlan, PreparedCall, ProcessHandle, ProcessState,
    ReadRange, ReadRender, Retryability, SideEffectHint, StaleFile, StaleSuggestion,
    ToolResultEnvelope, ToolStatus, TruncationInfo, TruncationStrategy,
};
pub use edit::EditTool;
pub use exec::ExecTool;
pub use load_skill::{LoadSkillTool, SharedSkillCatalog};
pub use fsutil::{assert_within_root, canonicalize, FileVersion};
pub use patch::ApplyPatchTool;
pub use process_io::{ProcessIoTool, ProcessManager};
pub use read::ReadFileTool;
pub use read_artifact::ReadArtifactTool;
pub use registry::ToolRegistry;
pub use write::WriteFileTool;
