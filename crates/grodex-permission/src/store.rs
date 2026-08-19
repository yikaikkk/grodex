//! TicketStore — SQLite persistence wrapper for ApprovalTicket.
//!
//! Encapsulates all rusqlite usage so that broker.rs remains free of
//! SQLite trait bounds. Fail-closed: if DB operations fail the store
//! returns errors; the broker degrades gracefully to pure memory.

use crate::schema::apply_schema;
use crate::ticket::{ApprovalTicket, RiskLevel, TicketStatus};
use grodex_core::id::ToolCallId;
use grodex_core::policy::PolicyDecision;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde_json error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid persisted value: {0}")]
    InvalidValue(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

impl std::fmt::Debug for TicketStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketStore")
            .field("db_path", &self.db_path)
            .finish()
    }
}

pub struct TicketStore {
    #[allow(dead_code)]
    db_path: PathBuf,
    conn: Connection,
}

impl TicketStore {
    pub fn new<P: AsRef<Path>>(path: P) -> StoreResult<Self> {
        let path_buf = path.as_ref().to_path_buf();
        if let Some(parent) = path_buf.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    StoreError::InvalidValue(format!("failed to create db dir: {e}"))
                })?;
            }
        }
        let conn = Connection::open(&path_buf)?;
        apply_schema(&conn)?;
        Ok(Self {
            db_path: path_buf,
            conn,
        })
    }

    pub fn new_in_memory() -> StoreResult<Self> {
        let conn = Connection::open_in_memory()?;
        apply_schema(&conn)?;
        Ok(Self {
            db_path: PathBuf::from(":memory:"),
            conn,
        })
    }

    pub fn upsert_ticket(&self, ticket: &ApprovalTicket) -> StoreResult<()> {
        let created_at_ms = instant_to_unix_ms(ticket.created_at);
        let timeout_ms = ticket.timeout.as_millis() as i64;
        let risk_level = ticket.risk_level.as_str();
        let status = ticket.status.as_str();
        let decision_tx_marker = if ticket.decision_tx.is_some() {
            Some("in-memory-only".to_string())
        } else {
            None
        };
        let policy_decision_str = ticket.policy_decision.map(|d| match d {
            PolicyDecision::Allow => "Allow".to_string(),
            PolicyDecision::Ask => "Ask".to_string(),
            PolicyDecision::Deny => "Deny".to_string(),
        });
        let arguments_snapshot = ticket
            .arguments_snapshot
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()?;
        let policy_rule_matches = ticket
            .policy_rule_matches
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()?;

        self.conn.execute(
            r#"
            INSERT INTO approval_tickets (
                ticket_id, tool_call_id, tool_name, summary, risk_level, status,
                decision_tx_serialized, policy_decision, created_at, timeout_ms,
                arguments_snapshot, policy_rule_matches, granted_by,
                session_id, source_agent_id, task_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(ticket_id) DO UPDATE SET
                tool_call_id = excluded.tool_call_id,
                tool_name = excluded.tool_name,
                summary = excluded.summary,
                risk_level = excluded.risk_level,
                status = excluded.status,
                decision_tx_serialized = excluded.decision_tx_serialized,
                policy_decision = excluded.policy_decision,
                created_at = excluded.created_at,
                timeout_ms = excluded.timeout_ms,
                arguments_snapshot = excluded.arguments_snapshot,
                policy_rule_matches = excluded.policy_rule_matches,
                granted_by = excluded.granted_by,
                session_id = excluded.session_id,
                source_agent_id = excluded.source_agent_id,
                task_id = excluded.task_id
            "#,
            rusqlite::params![
                ticket.ticket_id,
                ticket.tool_call_id.to_string(),
                ticket.tool_name,
                ticket.summary,
                risk_level,
                status,
                decision_tx_marker,
                policy_decision_str,
                created_at_ms as i64,
                timeout_ms,
                arguments_snapshot,
                policy_rule_matches,
                ticket.granted_by,
                ticket.session_id,
                ticket.source_agent_id,
                ticket.task_id,
            ],
        )?;
        Ok(())
    }

    pub fn get_pending_tickets(&self) -> StoreResult<Vec<ApprovalTicket>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                ticket_id, tool_call_id, tool_name, summary, risk_level, status,
                decision_tx_serialized, policy_decision, created_at, timeout_ms,
                arguments_snapshot, policy_rule_matches, granted_by,
                session_id, source_agent_id, task_id
            FROM approval_tickets
            WHERE status = 'Pending'
            "#,
        )?;

        #[allow(clippy::type_complexity)]
        let mut rows: Vec<(
            String, String, String, String, String, String,
            Option<String>, Option<String>, i64, i64,
            Option<String>, Option<String>, Option<String>,
            Option<String>, Option<String>, Option<String>,
        )> = Vec::new();

        let mapped = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<String>>(15)?,
            ))
        })?;

        for r in mapped {
            rows.push(r?);
        }

        let mut tickets = Vec::new();
        for raw in rows {
            tickets.push(raw_row_to_ticket(raw)?);
        }
        Ok(tickets)
    }

    pub fn update_status(
        &self,
        ticket_id: &str,
        status: TicketStatus,
        decision: Option<PolicyDecision>,
        granted_by: Option<&str>,
    ) -> StoreResult<bool> {
        let status_str = status.as_str();
        let decision_str = decision.map(|d| match d {
            PolicyDecision::Allow => "Allow".to_string(),
            PolicyDecision::Ask => "Ask".to_string(),
            PolicyDecision::Deny => "Deny".to_string(),
        });
        let granted_by_owned = granted_by.map(|s| s.to_string());

        let changes = self.conn.execute(
            r#"
            UPDATE approval_tickets
            SET status = ?1,
                policy_decision = ?2,
                decision_tx_serialized = NULL,
                granted_by = COALESCE(?3, granted_by)
            WHERE ticket_id = ?4
            "#,
            rusqlite::params![
                status_str,
                decision_str,
                granted_by_owned,
                ticket_id,
            ],
        )?;
        Ok(changes > 0)
    }

    /// Persist a narrowed arguments snapshot for a ticket. Used by
    /// ResolveApproval when the frontend returns `narrowed_args`, so
    /// a subsequent session resume rebuilds the ticket with the
    /// narrowed args instead of the original model-issued ones.
    pub fn update_arguments_snapshot(
        &self,
        ticket_id: &str,
        args: &serde_json::Value,
    ) -> StoreResult<bool> {
        let serialized = serde_json::to_string(args).map_err(|e| {
            StoreError::InvalidValue(format!("cannot serialize narrowed args: {e}"))
        })?;
        let changes = self.conn.execute(
            r#"
            UPDATE approval_tickets
            SET arguments_snapshot = ?1
            WHERE ticket_id = ?2
            "#,
            rusqlite::params![serialized, ticket_id],
        )?;
        Ok(changes > 0)
    }

    pub fn delete_ticket(&self, ticket_id: &str) -> StoreResult<bool> {
        let changes = self
            .conn
            .execute("DELETE FROM approval_tickets WHERE ticket_id = ?1", rusqlite::params![ticket_id])?;
        Ok(changes > 0)
    }

    pub fn cancel_all_pending(&self) -> StoreResult<usize> {
        let changes = self.conn.execute(
            r#"
            UPDATE approval_tickets
            SET status = 'Cancelled',
                policy_decision = 'Deny',
                decision_tx_serialized = NULL
            WHERE status = 'Pending'
            "#,
            [],
        )?;
        Ok(changes)
    }
}

#[allow(clippy::type_complexity)]
fn raw_row_to_ticket(
    raw: (
        String, String, String, String, String, String,
        Option<String>, Option<String>, i64, i64,
        Option<String>, Option<String>, Option<String>,
        Option<String>, Option<String>, Option<String>,
    ),
) -> StoreResult<ApprovalTicket> {
    let (
        ticket_id, tool_call_id_str, tool_name, summary, risk_level_str, status_str,
        _decision_tx_marker, policy_decision_str, created_at_ms, timeout_ms,
        arguments_snapshot_str, policy_rule_matches_str, granted_by,
        session_id, source_agent_id, task_id,
    ) = raw;

    let risk_level = RiskLevel::from_str(&risk_level_str).ok_or_else(|| {
        StoreError::InvalidValue(format!("invalid risk_level: {risk_level_str}"))
    })?;
    let status = TicketStatus::from_persistence_str(&status_str).ok_or_else(|| {
        StoreError::InvalidValue(format!("invalid status: {status_str}"))
    })?;
    let policy_decision = policy_decision_str
        .as_deref()
        .map(|s| match s {
            "Allow" => Ok(PolicyDecision::Allow),
            "Ask" => Ok(PolicyDecision::Ask),
            "Deny" => Ok(PolicyDecision::Deny),
            other => Err(StoreError::InvalidValue(format!(
                "invalid policy_decision: {other}"
            ))),
        })
        .transpose()?;
    let arguments_snapshot = arguments_snapshot_str
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;
    let policy_rule_matches = policy_rule_matches_str
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?;

    let tool_call_id = ToolCallId::from_string(&tool_call_id_str).unwrap_or_default();
    let created_at = unix_ms_to_instant(created_at_ms as u64);
    let timeout = Duration::from_millis(timeout_ms as u64);

    Ok(ApprovalTicket {
        ticket_id,
        tool_call_id,
        tool_name,
        summary,
        risk_level,
        status,
        decision_tx: None,
        policy_decision,
        created_at,
        timeout,
        arguments_snapshot,
        policy_rule_matches,
        granted_by,
        session_id,
        source_agent_id,
        task_id,
    })
}

fn instant_to_unix_ms(inst: Instant) -> u64 {
    let now = Instant::now();
    let now_unix_ms = chrono::Utc::now().timestamp_millis() as u64;
    if inst >= now {
        let delta = inst.duration_since(now).as_millis() as u64;
        now_unix_ms.saturating_add(delta)
    } else {
        let delta = now.duration_since(inst).as_millis() as u64;
        now_unix_ms.saturating_sub(delta)
    }
}

fn unix_ms_to_instant(ms: u64) -> Instant {
    let now = Instant::now();
    let now_unix_ms = chrono::Utc::now().timestamp_millis() as u64;
    if ms >= now_unix_ms {
        let delta = Duration::from_millis(ms - now_unix_ms);
        now + delta
    } else {
        let delta = Duration::from_millis(now_unix_ms - ms);
        now.checked_sub(delta).unwrap_or(now)
    }
}
