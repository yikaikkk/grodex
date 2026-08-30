//! Telemetry record + sink trait.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Sensitivity classification for a telemetry record — mirrors
/// `grodex_rollout::event::SensitivityLevel` without a crate dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sensitivity {
    /// Safe to include in logs and debug output.
    Normal,
    /// Contains credentials or secrets — must not appear in debug logs.
    Credential,
    /// Contains personally identifiable information.
    Personal,
}

impl Sensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Sensitivity::Normal => "normal",
            Sensitivity::Credential => "credential",
            Sensitivity::Personal => "personal",
        }
    }
}

/// Record priority. Low-priority (debug) records are dropped first when
/// the sink queue is full; critical lifecycle records should never be
/// emitted at debug severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Severity {
    Debug,
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Debug => "debug",
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

/// Stable `kind` strings used by the projection layer. The writer
/// (grodex-loop) maps each `RolloutEventType` to one of these.
pub mod kind {
    pub const SESSION_STARTED: &str = "session_started";
    pub const TURN_STARTED: &str = "turn_started";
    pub const TURN_COMPLETED: &str = "turn_completed";
    pub const USER_INPUT: &str = "user_input";
    pub const MODEL_ITEM: &str = "model_item";
    pub const MODEL_ATTEMPT_STARTED: &str = "model_attempt_started";
    pub const MODEL_ATTEMPT_FINISHED: &str = "model_attempt_finished";
    pub const MODEL_ROUTE_EVENT: &str = "model_route_event";
    pub const PROMPT_SNAPSHOT: &str = "prompt_snapshot";
    pub const TOOL_PREPARED: &str = "tool_prepared";
    pub const TOOL_APPROVED: &str = "tool_approved";
    pub const TOOL_STARTED: &str = "tool_started";
    pub const TOOL_FINISHED: &str = "tool_finished";
    pub const TOOL_RESULT_COMMITTED: &str = "tool_result_committed";
    pub const TOOL_INDETERMINATE: &str = "tool_indeterminate";
    pub const TOOL_RESOLVED: &str = "tool_resolved";
    pub const APPROVAL_REQUESTED: &str = "approval_requested";
    pub const APPROVAL_RESOLVED: &str = "approval_resolved";
    pub const LEASE_ISSUED: &str = "lease_issued";
    pub const LEASE_CONSUMED: &str = "lease_consumed";
    pub const LEASE_EXPIRED: &str = "lease_expired";
    pub const COMPACTION_STARTED: &str = "compaction_started";
    pub const COMPACTION_CANDIDATE: &str = "compaction_candidate_built";
    pub const COMPACTION_COMMITTED: &str = "compaction_committed";
    pub const COMPACTION_FAILED: &str = "compaction_failed";
    pub const SKILL_SNAPSHOT: &str = "skill_snapshot";
    pub const SUBAGENT_STARTED: &str = "subagent_started";
    pub const SUBAGENT_FINISHED: &str = "subagent_finished";
    pub const CONTEXT_RESTORED: &str = "context_restored";
    pub const STATE_CHANGED: &str = "state_changed";
    pub const PROJECTION_PRUNED: &str = "projection_pruned";
    pub const CAPABILITY_PROMOTED: &str = "capability_promoted";
    pub const CAPABILITY_REJECTED_STALE: &str = "capability_rejected_stale";
    pub const EFFECTIVE_REVISION_CREATED: &str = "effective_revision_created";
    pub const APP_ONLY_TOOL_CALL: &str = "app_only_tool_call";
    // Out-of-band kinds (not journaled; emitted directly to the sink).
    pub const MEMORY_RETRIEVAL: &str = "memory_retrieval";
    pub const MCP_LIFECYCLE: &str = "mcp_lifecycle";
}

/// One telemetry observation. Journal-derived records use a
/// deterministic `event_id` (`"{session_id}:{seq}"`) so re-projection is
/// idempotent; out-of-band records use a uuid v4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryRecord {
    pub event_id: String,
    pub run_id: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub step_id: Option<String>,
    pub call_id: Option<String>,
    pub journal_seq: Option<u64>,
    pub kind: String,
    pub status: Option<String>,
    pub severity: Severity,
    pub occurred_at: DateTime<Utc>,
    pub duration_ms: Option<u64>,
    pub payload_json: String,
    pub sensitivity: Sensitivity,
}

impl TelemetryRecord {
    /// Deterministic record for a journaled event — re-projecting the
    /// same journal seq produces the same `event_id`, so
    /// `INSERT OR IGNORE` keeps the projection idempotent.
    #[allow(clippy::too_many_arguments)]
    pub fn from_journal(
        session_id: &str,
        seq: u64,
        run_id: &str,
        turn_id: Option<&str>,
        step_id: Option<&str>,
        call_id: Option<&str>,
        kind: &str,
        occurred_at: DateTime<Utc>,
    ) -> Self {
        Self {
            event_id: format!("{session_id}:{seq}"),
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: turn_id.map(str::to_string),
            step_id: step_id.map(str::to_string),
            call_id: call_id.map(str::to_string),
            journal_seq: Some(seq),
            kind: kind.to_string(),
            status: None,
            severity: Severity::Info,
            occurred_at,
            duration_ms: None,
            payload_json: "{}".into(),
            sensitivity: Sensitivity::Normal,
        }
    }

    /// Out-of-band record (not derived from the journal).
    pub fn out_of_band(run_id: &str, session_id: &str, kind: &str) -> Self {
        Self {
            event_id: uuid::Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            turn_id: None,
            step_id: None,
            call_id: None,
            journal_seq: None,
            kind: kind.to_string(),
            status: None,
            severity: Severity::Info,
            occurred_at: Utc::now(),
            duration_ms: None,
            payload_json: "{}".into(),
            sensitivity: Sensitivity::Normal,
        }
    }
}

/// Sink trait — business code must never depend on SQL. `emit` MUST be
/// non-blocking; telemetry failures never propagate to the caller.
pub trait TelemetrySink: Send + Sync + 'static {
    /// Fire-and-forget emit. Never blocks for long; never errors.
    fn emit(&self, record: TelemetryRecord);

    /// Batch ingest for journal re-projection. Blocking is acceptable
    /// here (startup path only). Returns the number of records accepted.
    fn ingest(&self, records: Vec<TelemetryRecord>) -> usize;

    /// Flush queued records to durable storage. Blocking, bounded.
    fn flush(&self);
}

/// Sink that does nothing — used when telemetry is disabled or the
/// database cannot be opened (fail-open).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTelemetrySink;

impl TelemetrySink for NoopTelemetrySink {
    fn emit(&self, _record: TelemetryRecord) {}
    fn ingest(&self, records: Vec<TelemetryRecord>) -> usize {
        records.len()
    }
    fn flush(&self) {}
}

/// Cap a payload at [`MAX_PAYLOAD_BYTES`] — oversized payloads (e.g.
/// `ToolExecutionFinished` carrying a full tool result) are replaced by
/// a truncation marker so the telemetry DB never stores huge blobs.
/// Full content remains in the journal / blob store.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

pub fn bound_payload(value: &serde_json::Value) -> String {
    let full = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    if full.len() <= MAX_PAYLOAD_BYTES {
        return full;
    }
    let original = full.len();
    // Take a prefix of the serialized JSON and re-close it into valid JSON.
    // JSON escaping (\n, \", \uXXXX for control chars) can inflate the
    // preview well past its byte budget, so the preview length shrinks
    // until the serialized marker actually fits the cap.
    let budget = MAX_PAYLOAD_BYTES / 2;
    let mut cut = budget;
    let marker;
    loop {
        while cut > 0 && !full.is_char_boundary(cut) {
            cut -= 1;
        }
        let candidate = serde_json::json!({
            "truncated": true,
            "original_bytes": original,
            "preview": &full[..cut],
        });
        let serialized = serde_json::to_string(&candidate)
            .unwrap_or_else(|_| "{\"truncated\":true}".into());
        if serialized.len() <= MAX_PAYLOAD_BYTES || cut == 0 {
            marker = serialized;
            break;
        }
        cut = cut * 3 / 4;
    }
    marker
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payload_passes_through() {
        let v = serde_json::json!({"a": 1});
        assert_eq!(bound_payload(&v), r#"{"a":1}"#);
    }

    #[test]
    fn oversized_payload_becomes_marker() {
        let big = "x".repeat(MAX_PAYLOAD_BYTES * 2);
        let v = serde_json::json!({"content": big});
        let out = bound_payload(&v);
        assert!(out.len() <= MAX_PAYLOAD_BYTES, "marker must fit the cap, got {}", out.len());
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["truncated"], serde_json::json!(true));
        assert!(parsed["original_bytes"].as_u64().unwrap() > MAX_PAYLOAD_BYTES as u64);
    }

    #[test]
    fn journal_event_ids_are_deterministic() {
        let a = TelemetryRecord::from_journal("sess", 7, "run", None, None, None, kind::TURN_STARTED, Utc::now());
        let b = TelemetryRecord::from_journal("sess", 7, "run", None, None, None, kind::TURN_STARTED, Utc::now());
        assert_eq!(a.event_id, b.event_id);
        assert_eq!(a.event_id, "sess:7");
    }
}
