//! Standard ACP (Agent Client Protocol) message types.
//!
//! These map directly to the ACP specification, providing interoperability
//! with Zed and other ACP-compatible clients.

use grodex_core::id::SessionId;
use serde::{Deserialize, Serialize};

/// Initialization handshake sent by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeRequest {
    /// ACP protocol version range the client supports.
    pub protocol_versions: Vec<String>,
    /// Client identification (e.g. "zed", "grodex-tui", "grodex-desktop").
    pub client_info: ClientInfo,
    /// Optional authentication token.
    pub auth_token: Option<String>,
    /// Client capabilities bitmap / feature set.
    pub client_capabilities: serde_json::Value,
}

/// Metadata about the connecting client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// Initialization response from the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResponse {
    /// Selected protocol version.
    pub protocol_version: String,
    /// Agent identification.
    pub server_info: ServerInfo,
    /// Agreed-upon capabilities.
    pub capabilities: serde_json::Value,
}

/// Metadata about the agent server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Create a new session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNewRequest {
    /// Optional working directory.
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNewResponse {
    pub session_id: SessionId,
}

/// Load an existing session by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLoadRequest {
    pub session_id: SessionId,
}

/// User input (text or command) sent to the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPrompt {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub session_id: SessionId,
    pub text: String,
}

/// Steer an in-progress Turn: the user adds/changes the goal while the
/// agent is still streaming. The agent cancels the current Turn and
/// starts a new one seeded with the steering text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSteer {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub session_id: SessionId,
    pub text: String,
}

/// Cancel the current operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCancel {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub session_id: SessionId,
}

// ── Session snapshot payload (Design Doc 17 §8.3) ─────────────────

/// A snapshot of the session state for resync/reconnection.
///
/// Contains the minimal state the UI needs to rebuild its view without
/// replaying every event from seq 0. Sent in response to `session/resume`
/// or when the client signals it lost events (gap detection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshotPayload {
    /// The session this snapshot represents.
    pub session_id: SessionId,
    /// The seq of the last event included in this snapshot.
    pub last_seq: u64,
    /// The current capability generation.
    pub generation: u64,
    /// Current Turn id (None if no Turn is active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_id: Option<String>,
    /// Items the UI should display (text blocks, tool calls, results).
    pub items: Vec<SnapshotItem>,
}

/// One item in a session snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotItem {
    pub item_id: String,
    pub item_type: String,
    /// The content (may be partial if the item is still streaming).
    pub content: String,
    /// Whether this item is complete or still being streamed.
    pub complete: bool,
}

// ── B1 / B4: Command + ResolveApproval ────────────────────────────

/// Top-level command envelope dispatched from the client to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Command {
    Prompt(SessionPrompt),
    /// Modify the in-progress Turn mid-stream (agent cancels + restarts).
    Steer(SessionSteer),
    Cancel(SessionCancel),
    ResolveApproval(ResolveApprovalCommand),
    ResumeSession(ResumeSessionCommand),
    /// Resolve an Indeterminate tool call discovered during crash recovery.
    ResolveIndeterminate(ResolveIndeterminateCommand),
}

/// The client's resolution of an approval ticket.
///
/// Equivalent to `ApprovalResolution` in `grodex-permission`, kept as a
/// separate type to avoid a cross-crate dependency in the protocol layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolution {
    Allow,
    /// Approve this call AND grant it for the rest of the session
    /// ("always allow"). The agent mints a `SessionPolicyGrant` keyed to
    /// the tool so later `check()` calls fast-path to Allowed.
    AlwaysAllow,
    Narrow { narrowed_args: serde_json::Value },
    Deny,
    Cancel,
}

/// ResolveApproval command — client resolves a pending approval ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveApprovalCommand {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub ticket_id: String,
    pub resolution: ApprovalResolution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_by: Option<String>,
    pub issued_at_ms: u64,
}

/// Human adjudication for an Indeterminate tool call (protocol-level).
///
/// Mirrors `IndeterminateResolution` in `grodex-loop::command`, kept as a
/// separate type to avoid a cross-crate dependency in the protocol layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndeterminateResolution {
    /// The user confirmed the side effect completed successfully.
    Succeeded,
    /// The user confirmed the side effect failed or was partial.
    Failed,
    /// Discard the indeterminate call; the model will re-issue it.
    Retry,
}

/// ResolveIndeterminate command — client resolves an indeterminate tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveIndeterminateCommand {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub call_id: String,
    pub resolution: IndeterminateResolution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

// ── Legacy approval types (kept for backward compat) ──────────────

/// Legacy: kept for backward compat; new code uses `ResolveApprovalCommand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveApprovalRequest {
    pub session_id: SessionId,
    pub ticket_id: String,
    pub resolution: ApprovalResolutionKind,
}

/// Legacy: kept for backward compat; new code uses `ApprovalResolution`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResolutionKind {
    Allow,
    Narrow { narrowed_args: serde_json::Value },
    Deny,
    Cancel,
}

// ── B2: ResumeSession + ReplayCursor + AckBucket ──────────────────

/// ResumeSession command — client requests replay after a disconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSessionCommand {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub session_id: String,
    pub resume_from: ReplayCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_bucket: Option<AckBucket>,
}

/// Cursor describing where replay should start from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayCursor {
    pub last_consumed_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<String>,
    pub mode: ReplayMode,
}

/// Replay strategy requested by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMode {
    CatchUp,
    LiveOnly,
    SnapshotThenLive,
}

/// Client receive-capability declaration for back-pressure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckBucket {
    pub max_inflight_events: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_pause_ms: Option<u32>,
}

// ── Legacy resume types (kept for backward compat) ────────────────

/// Legacy: kept for backward compat; new code uses `ResumeSessionCommand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeRequest {
    pub session_id: SessionId,
    pub last_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// Legacy ACK (kept for backward compat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAck {
    pub session_id: SessionId,
    pub acked_seq: u64,
}

// ── Command metadata (legacy, kept for backward compat) ───────────

/// Legacy: metadata attached to every command for idempotency and
/// generation fencing. New commands carry these fields inline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CommandMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

// ── B3: Item lifecycle events + B5: RequestPermission ─────────────

/// Rich item-lifecycle event (replaces the flat ItemStarted/ItemAborted/
/// ItemReplacement variants). Provides parent linkage, typed ItemKind,
/// and wall-clock timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionLifecycleEvent {
    ItemStarted {
        item_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_item_id: Option<String>,
        kind: ItemKind,
        started_at_ms: u64,
    },
    ItemAborted {
        item_id: String,
        reason: String,
        aborted_at_ms: u64,
    },
    ItemReplacement {
        superseded_item_id: String,
        replacement_item_id: String,
        replaced_at_ms: u64,
    },
}

/// Classification of a stream item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    TextDelta,
    ToolCall,
    Reasoning,
    Summary,
    Other(String),
}

// ── B5: RequestPermission (agent → client event) ──────────────────

/// Agent-initiated request for the client to surface a permission dialog.
///
/// Emitted as a SessionEvent (not a Command) — the agent pushes it, the
/// client responds later via `ResolveApprovalCommand`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPermissionPayload {
    pub ticket_id: String,
    pub tool_name: String,
    pub summary: String,
    pub risk: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_snapshot: Option<serde_json::Value>,
    pub timeout_remaining_ms: u64,
}

// ── SessionEvent / UpdateContent ──────────────────────────────────

/// Streaming update from the agent to the client during a Turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUpdate {
    pub session_id: SessionId,
    pub content: UpdateContent,
}

/// Alias: SessionEvent is the canonical name for per-event payloads.
pub type SessionEvent = UpdateContent;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UpdateContent {
    /// A chunk of assistant text.
    TextDelta { text: String },
    /// A chunk of reasoning / thinking.
    ThoughtDelta { text: String },
    /// A tool call is starting.
    ToolCallStart { call_id: String, name: String },
    /// Arguments are arriving incrementally.
    ToolCallArgs { call_id: String, args_delta: String },
    /// A tool call has completed streaming.
    ToolCallEnd { call_id: String },
    /// A tool result is available.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// Sub-agent lifecycle/progress event. The TUI renders each
    /// sub-agent as a collapsible card: `phase = "started"` opens the
    /// card, `"step"` appends an internal execution line, `"finished"`
    /// closes it (`ok = false` marks failure).
    SubagentProgress {
        id: String,
        label: String,
        phase: String,
        detail: String,
        ok: Option<bool>,
    },
    /// The Turn has completed. `cached_tokens` is the prompt-cache hit
    /// subset of `input_tokens` (0 when usage is unavailable).
    TurnComplete {
        turn_id: String,
        input_tokens: u64,
        cached_tokens: u64,
    },
    /// A non-final error occurred.
    Error { message: String },
    /// Informational log (success confirmations, diagnostics, resume
    /// reports, …). Render this as a one-line log entry rather than a
    /// scary red error card. Clients that do not surface Info can fall
    /// back to treating it as TextDelta so the user still sees the text.
    Info { message: String },
    /// Context compaction lifecycle. The client shows a transient
    /// "会话压缩中…" indicator while `phase == "started"` and clears it on
    /// `"finished"` / `"failed"`. Compaction runs an extra model
    /// round-trip, so without this the UI looks frozen mid-turn.
    CompactionStatus { phase: String },

    // ── Legacy item lifecycle (flat form, kept for backward compat) ─
    ItemStarted { item_id: String, item_type: String },
    ItemAborted { item_id: String, reason: String },
    ItemReplacement { item_id: String, replaces: String },

    // ── B3: Rich session-lifecycle event ────────────────────────────
    SessionLifecycle(SessionLifecycleEvent),

    // ── B5: Agent asks client for permission ────────────────────────
    RequestPermission(RequestPermissionPayload),

    // ── Session snapshot (Design Doc 17 §8.3) ────────────────────
    /// A full snapshot of the session state — sent on initial connect,
    /// reconnection, or when the client requests a resync. Contains
    /// enough state for the UI to rebuild without replaying every event.
    SessionSnapshot { snapshot: SessionSnapshotPayload },

    // ── Indeterminate tool call (crash recovery) ──────────────────
    /// A tool call was in-flight when the session crashed. The side
    /// effect state is unknown — the user must inspect the real-world
    /// result and resolve as Succeeded, Failed, or Retry via
    /// `Command::ResolveIndeterminate`.
    IndeterminateToolCall {
        call_id: String,
        tool_name: String,
        message: String,
    },
}

// ── EventEnvelope (with ack_ref from B2) ─────────────────────────

/// Unified event envelope (Design Doc 17 §7).
///
/// The bare `SessionUpdate` carries only `session_id` + `content`. The
/// envelope wraps it with the trace/causality fields required for replay,
/// ordering, and back-pressure: a monotonic `seq`, a stable `event_id`, the
/// `parent_event_id` it builds on, a `causation_token` linking a tool-result
/// event to the tool-call event that caused it, and the `generation` (the
/// capability/turn generation the event was produced under — invariant #14).
///
/// Clients that understand the envelope use `seq` for gap detection and
/// `parent_event_id`/`causation_token` for incremental UI stitching and
/// crash-resume ("resume from seq N"). The fields are all optional-friendly
/// so a partial envelope still deserializes from older producers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Monotonic per-session sequence number (gap-free on the producer side;
    /// a client seeing a gap knows it missed events and must resync).
    pub seq: u64,
    /// Stable unique id for this event (UUID v4 string).
    pub event_id: String,
    /// The event this one builds on (e.g. a ToolCallEnd's parent is the
    /// ToolCallStart). None for root events (UserInput, TurnStart).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<String>,
    /// Links a *result* event to the *call* event that caused it
    /// (ToolResult → ToolCallStart). Lets the UI pair them deterministically
    /// even when interleaved with unrelated events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_token: Option<String>,
    /// Capability/turn generation active when the event was emitted. A
    /// late-arriving event whose generation regressed is rejected by replay
    /// (invariant #14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// B2: reference this event in an ACK for fine-grained back-pressure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_ref: Option<String>,
    /// Session this event belongs to (carried through from the SessionUpdate).
    pub session_id: SessionId,
    /// The wrapped content payload.
    pub content: UpdateContent,
}

impl EventEnvelope {
    /// Wrap a bare `UpdateContent` into an envelope, assigning fresh seq/id.
    /// `next_seq` should be the session's monotonic counter; the caller owns
    /// it because the transport may multiplex several sessions.
    pub fn wrap(
        next_seq: u64,
        session_id: SessionId,
        content: UpdateContent,
    ) -> Self {
        Self {
            seq: next_seq,
            event_id: format!("evt_{}", uuid::Uuid::new_v4()),
            parent_event_id: None,
            causation_token: None,
            generation: None,
            ack_ref: None,
            session_id,
            content,
        }
    }

    /// Builder: set the parent event this one builds on.
    pub fn with_parent(mut self, parent_event_id: impl Into<String>) -> Self {
        self.parent_event_id = Some(parent_event_id.into());
        self
    }

    /// Builder: set the causation token linking this event to a cause event.
    pub fn with_causation(mut self, token: impl Into<String>) -> Self {
        self.causation_token = Some(token.into());
        self
    }

    /// Builder: stamp the capability/turn generation onto the event.
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = Some(generation);
        self
    }

    /// Builder (B2): set the ACK ref so the client can ACK this specific event.
    pub fn with_ack_ref(mut self, ack_ref: impl Into<String>) -> Self {
        self.ack_ref = Some(ack_ref.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid() -> SessionId {
        SessionId::new()
    }

    #[test]
    fn envelope_assigns_monotonic_seq_and_id() {
        let s = sid();
        let e1 = EventEnvelope::wrap(1, s, UpdateContent::TextDelta { text: "a".into() });
        let e2 = EventEnvelope::wrap(2, s, UpdateContent::TextDelta { text: "b".into() });
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_ne!(e1.event_id, e2.event_id, "each event gets a fresh id");
        assert!(e1.parent_event_id.is_none());
        assert!(e1.causation_token.is_none());
        assert!(e1.generation.is_none(), "generation is opt-in");
        assert!(e1.ack_ref.is_none(), "ack_ref is opt-in");
    }

    #[test]
    fn envelope_builders_set_causality_fields() {
        let s = sid();
        let e = EventEnvelope::wrap(7, s, UpdateContent::ToolResult {
            call_id: "c1".into(),
            content: "ok".into(),
            is_error: false,
        })
        .with_parent("evt_start")
        .with_causation("call_c1")
        .with_generation(3)
        .with_ack_ref("ack-7");
        assert_eq!(e.parent_event_id.as_deref(), Some("evt_start"));
        assert_eq!(e.causation_token.as_deref(), Some("call_c1"));
        assert_eq!(e.generation, Some(3));
        assert_eq!(e.ack_ref.as_deref(), Some("ack-7"));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let s = sid();
        let e = EventEnvelope::wrap(1, s, UpdateContent::TurnComplete { turn_id: "t1".into(), input_tokens: 0, cached_tokens: 0 })
            .with_generation(2);
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"seq\":1"));
        assert!(json.contains("\"generation\":2"));
        assert!(!json.contains("parent_event_id"));
        assert!(!json.contains("causation_token"));
        assert!(!json.contains("ack_ref"));

        let back: EventEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 1);
        assert_eq!(back.generation, Some(2));
        assert!(back.parent_event_id.is_none());
        assert!(back.ack_ref.is_none());
    }
}
