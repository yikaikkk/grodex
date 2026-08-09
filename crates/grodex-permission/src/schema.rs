//! SQLite schema for approval ticket persistence.
//!
//! Defines the `approval_tickets` table for durable ticket storage and
//! `ticket_persistence_meta` single-row metadata table for schema versioning.
//!
//! Uses WAL + foreign_keys + busy_timeout pragmas consistent with grodex-memory.

use rusqlite::Connection;

pub const SCHEMA_VERSION: u32 = 1;

pub fn apply_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS approval_tickets (
            ticket_id               TEXT    PRIMARY KEY,
            tool_call_id            TEXT    NOT NULL,
            tool_name               TEXT    NOT NULL,
            summary                 TEXT,
            risk_level              TEXT    NOT NULL,
            status                  TEXT    NOT NULL,
            decision_tx_serialized  TEXT,
            policy_decision         TEXT,
            created_at              INTEGER NOT NULL,
            timeout_ms              INTEGER NOT NULL,
            arguments_snapshot      TEXT,
            policy_rule_matches     TEXT,
            granted_by              TEXT,
            session_id              TEXT,
            source_agent_id         TEXT,
            task_id                 TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_tickets_status ON approval_tickets(status);
        CREATE INDEX IF NOT EXISTS idx_tickets_tool_call ON approval_tickets(tool_call_id);
        CREATE INDEX IF NOT EXISTS idx_tickets_created_at ON approval_tickets(created_at);
        "#,
    )?;

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS ticket_persistence_meta (
            schema_version INTEGER NOT NULL
        );
        "#,
    )?;

    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ticket_persistence_meta",
        [],
        |row| row.get(0),
    )?;
    if count == 0 {
        conn.execute(
            "INSERT INTO ticket_persistence_meta (schema_version) VALUES (?1)",
            rusqlite::params![SCHEMA_VERSION as i64],
        )?;
    }

    Ok(())
}

pub fn read_schema_version(conn: &Connection) -> rusqlite::Result<u32> {
    let value: i64 = conn.query_row(
        "SELECT schema_version FROM ticket_persistence_meta LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    Ok(value as u32)
}
