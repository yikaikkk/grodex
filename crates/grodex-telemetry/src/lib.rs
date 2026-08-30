//! grodex-telemetry — runtime-observation layer ("运行观测层").
//!
//! Architecture (Design Doc: 单机可观测系统):
//!
//! ```text
//!   业务正确性层                     运行观测层
//!   RolloutWriter ──► rollout.jsonl   TelemetrySink ──► telemetry.db (SQLite WAL)
//!   恢复 / 审计 / 重放                查询 / 统计 / 诊断
//! ```
//!
//! Core principles:
//! 1. `rollout.jsonl` is the source of truth — telemetry is a query projection.
//! 2. Telemetry write failures MUST NOT affect the Agent Loop or Turn results.
//! 3. Every record carries `journal_seq` so the projection can be rebuilt
//!    from the journal after a crash.
//!
//! This crate has no dependency on other grodex crates: the writer converts
//! journal events into [`TelemetryRecord`]s and the sink persists them.

pub mod query;
mod record;
mod schema;
mod sqlite;

pub use record::{
    bound_payload, kind, NoopTelemetrySink, Sensitivity, Severity, TelemetryRecord, TelemetrySink,
};
pub use schema::SCHEMA_VERSION;
pub use query::{
    cache_stats, doctor, errors, projection_cursor, recovery_anomalies, session_turns, sessions,
    slow_models, slow_tools, timeline, turn, CacheStatsRow, DoctorReport, ErrorRow, ModelAgg,
    RecoveryRow, SessionRow, TimelineRow, ToolAgg, TurnRow,
};
pub use sqlite::{retain, SqliteTelemetrySink, TelemetryGuard};
