//! Grodex Protocol — ACP-based frontend event and command types.
//!
//! The protocol layer uses standard ACP message types for interop with
//! ACP-compatible clients (Zed, etc.) and `x-agent/v2` extensions for
//! concepts that ACP cannot natively express.

pub mod acp;
pub mod extensions;
pub mod transport;

// ── Convenience re-exports (ACP V2 additions: B1–B5) ──────────────

pub use acp::{
    // B1: Command envelope + ResolveApproval
    ApprovalResolution, Command, ResolveApprovalCommand,
    // B2: ResumeSession + replay + back-pressure
    AckBucket, ReplayCursor, ReplayMode, ResumeSessionCommand,
    // B3: Item lifecycle
    ItemKind, SessionLifecycleEvent,
    // B5: Agent-initiated permission request event
    RequestPermissionPayload,
    // Canonical alias: SessionEvent ≡ per-event payload (UpdateContent)
    SessionEvent,
};

// ── Re-exports of well-used existing types ─────────────────────────

pub use acp::{
    ApprovalResolutionKind, ClientInfo, CommandMeta, EventEnvelope, InitializeRequest,
    InitializeResponse, ResolveApprovalRequest, ServerInfo, SessionAck, SessionCancel,
    SessionLoadRequest, SessionNewRequest, SessionNewResponse, SessionPrompt,
    SessionResumeRequest, SessionSnapshotPayload, SessionUpdate, SnapshotItem, UpdateContent,
};

pub use transport::{ClientFrame, ServerFrame};
