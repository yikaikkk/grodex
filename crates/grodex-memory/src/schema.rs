//! SQLite schema for the Memory V2 index.
//!
//! Implements the 10-table design from `docs/08-memory-retrieval-v2-design.md` §9:
//!   skill_catalog, skill_fts, memory_units, memory_fts, evidence_units,
//!   evidence_fts, memory_evidence_edges, indexed_files,
//!   consolidation_transactions, index_meta.
//!
//! File (Markdown) remains the source of truth; SQLite is a rebuildable
//! derived index and relationship projection.

use rusqlite::Connection;

/// Current schema version. Bump when the DDL changes to force a controlled
/// rebuild via `index_generation` increment.
///   v1: 10-table baseline (skill/memory/evidence + FTS5 + edges + indexed_files + tx + meta)
///   v2: +document_embeddings (brute-force vector blob store) + embedding_metadata
///   v3: +evidence_units.access_count/last_accessed_at + governance runtime columns
pub const SCHEMA_VERSION: u32 = 3;

/// Apply the full DDL to a fresh connection. Idempotent — safe to call on
/// every open. Uses `CREATE TABLE IF NOT EXISTS` for all tables.
///
/// FTS5 tables are standalone (not external-content) to avoid column-name
/// mismatch issues. The CRUD layer in `database.rs` keeps them in sync
/// manually via INSERT/DELETE on each upsert.
pub fn apply_schema(conn: &Connection) -> rusqlite::Result<()> {
    // --- Pragmas (Section 10.1 concurrency protocol) ---
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;

    // ─── 1. skill_catalog ───────────────────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS skill_catalog (
            skill_id            TEXT    PRIMARY KEY,
            name                TEXT    NOT NULL,
            description         TEXT    NOT NULL,
            when_to_use         TEXT    NOT NULL DEFAULT '',
            triggers            TEXT    NOT NULL DEFAULT '',
            scope               TEXT    NOT NULL DEFAULT 'workspace',
            enabled             INTEGER NOT NULL DEFAULT 1,
            required_capabilities TEXT  NOT NULL DEFAULT '[]',
            entry_path          TEXT    NOT NULL,
            content_hash        TEXT    NOT NULL DEFAULT '',
            created_at          TEXT    NOT NULL,
            updated_at          TEXT    NOT NULL,
            last_retrieved_at   TEXT,
            retrieved_count     INTEGER NOT NULL DEFAULT 0,
            lint_warning_count  INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )?;

    // ─── 2. skill_fts (standalone, manually synced) ────────────────
    conn.execute_batch(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS skill_fts USING fts5(
            skill_id UNINDEXED,
            name,
            description,
            when_to_use,
            triggers,
            tokenize='unicode61'
        );
        "#,
    )?;

    // ─── 3. memory_units ───────────────────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memory_units (
            id                  TEXT    PRIMARY KEY,
            path                TEXT    NOT NULL,
            section             TEXT    NOT NULL DEFAULT '',
            kind                TEXT    NOT NULL DEFAULT 'fact',
            scope               TEXT    NOT NULL DEFAULT 'workspace',
            status              TEXT    NOT NULL DEFAULT 'active',
            content             TEXT    NOT NULL DEFAULT '',
            content_hash        TEXT    NOT NULL DEFAULT '',
            updated_at          TEXT    NOT NULL,
            created_at          TEXT    NOT NULL,
            access_count        INTEGER NOT NULL DEFAULT 0,
            last_accessed_at    TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_memory_path ON memory_units(path);
        CREATE INDEX IF NOT EXISTS idx_memory_scope_status ON memory_units(scope, status);
        "#,
    )?;

    // ─── 4. memory_fts (standalone, manually synced) ──────────────
    conn.execute_batch(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
            unit_id UNINDEXED,
            content,
            path UNINDEXED,
            tokenize='unicode61'
        );
        "#,
    )?;

    // ─── 5. evidence_units ─────────────────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS evidence_units (
            id                  TEXT    PRIMARY KEY,
            rollout_id          TEXT    NOT NULL,
            path                TEXT    NOT NULL,
            section             TEXT    NOT NULL DEFAULT '',
            scope               TEXT    NOT NULL DEFAULT 'workspace',
            status              TEXT    NOT NULL DEFAULT 'active',
            content             TEXT    NOT NULL DEFAULT '',
            content_hash        TEXT    NOT NULL DEFAULT '',
            occurred_at         TEXT    NOT NULL,
            created_at          TEXT    NOT NULL,
            superseded_by       TEXT,
            superseded_at       TEXT,
            rollout_available   INTEGER NOT NULL DEFAULT 1,
            rollout_expired_at  TEXT,
            subchunk_index      INTEGER NOT NULL DEFAULT 0,
            access_count        INTEGER NOT NULL DEFAULT 0,
            last_accessed_at    TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_evidence_path ON evidence_units(path);
        CREATE INDEX IF NOT EXISTS idx_evidence_rollout ON evidence_units(rollout_id);
        CREATE INDEX IF NOT EXISTS idx_evidence_status ON evidence_units(status);
        CREATE INDEX IF NOT EXISTS idx_evidence_superseded ON evidence_units(superseded_by);
        "#,
    )?;

    // ─── 6. evidence_fts (standalone, manually synced) ────────────
    conn.execute_batch(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS evidence_fts USING fts5(
            unit_id UNINDEXED,
            content,
            path UNINDEXED,
            tokenize='unicode61'
        );
        "#,
    )?;

    // ─── 7. memory_evidence_edges ──────────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memory_evidence_edges (
            memory_id   TEXT    NOT NULL,
            evidence_id TEXT    NOT NULL,
            relation    TEXT    NOT NULL,
            created_at  TEXT    NOT NULL,
            PRIMARY KEY (memory_id, evidence_id, relation),
            FOREIGN KEY (memory_id)   REFERENCES memory_units(id),
            FOREIGN KEY (evidence_id) REFERENCES evidence_units(id)
        );
        CREATE INDEX IF NOT EXISTS idx_edge_evidence ON memory_evidence_edges(evidence_id);
        CREATE INDEX IF NOT EXISTS idx_edge_relation ON memory_evidence_edges(relation);
        "#,
    )?;

    // ─── 8. indexed_files ──────────────────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS indexed_files (
            path            TEXT    PRIMARY KEY,
            source_kind     TEXT    NOT NULL,
            mtime           INTEGER NOT NULL,
            size            INTEGER NOT NULL,
            content_hash    TEXT    NOT NULL,
            index_generation INTEGER NOT NULL DEFAULT 1,
            last_indexed_at TEXT    NOT NULL
        );
        "#,
    )?;

    // ─── 9. consolidation_transactions ─────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS consolidation_transactions (
            tx_id           TEXT    PRIMARY KEY,
            state           TEXT    NOT NULL,
            memory_unit_id  TEXT,
            section         TEXT,
            input_hash      TEXT    NOT NULL,
            output_hash     TEXT,
            manifest_json   TEXT    NOT NULL DEFAULT '{}',
            created_at      TEXT    NOT NULL,
            updated_at      TEXT    NOT NULL
        );
        "#,
    )?;

    // ─── 10. index_meta ────────────────────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS index_meta (
            key     TEXT    PRIMARY KEY,
            value   TEXT    NOT NULL
        );
        "#,
    )?;

    // ─── 11. document_embeddings (brute-force vector blob store) ──
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS document_embeddings (
            doc_ref TEXT NOT NULL,
            chunk_index INTEGER NOT NULL DEFAULT 0,
            embedding_model TEXT NOT NULL,
            embedding_dim INTEGER NOT NULL,
            vector_blob BLOB NOT NULL,
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY (doc_ref, chunk_index, embedding_model)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS idx_doc_emb_model ON document_embeddings(embedding_model);
        "#,
    )?;

    // ─── 12. embedding_metadata ──────────────────────────────────
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS embedding_metadata (
            key TEXT PRIMARY KEY,
            value_json TEXT NOT NULL
        );
        "#,
    )?;

    // Seed index_meta with defaults if absent.
    conn.execute(
        "INSERT OR IGNORE INTO index_meta (key, value) VALUES ('schema_version', ?1)",
        rusqlite::params![SCHEMA_VERSION.to_string()],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO index_meta (key, value) VALUES ('index_generation', '1')",
        [],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO index_meta (key, value) VALUES ('parser_version', '1')",
        [],
    )?;

    // Always bump schema_version record if an older one exists (backward compat upgrade).
    let cur: Result<String, _> = conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'schema_version'",
        [],
        |row| row.get(0),
    );
    if let Ok(cur_str) = cur {
        let parsed: u32 = cur_str.parse().unwrap_or(0);
        if parsed < SCHEMA_VERSION {
            conn.execute(
                "UPDATE index_meta SET value = ?1 WHERE key = 'schema_version'",
                rusqlite::params![SCHEMA_VERSION.to_string()],
            )?;
        }
    }

    // Seed embedding_metadata with its own version.
    conn.execute(
        "INSERT OR IGNORE INTO embedding_metadata (key, value_json) VALUES ('schema_embedding_version', '\"1\"')",
        [],
    )?;

    // ─── v3 migration: add access counters if upgrading from v1/v2 ───
    // SQLite has no "ALTER TABLE ADD IF NOT EXISTS", so we check column
    // existence via PRAGMA and only ALTER when missing. Errors are
    // tolerated (duplicate column is a no-op, any other failure must not
    // abort startup — fail-open).
    {
        fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
            let mut stmt = match conn.prepare(&format!("PRAGMA table_info({table})")) {
                Ok(s) => s,
                Err(_) => return false,
            };
            stmt.query_map([], |r| r.get::<_, String>(1))
                .ok()
                .and_then(|rows| {
                    rows.filter_map(|r| r.ok())
                        .find(|c| c == column)
                        .map(|_| true)
                })
                .unwrap_or(false)
        }
        if !has_column(conn, "evidence_units", "access_count") {
            let _ = conn.execute_batch(
                "ALTER TABLE evidence_units ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0",
            );
        }
        if !has_column(conn, "evidence_units", "last_accessed_at") {
            let _ = conn.execute_batch(
                "ALTER TABLE evidence_units ADD COLUMN last_accessed_at TEXT",
            );
        }
    }

    Ok(())
}

/// Read the current `index_generation` from `index_meta`.
pub fn read_index_generation(conn: &Connection) -> rusqlite::Result<u64> {
    let value: String = conn.query_row(
        "SELECT value FROM index_meta WHERE key = 'index_generation'",
        [],
        |row| row.get(0),
    )?;
    Ok(value.parse().unwrap_or(1))
}

/// Increment `index_generation` by 1. Must be called inside the same
/// transaction that modifies index data (Section 10.1 rule).
pub fn bump_index_generation(conn: &Connection) -> rusqlite::Result<u64> {
    let current = read_index_generation(conn)?;
    let next = current + 1;
    conn.execute(
        "UPDATE index_meta SET value = ?1 WHERE key = 'index_generation'",
        rusqlite::params![next.to_string()],
    )?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_applies_cleanly() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for expected in [
            "skill_catalog",
            "skill_fts",
            "memory_units",
            "memory_fts",
            "evidence_units",
            "evidence_fts",
            "memory_evidence_edges",
            "indexed_files",
            "consolidation_transactions",
            "index_meta",
            "document_embeddings",
            "embedding_metadata",
        ] {
            assert!(tables.contains(&expected.to_string()), "missing table: {expected}");
        }
    }

    #[test]
    fn index_generation_starts_at_1() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        assert_eq!(read_index_generation(&conn).unwrap(), 1);
    }

    #[test]
    fn index_generation_increments() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        assert_eq!(bump_index_generation(&conn).unwrap(), 2);
        assert_eq!(read_index_generation(&conn).unwrap(), 2);
        assert_eq!(bump_index_generation(&conn).unwrap(), 3);
    }

    #[test]
    fn schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        apply_schema(&conn).unwrap();
        assert_eq!(read_index_generation(&conn).unwrap(), 1);
    }
}
