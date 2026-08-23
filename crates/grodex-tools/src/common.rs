//! Shared Doc-15 V2 primitives reused across all built-in tools.
//!
//! Design Doc 15 §7–12 defines a cross-tool vocabulary for:
//!   * the two-phase prepare/execute split (§7);
//!   * the unified result envelope with bounded model views + artifacts (§8);
//!   * the stable FileSnapshot + version fence for Read/Edit freshness (§9 + §12);
//!   * the HeadTailBuffer / ProcessHandle Exec primitives (§11);
//!   * the atomic PatchPlan used by the multi-file apply_patch tool (§10).
//!
//! All structs are Serde round-trippable so they can be stored in rollout
//! events and reproduced exactly during replay.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

// ── Two-phase built-in tool trait (§7) ────────────────────────────────────

/// A prepared, side-effect-free representation of a tool invocation.
///
/// Produced by `BuiltInTool::prepare()`. Holds *exactly* the operations the
/// tool will perform, so approval/policy gates can be applied to it without
/// re-parsing untrusted model strings. Execute consumes one of these.
pub trait PreparedCall {
    /// Classify side effects so sandbox and policy layers can make fast
    /// decisions without walking the whole plan.
    fn side_effect_hint(&self) -> SideEffectHint;
    /// A deterministic, short id for this plan — used for dedup and as an
    /// approval token. Defaults to a debug string; tools should override
    /// with a stable hash over the plan.
    fn plan_id(&self) -> String {
        format!("{:x}", std::num::Wrapping(0))
    }
}

/// Rough side-effect classification used by policy and sandbox routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideEffectHint {
    /// Pure read, no mutations (Read in any form).
    ReadOnly,
    /// Local fs writes only (Edit, Write, ApplyPatch) — no network/process.
    LocalFsWrite,
    /// Executes arbitrary processes (ExecTool variants).
    ProcessSpawn,
    /// Network/IPC outbound calls.
    NetworkCall,
    /// Combination or unknown — treated as the most restrictive category.
    Other,
}

/// The canonical two-phase built-in tool contract (§7).
///
/// Implementors keep `prepare` pure — no filesystem writes, no process
/// spawns, no network. Only `execute` may cause side effects, and it must
/// treat the `Prepared` plan as authoritative (it cannot go back to the
/// model's raw input and derive a different set of targets).
pub trait BuiltInTool {
    /// Parsed, validated input schema (tool-specific).
    type Input;
    /// Pure prepare output.
    type Prepared: PreparedCall;
    /// Result envelope returned on success.
    type OkResult;
    /// Tool-specific typed error, usually convertable to ToolError.
    type Error;

    /// Validate input, resolve resources, record version fences, and build
    /// the plan that `execute` will consume. Must be side-effect-free.
    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error>;

    /// Perform the operations described in `prepared`. Callers guarantee
    /// that policy checks, sandbox grants, and approvals have already
    /// been applied based on the `Prepared` plan.
    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error>;
}

// ── Read V2 primitives (§9) ──────────────────────────────────────────────

/// A range within a file that a Read call operates against (§9.1).
///
/// Uses `#[serde(untagged)]` so the model can pass plain objects
/// (e.g. `{"start_line": 10, "count": null}`) without needing to
/// wrap them in the variant name. LLM function-calling schemas
/// do not understand serde's default externally-tagged encoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReadRange {
    /// No explicit range — handler chooses a safe default upper bound.
    Whole,
    /// 1-based inclusive [start_line, start_line + count - 1].
    Lines {
        /// First line to include (1-based).
        start_line: u64,
        /// How many lines to include (0 = empty, None = to EOF).
        count: Option<u64>,
    },
    /// Byte offset + byte length.
    Bytes {
        /// 0-based inclusive start byte.
        start_byte: u64,
        /// How many bytes to include (None = to EOF).
        count: Option<u64>,
    },
    /// PDF/Office-style page range.
    Pages {
        /// 1-based start page.
        start_page: u32,
        /// None = through end of document.
        count: Option<u32>,
    },
    /// Pattern-anchor range: find lines matching start_pattern, then
    /// optionally extend to the first line matching end_pattern.
    Anchor {
        /// Regex or literal pattern for the start line.
        start_pattern: String,
        /// Optional pattern for the end line (inclusive). None = just the
        /// matched start line.
        end_pattern: Option<String>,
    },
}

impl Default for ReadRange {
    fn default() -> Self { ReadRange::Whole }
}

/// Read rendering request (§9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ReadRender {
    /// Plain text with canonical line-number format (§9.3).
    #[default]
    Text,
    /// Image output (for PDFs, diagrams, raw image files).
    Image,
    /// Return only the FileSnapshot + mime info — no content bytes.
    Metadata,
    /// Pick based on file-type heuristics.
    Auto,
}

/// Broad file-type classification used by the Read router (§9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileType {
    /// UTF-8 / ASCII / known text codec.
    Text,
    /// Text-like but codec uncertain.
    UnknownEncoding,
    /// Non-text blob; route to metadata + hex preview.
    Binary,
    /// Raster or vector image.
    Image,
    /// Portable Document Format; use extractor adapter.
    Pdf,
    /// Office document (docx/xlsx/pptx/etc).
    Office,
    /// Jupyter-style notebook (cell-aware rendering).
    Notebook,
    /// File too large to classify without explicit range — caller must
    /// narrow to a range.
    Oversized,
    /// Extractor known but extraction failed.
    UnsupportedOrExtractorFailed,
}

/// A single, lightweight hashline anchor placed beside a line of code (§9.3).
///
/// Format in the model-facing output: `L{line}@{short_hash}:     <code>`
///
/// The `short_hash` is intended to be stable against *neighbor* changes but
/// sensitive to line content change. We do not define the exact hash algo
/// here (it is computed by the `ReadTool` that produced the anchor) — we
/// only guarantee that `EditTool` will verify it before applying an edit
/// that references it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashlineAnchor {
    /// 1-based line number in the file at the time the anchor was produced.
    pub line_number: u64,
    /// Short stable hash string (usually 6-10 chars). Opaque to consumers.
    pub short_hash: String,
    /// Optional raw line text at anchor time (for edit validation; not
    /// part of the model-facing format).
    pub expected_line_text: Option<String>,
}

impl std::fmt::Display for HashlineAnchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "L{}@{}", self.line_number, self.short_hash)
    }
}

/// Canonical description of a file at a particular moment in time (§9.2).
///
/// A FileSnapshot is what every Read produces, and every Edit consumes
/// through `expected_file_version` / `expected_snapshot` fields. Edit
/// prepare fences the write against this snapshot's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// Stable, canonical identity for the resource (e.g. `fs://{abs_path}`
    /// or `env://{env_id}/{cwd_rel}`). Edit locks are keyed on this.
    pub canonical_resource_id: String,
    /// Human-readable path used in summaries and error messages.
    pub display_path: PathBuf,
    /// Classified file type from the Read routing table.
    pub file_type: FileType,
    /// Total size of the file in bytes (NOT the size of the selected range).
    pub size: u64,
    /// mtime if measurable (same semantics as `FileVersion.mtime_secs`).
    pub mtime_secs: Option<i64>,
    /// Full-file content hash (SHA-256 hex) — always present for small
    /// files; may be `None` for oversized files (callers then rely on
    /// `range_hash` + metadata identity for fencing).
    pub content_hash: Option<String>,
    /// Hash of exactly the bytes returned for the selected `read_range`
    /// (useful as a lightweight fence for partial reads).
    pub range_hash: Option<String>,
    /// Detected or assumed line-ending convention for text files.
    pub line_ending: LineEnding,
    /// Character encoding if detectable. `None` for non-text types.
    pub encoding: Option<String>,
    /// Wall-clock timestamp when this snapshot was captured. Useful for
    /// replay ordering and "changed between T1 and T2" diagnostics.
    pub read_at: SystemTime,
    /// Which environment (sandbox/remote host) the read was performed
    /// against. Empty string = local host.
    pub environment_id: String,
    /// The `ReadRange` that produced this snapshot. Lets subsequent tools
    /// know what portion of the file they are reasoning about.
    pub read_range: ReadRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
    Cr,
    Mixed,
    /// Non-text file; not applicable.
    NotApplicable,
}

// ── Version fence / stale-file (§12) ──────────────────────────────────────

/// Structured error returned when an edit/patch is attempted against stale
/// content (§12.1). Always includes both the expected and actual hashes so
/// the caller (and model) can formulate a precise "please re-read and try
/// again" response rather than silently fuzzy-matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleFile {
    /// Canonical resource id the fence applies to.
    pub resource_id: String,
    /// Hash the caller expected (from an earlier snapshot / file_version).
    pub expected_hash: Option<String>,
    /// Hash we actually observed when checking right before write.
    pub actual_hash: Option<String>,
    /// If measurable, the wall-clock delta between the read and the
    /// detected stale write. `None` if clocks are unreliable.
    pub changed_since_secs: Option<i64>,
    /// Suggested action — always `reread_and_resample` in V1.
    pub suggested_action: StaleSuggestion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StaleSuggestion {
    #[default]
    RereadAndResample,
}

/// Atomicity promise of a write / patch plan (§10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AtomicityLevel {
    /// All targets on the same filesystem; atomic rename + fsync used for
    /// every target; either ALL succeed or NONE are visible.
    #[default]
    StrictAtomic,
    /// Cross-fs move or unsupported platform; best-effort ordering only,
    /// with explicit manifests written before and after so recovery can
    /// detect a torn write.
    BestEffort,
}

// ── Unified result envelope (§8) ─────────────────────────────────────────

/// Final status of a single tool invocation (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    /// Operation completed fully and within budget.
    Ok,
    /// Operation completed but output was truncated or partially degraded.
    OkWithTruncation,
    /// Operation ran, but the "business" result is a modeled error (e.g.
    /// `exit_code != 0`). This is NOT a runtime-internal error.
    ToolError,
    /// Internal runtime error — the tool was unable to run to completion
    /// (e.g. sandbox denied, missing binary, OOM on our side).
    RuntimeError,
    /// Operation cancelled by the user or by a timeout fence before
    /// completion. Partial output may still be present.
    Cancelled,
}

/// Whether a failed tool call can be safely retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Retryability {
    /// Retry-safe: the operation is idempotent or the caller has a fresh
    /// enough fence that re-running will not double-apply side effects.
    #[default]
    Retryable,
    /// It is unknown whether the operation has already applied effects.
    /// Recovery must reconcile with rollout facts before retrying.
    UnknownOutcome,
    /// Retry would definitely double-apply (e.g. non-idempotent fs move
    /// that has already renamed the source).
    NotRetryable,
    /// A version fence / stale-resource check failed. Safe to retry after
    /// the caller re-reads the target and reconstructs a fresh plan.
    StaleResource,
}

/// Description of output truncation applied to the bounded model view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TruncationInfo {
    /// Original size in bytes before any truncation.
    pub original_bytes: u64,
    /// How many bytes are retained in `model_content` combined.
    pub retained_bytes: u64,
    /// Which retention strategy was used.
    pub strategy: TruncationStrategy,
    /// Count of omitted lines / bytes depending on strategy.
    pub omitted: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TruncationStrategy {
    /// No truncation applied.
    #[default]
    None,
    /// Head-only truncation (retain first N bytes / lines).
    HeadOnly,
    /// Head + tail with omission marker in the middle (§11.4 — default).
    HeadTail,
    /// Whole output moved to an artifact; model_content carries just a ref.
    BlobOnly,
}

/// A single model-visible content fragment inside the envelope (§8).
///
/// Corresponds to one ContextItem at the transcript boundary. Stored as an
/// enum so we can cleanly carry images alongside plain text without union
/// types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelContent {
    /// Plain text (possibly with a bounded prefix / head+tail view).
    Text(String),
    /// Image (e.g. from a PDF-page render).
    Image {
        /// Reference to a stored image (local path / blob url / URI).
        content_ref: String,
        /// Mime type if known.
        mime: Option<String>,
        /// Alt text / caption for the model.
        alt: Option<String>,
    },
}

/// A reference to a full-length artifact stored outside the bounded
/// model view (§8). Always includes a size and a content hash so consumers
/// can verify integrity before dereferencing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Logical artifact type (full-output, patch-diff, raw-binary, …).
    pub kind: String,
    /// How to find the artifact (filesystem path / blob id / URL).
    pub location: String,
    /// Total byte size of the referenced artifact.
    pub size_bytes: u64,
    /// Content hash (alg:value, e.g. "sha256:deadbeef…").
    pub content_hash: String,
    /// Optional human-readable label.
    pub label: Option<String>,
}

/// Description of a resource the invocation changed (§8). Stored in the
/// envelope's `changed_resources` so downstream consumers (diff tracker,
/// rollout, capability snapshots) do not have to guess by grepping strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedResource {
    /// Canonical id matching `FileSnapshot.canonical_resource_id`.
    pub resource_id: String,
    /// Human-readable path.
    pub display_path: PathBuf,
    /// What kind of change happened.
    pub change_type: ChangeType,
    /// Before/after snapshot reference (if the change is a file write).
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeType {
    Created,
    Updated,
    Deleted,
    Moved,
    /// Not a content change, but metadata (chmod, xattr, mtime, …).
    Metadata,
}

/// Description of the contract a built-in tool follows.
///
/// Each built-in tool declares its argument schema, output schema,
/// side-effect classification, and concurrency class. The contract
/// is used for validation, prompt generation, and policy enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltInToolContract {
    /// Tool name (e.g. "read", "edit", "exec").
    pub tool_name: String,
    /// JSON Schema for the tool's arguments.
    pub args_schema: serde_json::Value,
    /// JSON Schema for the tool's output.
    pub output_schema: serde_json::Value,
    /// Side-effect classification.
    pub side_effect: SideEffectClass,
    /// Concurrency classification.
    pub concurrency: ConcurrencyClass,
    /// Whether this tool requires approval before execution.
    pub requires_approval: bool,
    /// Contract version (bumps on schema changes).
    pub contract_version: u64,
    /// Human-readable description for the model.
    pub description: String,
}

use grodex_core::tool::{ConcurrencyClass, SideEffectClass};

/// Unified envelope shared by Read/Edit/Exec/Write/Patch results (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultEnvelope {
    /// Ties this result back to the originating model tool call.
    pub tool_call_id: String,
    /// Matches `OperationId` if this was an idempotent / side-effecting call.
    pub operation_id: Option<String>,
    /// Capability id of the tool that produced this envelope.
    pub capability_id: Option<String>,
    /// Contract version this envelope was produced under; bumps when the
    /// format/model_description/schema changes.
    pub contract_version: u64,
    /// Final status of the invocation.
    pub status: ToolStatus,
    /// Short, stable, model-facing conclusion. Should never exceed ~1-2 sentences.
    pub summary: String,
    /// Bounded content for the model (what actually enters context).
    pub model_content: Vec<ModelContent>,
    /// Structured side-channel data (file snapshots, exit codes, etc.) —
    /// for UI, hooks, and recovery; not shown directly to the model.
    pub structured_data: BTreeMap<String, serde_json::Value>,
    /// References to artifacts that hold the full unbounded output.
    pub artifacts: Vec<ArtifactRef>,
    /// Side-effect facts (changed files / processes spawned).
    pub changed_resources: Vec<ChangedResource>,
    /// Output truncation summary (§8 + §11.4).
    pub truncation: TruncationInfo,
    /// Wall-clock runtime of the invocation.
    pub wall_time: Duration,
    /// Whether a failed call is safe to retry.
    pub retryability: Retryability,
    /// Free-form diagnostics (warnings that didn't rise to error level,
    /// deprecation notices, extractor versions, etc.).
    pub diagnostics: Vec<String>,
}

// ── Head+tail buffer + Exec output (§11.4) ───────────────────────────────

/// Bounded buffer that retains a fixed prefix and suffix of a potentially
/// unbounded byte stream, while counting omitted bytes in the middle
/// (Design Doc 15 §11.4, inherited from Codex).
///
/// Suitable for compilation/test output where the interesting diagnostics
/// live at the very start (env, cmdline) and very end (error summary,
/// failing tests list) and the middle is noisy log spam.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadTailBuffer {
    /// Maximum bytes retained in the head. A single write that exceeds
    /// this capacity is truncated at write time.
    pub head_capacity: usize,
    /// Maximum bytes retained in the tail.
    pub tail_capacity: usize,
    head: Vec<u8>,
    tail: Vec<u8>,
    /// Total bytes observed (for truncation reporting).
    total_seen: u64,
}

impl HeadTailBuffer {
    pub fn new(head_capacity: usize, tail_capacity: usize) -> Self {
        Self {
            head_capacity,
            tail_capacity,
            head: Vec::with_capacity(head_capacity),
            tail: Vec::with_capacity(tail_capacity),
            total_seen: 0,
        }
    }

    /// Default ratio used by the exec tool: keep 24 KiB head, 40 KiB tail.
    pub fn default_exec_ratio() -> Self {
        Self::new(24 * 1024, 40 * 1024)
    }

    /// Append bytes to the buffer. Always O(min(bytes, capacity)).
    pub fn append(&mut self, mut bytes: &[u8]) {
        self.total_seen = self.total_seen.saturating_add(bytes.len() as u64);

        // Fill head first.
        if self.head.len() < self.head_capacity {
            let take = (self.head_capacity - self.head.len()).min(bytes.len());
            self.head.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        if bytes.is_empty() {
            return;
        }

        // Everything after the head is candidate tail. We keep only the
        // last `tail_capacity` bytes, using a rotating copy.
        if bytes.len() >= self.tail_capacity {
            // Fast path: new bytes completely fill the tail.
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - self.tail_capacity..]);
        } else {
            // Append and trim to tail_capacity if needed.
            if self.tail.len() + bytes.len() > self.tail_capacity {
                let overflow = (self.tail.len() + bytes.len()) - self.tail_capacity;
                // Drop oldest `overflow` bytes from tail front.
                self.tail.drain(..overflow);
            }
            self.tail.extend_from_slice(bytes);
        }
    }

    /// Total bytes written to this buffer.
    pub fn total_seen(&self) -> u64 {
        self.total_seen
    }

    /// Bytes retained across head and tail combined (omitted = total_seen - retained).
    pub fn retained_bytes(&self) -> u64 {
        (self.head.len() + self.tail.len()) as u64
    }

    /// Bytes omitted from the middle (0 when the input fit entirely).
    pub fn omitted_bytes(&self) -> u64 {
        self.total_seen.saturating_sub(self.retained_bytes())
    }

    /// Head bytes (never empty when total_seen > 0, unless capacities are 0).
    pub fn head(&self) -> &[u8] {
        &self.head
    }

    /// Tail bytes.
    pub fn tail(&self) -> &[u8] {
        &self.tail
    }

    /// Returns a UTF-8 lossy rendering of head + omission marker + tail,
    /// plus a count of how many lines/bytes were dropped. Used to build
    /// the bounded model view for Exec output.
    ///
    /// The `omission_marker` parameter lets callers insert a stable line
    /// (e.g. `"\n... omitted {n} bytes ...\n"`) that models can reliably
    /// recognise. If `omission_marker` contains the substring `{n}`, it
    /// is replaced with the numeric byte count.
    pub fn to_string_with_marker(&self, omission_marker: &str) -> String {
        let mut out = String::new();
        out.push_str(&String::from_utf8_lossy(&self.head));
        let omitted = self.omitted_bytes();
        if omitted > 0 {
            let marker = if omission_marker.contains("{n}") {
                omission_marker.replace("{n}", &omitted.to_string())
            } else {
                omission_marker.to_string()
            };
            out.push_str(&marker);
        }
        out.push_str(&String::from_utf8_lossy(&self.tail));
        out
    }
}

/// Final structured Exec output (§11.5). Includes both the bounded model
/// view via a HeadTailBuffer and the full output path when applicable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    /// Process exit code (None if still running or killed w/out exit).
    pub exit_code: Option<i32>,
    /// Terminating signal name / number, if platform-visible.
    pub signal: Option<String>,
    /// Elapsed wall time from spawn to observed exit / yield.
    pub wall_time: Duration,
    /// Kernel-assigned pid of the leading process (if still running, see
    /// `ProcessHandle`; if already exited, this is historical pid info).
    pub process_id: Option<u32>,
    /// Combined stdout/stderr retained bounded output.
    pub output_buffer: HeadTailBuffer,
    /// Estimated tokens for the bounded `output_buffer` model view.
    pub original_token_estimate: Option<u64>,
    /// Reference to a full-length capture if output was written to an
    /// artifact blob. `None` when the bounded buffer retained everything.
    pub full_output_ref: Option<ArtifactRef>,
    /// Working directory the command actually ran in (for audit).
    pub cwd: PathBuf,
    /// Final status (running / timed-out / exit-ok / exit-err / cancelled).
    pub status: ExecStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecStatus {
    /// Exit code observed and was 0.
    Success,
    /// Exit code observed and was nonzero (NOT a runtime error — this is
    /// a normal tool result per §11.5 rule 6).
    NonZeroExit,
    /// Timed out at `timeout` or `yield_time`.
    TimedOut,
    /// Command cancelled before completion (SIGINT/SIGKILL/cancel fence).
    Cancelled,
    /// Still running — consumers should follow up via `ProcessHandle`.
    StillRunning,
    /// Spawn failed entirely (missing binary, sandbox deny, bad cwd …).
    SpawnFailed,
}

// ── Long-running process handle (§11.2) ──────────────────────────────────

/// A durable reference to a live process that outlasts the original Exec
/// tool call (Design Doc 15 §11.2). All subsequent interaction with the
/// process (polling, write_stdin, signalling) goes through the unified
/// `process_io` interface; the Exec tool itself never re-encodes these
/// as free-form shell strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessHandle {
    /// Kernel pid at the time the handle was created. PIDs get reused, so
    /// long-lived code MUST also validate `operation_id` + lease expiry
    /// before trusting this value.
    pub process_id: u32,
    /// Idempotency key / operation identifier under which the process
    /// was originally spawned. Cancellation / reconciliation key off this.
    pub operation_id: String,
    /// Environment the process belongs to (e.g. a remote host id).
    pub environment_id: String,
    /// Timestamp when this handle was issued. Used for lease expiry math.
    pub created_at: SystemTime,
    /// Current lifecycle state at the moment the handle was returned.
    pub state: ProcessState,
    /// Whether the process's stdin is still open for writes.
    pub stdin_open: bool,
    /// Whether a PTY was allocated (affects signal routing + EOF handling).
    pub tty: bool,
    /// Timestamp when the process manager will evict this handle if no
    /// polling/write keeps it alive. Callers must renew the lease or the
    /// manager will gracefully terminate and reap the process.
    pub lease_expires_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessState {
    /// Running normally.
    Running,
    /// Sleeping / waiting on I/O.
    Sleeping,
    /// Stop / SIGSTOP received.
    Stopped,
    /// Exited with code.
    Exited(i32),
    /// Terminated by signal.
    Signaled(i32),
    /// Unknown — manager lost track and must probe before acting.
    Unknown,
}

// ── Multi-file apply_patch plan (§10.2) ──────────────────────────────────

/// Atomic multi-file patch plan produced by parsing a Codex-style patch
/// and validating every hunk against actual on-disk content (§10.2).
///
/// `prepare` builds one of these deterministically; `execute` takes the
/// locks, re-validates the version fence, writes temp files, fsyncs,
/// then atomically renames over the targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchPlan {
    /// One entry per file touched by the patch (create / update / delete /
    /// move-source + move-target both appear here).
    pub files: Vec<PatchFile>,
    /// Deterministic hash over (sorted target_resource_ids, hunk contents,
    /// after_hashes). Two PatchPlans that share a `plan_hash` can be
    /// treated as semantically equivalent for approval / dedup purposes.
    pub plan_hash: String,
    /// What atomicity guarantees the executor can actually deliver for
    /// this specific plan.
    pub atomicity: AtomicityLevel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchFile {
    /// Source resource id for move/copy operations. `None` for a plain
    /// create/update/delete.
    pub source_resource_id: Option<String>,
    /// Target resource id this patch file will mutate or create.
    pub target_resource_id: String,
    /// Path-level operation the executor must perform.
    pub operation: PatchOperation,
    /// Expected `FileSnapshot.content_hash` for the *target* prior to the
    /// write. `None` for pure creates.
    pub expected_version_before: Option<String>,
    /// Expected content hash for the *target* AFTER the write. This lets
    /// post-execution verification confirm the write produced exactly
    /// what the plan promised.
    pub after_hash: String,
    /// Per-hunk edit operations (for Update). Order is from-top-of-file to
    /// bottom; the executor will apply in REVERSE (bottom-up) so earlier
    /// hunk byte offsets remain valid while edits stack.
    pub hunks: Vec<PatchHunk>,
    /// Human-readable display path for diagnostics.
    pub target_display_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatchOperation {
    /// Create a new file (target must not exist).
    Add,
    /// Apply hunks to an existing file.
    Update,
    /// Delete the target (target must exist).
    Delete,
    /// Rename source → target (no content changes).
    Move,
    /// Copy source → target.
    Copy,
}

/// A single patch hunk inside a `PatchFile::Update`.
///
/// Equivalent to a unified-diff @@ block. Context lines are always the
/// exact bytes the prepare step observed, so during execute a second
/// fence validation can refuse if the context drifted post-approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchHunk {
    /// 0-based start byte offset in the BEFORE file.
    pub before_start: u64,
    /// Count of bytes consumed from the BEFORE file.
    pub before_length: u64,
    /// 0-based start byte offset in the AFTER file (informational).
    pub after_start: u64,
    /// Count of bytes written into the AFTER file.
    pub after_length: u64,
    /// Exact bytes to match at `before_start` (a substring of the BEFORE
    /// file, typically context + removed lines).
    pub context_and_removed: Vec<u8>,
    /// Exact bytes to write in their place (typically context + added lines).
    pub context_and_added: Vec<u8>,
}
