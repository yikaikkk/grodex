//! SQLite-backed memory database — CRUD operations for all unit types.
//!
//! Wraps a `rusqlite::Connection` with the schema from `schema.rs`.
//! All insert/update operations run inside short `BEGIN IMMEDIATE`
//! transactions (Section 10.1 concurrency protocol) and atomically
//! bump `index_generation`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::embedding::{cosine_similarity, EmbeddingVector};
use crate::schema;
use crate::types::*;

/// Errors from the memory database.
#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unit not found: {0}")]
    NotFound(String),
    #[error("foreign key violation: {0}")]
    ForeignKey(String),
    #[error("query error: {0}")]
    Query(String),
    #[error("embedding error: {0}")]
    Embedding(String),
}

/// The SQLite-backed memory index database.
///
/// Thread-safe via `Arc<Mutex<Connection>>`. WAL mode is enabled on open.
/// File (Markdown) is the source of truth; this is a rebuildable projection.
pub struct MemoryDatabase {
    pub(crate) conn: Arc<Mutex<Connection>>,
}

impl MemoryDatabase {
    /// Open or create a database at the given path.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        schema::apply_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        schema::apply_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Wrap an existing connection (assumes schema is already applied).
    /// Used by the indexer module and tests that need to share a connection.
    pub fn from_conn(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Read the current index generation.
    pub fn index_generation(&self) -> Result<u64, DbError> {
        let conn = self.conn.lock().unwrap();
        Ok(schema::read_index_generation(&conn)?)
    }

    // ─────────────── Memory Units ───────────────

    /// Upsert a memory unit. Bumps index_generation.
    pub fn upsert_memory_unit(&self, unit: &MemoryUnit) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            r#"INSERT INTO memory_units (id, path, section, kind, scope, status, content,
               content_hash, updated_at, created_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
               ON CONFLICT(id) DO UPDATE SET
               path=excluded.path, section=excluded.section, kind=excluded.kind,
               scope=excluded.scope, status=excluded.status, content=excluded.content,
               content_hash=excluded.content_hash, updated_at=excluded.updated_at"#,
            params![
                unit.id,
                unit.path,
                unit.section,
                unit.kind.as_str(),
                unit.scope.as_str(),
                unit.status.as_str(),
                unit.content,
                unit.content_hash,
                unit.updated_at.to_rfc3339(),
                unit.created_at.to_rfc3339(),
            ],
        )?;
        // Sync standalone FTS: delete old row, insert new.
        tx.execute("DELETE FROM memory_fts WHERE unit_id = ?1", params![unit.id])?;
        tx.execute(
            "INSERT INTO memory_fts (unit_id, content, path) VALUES (?1, ?2, ?3)",
            params![unit.id, unit.content, unit.path],
        )?;
        schema::bump_index_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Get a memory unit by ID.
    pub fn get_memory_unit(&self, id: &str) -> Result<Option<MemoryUnit>, DbError> {
        let conn = self.conn.lock().unwrap();
        let unit = conn
            .query_row(
                "SELECT id, path, section, kind, scope, status, content, content_hash,
                 updated_at, created_at FROM memory_units WHERE id = ?1",
                params![id],
                |row| {
                    Ok(MemoryUnit {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        section: row.get(2)?,
                        kind: MemoryKind::from_str(&row.get::<_, String>(3)?)
                            .unwrap_or(MemoryKind::Fact),
                        scope: MemoryScope::from_str(&row.get::<_, String>(4)?)
                            .unwrap_or(MemoryScope::Workspace),
                        status: UnitStatus::from_str(&row.get::<_, String>(5)?)
                            .unwrap_or(UnitStatus::Active),
                        content: row.get(6)?,
                        content_hash: row.get(7)?,
                        updated_at: parse_ts(&row.get::<_, String>(8)?),
                        created_at: parse_ts(&row.get::<_, String>(9)?),
                    })
                },
            )
            .optional()?;
        Ok(unit)
    }

    /// Mark a memory unit as orphaned (source file disappeared).
    /// Keeps the unit and its provenance edges for diagnostics.
    pub fn orphan_memory_unit(&self, id: &str) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE memory_units SET status = 'orphaned' WHERE id = ?1 AND status = 'active'",
            params![id],
        )?;
        if changed > 0 {
            schema::bump_index_generation(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// List all memory units with the given status.
    pub fn list_memory_units(&self, status: UnitStatus) -> Result<Vec<MemoryUnit>, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, path, section, kind, scope, status, content, content_hash,
             updated_at, created_at FROM memory_units WHERE status = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![status.as_str()], |row| {
            Ok(MemoryUnit {
                id: row.get(0)?,
                path: row.get(1)?,
                section: row.get(2)?,
                kind: MemoryKind::from_str(&row.get::<_, String>(3)?).unwrap_or(MemoryKind::Fact),
                scope: MemoryScope::from_str(&row.get::<_, String>(4)?)
                    .unwrap_or(MemoryScope::Workspace),
                status: UnitStatus::from_str(&row.get::<_, String>(5)?)
                    .unwrap_or(UnitStatus::Active),
                content: row.get(6)?,
                content_hash: row.get(7)?,
                updated_at: parse_ts(&row.get::<_, String>(8)?),
                created_at: parse_ts(&row.get::<_, String>(9)?),
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    // ─────────────── Evidence Units ───────────────

    /// Upsert an evidence unit. Bumps index_generation.
    pub fn upsert_evidence_unit(&self, unit: &EvidenceUnit) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            r#"INSERT INTO evidence_units (id, rollout_id, path, section, scope, status,
               content, content_hash, occurred_at, created_at, superseded_by, superseded_at,
               rollout_available, rollout_expired_at, subchunk_index)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
               ON CONFLICT(id) DO UPDATE SET
               rollout_id=excluded.rollout_id, path=excluded.path, section=excluded.section,
               scope=excluded.scope, status=excluded.status, content=excluded.content,
               content_hash=excluded.content_hash, occurred_at=excluded.occurred_at,
               superseded_by=excluded.superseded_by, superseded_at=excluded.superseded_at,
               rollout_available=excluded.rollout_available,
               rollout_expired_at=excluded.rollout_expired_at,
               subchunk_index=excluded.subchunk_index"#,
            params![
                unit.id,
                unit.rollout_id,
                unit.path,
                unit.section,
                unit.scope.as_str(),
                unit.status.as_str(),
                unit.content,
                unit.content_hash,
                unit.occurred_at.to_rfc3339(),
                unit.created_at.to_rfc3339(),
                unit.superseded_by,
                unit.superseded_at.map(|t| t.to_rfc3339()),
                unit.rollout_available as i64,
                unit.rollout_expired_at.map(|t| t.to_rfc3339()),
                unit.subchunk_index,
            ],
        )?;
        // Sync standalone FTS: delete old row, insert new.
        tx.execute("DELETE FROM evidence_fts WHERE unit_id = ?1", params![unit.id])?;
        tx.execute(
            "INSERT INTO evidence_fts (unit_id, content, path) VALUES (?1, ?2, ?3)",
            params![unit.id, unit.content, unit.path],
        )?;
        schema::bump_index_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Mark an evidence unit as superseded by a memory unit.
    pub fn supersede_evidence(
        &self,
        evidence_id: &str,
        memory_id: &str,
    ) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE evidence_units SET status = 'superseded', superseded_by = ?1, superseded_at = ?2
             WHERE id = ?3",
            params![memory_id, now, evidence_id],
        )?;
        schema::bump_index_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Mark evidence as rollout-expired (original rollout deleted by TTL).
    /// The summary remains retrievable but flagged as unavailable for deep-dive.
    pub fn mark_rollout_expired(&self, evidence_id: &str) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE evidence_units SET rollout_available = 0, rollout_expired_at = ?1 WHERE id = ?2",
            params![now, evidence_id],
        )?;
        schema::bump_index_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Get an evidence unit by ID.
    pub fn get_evidence_unit(&self, id: &str) -> Result<Option<EvidenceUnit>, DbError> {
        let conn = self.conn.lock().unwrap();
        let unit = conn
            .query_row(
                "SELECT id, rollout_id, path, section, scope, status, content, content_hash,
                 occurred_at, created_at, superseded_by, superseded_at,
                 rollout_available, rollout_expired_at, subchunk_index
                 FROM evidence_units WHERE id = ?1",
                params![id],
                |row| {
                    let rollout_avail: i64 = row.get(12)?;
                    Ok(EvidenceUnit {
                        id: row.get(0)?,
                        rollout_id: row.get(1)?,
                        path: row.get(2)?,
                        section: row.get(3)?,
                        scope: MemoryScope::from_str(&row.get::<_, String>(4)?)
                            .unwrap_or(MemoryScope::Workspace),
                        status: EvidenceStatus::from_str(&row.get::<_, String>(5)?)
                            .unwrap_or(EvidenceStatus::Active),
                        content: row.get(6)?,
                        content_hash: row.get(7)?,
                        occurred_at: parse_ts(&row.get::<_, String>(8)?),
                        created_at: parse_ts(&row.get::<_, String>(9)?),
                        superseded_by: row.get(10)?,
                        superseded_at: row.get::<_, Option<String>>(11)?.map(|s| parse_ts(&s)),
                        rollout_available: rollout_avail != 0,
                        rollout_expired_at: row.get::<_, Option<String>>(13)?.map(|s| parse_ts(&s)),
                        subchunk_index: row.get(14)?,
                    })
                },
            )
            .optional()?;
        Ok(unit)
    }

    // ─────────────── Provenance Edges ───────────────

    /// Add a provenance edge between a memory unit and an evidence unit.
    pub fn add_provenance_edge(&self, edge: &ProvenanceEdge) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO memory_evidence_edges (memory_id, evidence_id, relation, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                edge.memory_id,
                edge.evidence_id,
                edge.relation.as_str(),
                edge.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get all provenance edges for a memory unit.
    pub fn edges_for_memory(&self, memory_id: &str) -> Result<Vec<ProvenanceEdge>, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT memory_id, evidence_id, relation, created_at
             FROM memory_evidence_edges WHERE memory_id = ?1",
        )?;
        let rows = stmt.query_map(params![memory_id], |row| {
            Ok(ProvenanceEdge {
                memory_id: row.get(0)?,
                evidence_id: row.get(1)?,
                relation: EdgeRelation::from_str(&row.get::<_, String>(2)?)
                    .unwrap_or(EdgeRelation::Supports),
                created_at: parse_ts(&row.get::<_, String>(3)?),
            })
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get all memory units that supersede a given evidence unit.
    pub fn superseding_memories(&self, evidence_id: &str) -> Result<Vec<String>, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT memory_id FROM memory_evidence_edges
             WHERE evidence_id = ?1 AND relation = 'supersedes'",
        )?;
        let rows = stmt.query_map(params![evidence_id], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    // ─────────────── Skill Catalog ───────────────

    /// Upsert a skill catalog entry. Bumps index_generation.
    pub fn upsert_skill(&self, skill: &SkillCatalogEntry) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let triggers_json = serde_json::to_string(&skill.triggers).unwrap_or_default();
        tx.execute(
            r#"INSERT INTO skill_catalog (skill_id, name, description, when_to_use, triggers,
               scope, enabled, required_capabilities, entry_path, content_hash,
               created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
               ON CONFLICT(skill_id) DO UPDATE SET
               name=excluded.name, description=excluded.description, when_to_use=excluded.when_to_use,
               triggers=excluded.triggers, scope=excluded.scope, enabled=excluded.enabled,
               required_capabilities=excluded.required_capabilities, entry_path=excluded.entry_path,
               content_hash=excluded.content_hash, updated_at=excluded.updated_at"#,
            params![
                skill.skill_id,
                skill.name,
                skill.description,
                skill.when_to_use,
                triggers_json,
                skill.scope.as_str(),
                skill.enabled as i64,
                serde_json::to_string(&skill.required_capabilities).unwrap_or_default(),
                skill.entry_path,
                skill.content_hash,
                skill.created_at.to_rfc3339(),
                skill.updated_at.to_rfc3339(),
            ],
        )?;
        // Sync standalone FTS: delete old row, insert new.
        tx.execute("DELETE FROM skill_fts WHERE skill_id = ?1", params![skill.skill_id])?;
        tx.execute(
            "INSERT INTO skill_fts (skill_id, name, description, when_to_use, triggers) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                skill.skill_id,
                skill.name,
                skill.description,
                skill.when_to_use,
                triggers_json,
            ],
        )?;
        schema::bump_index_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Get a skill by ID.
    pub fn get_skill(&self, skill_id: &str) -> Result<Option<SkillCatalogEntry>, DbError> {
        let conn = self.conn.lock().unwrap();
        let skill = conn
            .query_row(
                "SELECT skill_id, name, description, when_to_use, triggers, scope, enabled,
                 required_capabilities, entry_path, content_hash, created_at, updated_at
                 FROM skill_catalog WHERE skill_id = ?1",
                params![skill_id],
                |row| {
                    let triggers_str: String = row.get(4)?;
                    let caps_str: String = row.get(7)?;
                    let enabled: i64 = row.get(6)?;
                    Ok(SkillCatalogEntry {
                        skill_id: row.get(0)?,
                        name: row.get(1)?,
                        description: row.get(2)?,
                        when_to_use: row.get(3)?,
                        triggers: serde_json::from_str(&triggers_str).unwrap_or_default(),
                        scope: MemoryScope::from_str(&row.get::<_, String>(5)?)
                            .unwrap_or(MemoryScope::Workspace),
                        enabled: enabled != 0,
                        required_capabilities: serde_json::from_str(&caps_str).unwrap_or_default(),
                        entry_path: row.get(8)?,
                        content_hash: row.get(9)?,
                        created_at: parse_ts(&row.get::<_, String>(10)?),
                        updated_at: parse_ts(&row.get::<_, String>(11)?),
                    })
                },
            )
            .optional()?;
        Ok(skill)
    }

    // ─────────────── Indexed Files ───────────────

    /// Record or update an indexed file entry.
    pub fn upsert_indexed_file(&self, file: &IndexedFile) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO indexed_files (path, source_kind, mtime, size, content_hash,
               index_generation, last_indexed_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
               ON CONFLICT(path) DO UPDATE SET
               source_kind=excluded.source_kind, mtime=excluded.mtime, size=excluded.size,
               content_hash=excluded.content_hash, index_generation=excluded.index_generation,
               last_indexed_at=excluded.last_indexed_at"#,
            params![
                file.path,
                file.source_kind.as_str(),
                file.mtime,
                file.size,
                file.content_hash,
                file.index_generation as i64,
                file.last_indexed_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get an indexed file by path.
    pub fn get_indexed_file(&self, path: &str) -> Result<Option<IndexedFile>, DbError> {
        let conn = self.conn.lock().unwrap();
        let file = conn
            .query_row(
                "SELECT path, source_kind, mtime, size, content_hash, index_generation,
                 last_indexed_at FROM indexed_files WHERE path = ?1",
                params![path],
                |row| {
                    Ok(IndexedFile {
                        path: row.get(0)?,
                        source_kind: SourceKind::from_str(&row.get::<_, String>(1)?)
                            .unwrap_or(SourceKind::Memory),
                        mtime: row.get(2)?,
                        size: row.get(3)?,
                        content_hash: row.get(4)?,
                        index_generation: row.get::<_, i64>(5)? as u64,
                        last_indexed_at: parse_ts(&row.get::<_, String>(6)?),
                    })
                },
            )
            .optional()?;
        Ok(file)
    }

    /// Delete an indexed file and all its derived units (for rebuilds).
    /// Units with provenance edges are orphaned, not deleted.
    pub fn delete_file_and_orphan(&self, path: &str) -> Result<(), DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        // Orphan memory units that have provenance edges.
        tx.execute(
            "UPDATE memory_units SET status = 'orphaned'
             WHERE path = ?1 AND status = 'active' AND id IN
             (SELECT memory_id FROM memory_evidence_edges)",
            params![path],
        )?;
        // Delete memory_fts rows for units being deleted.
        tx.execute(
            "DELETE FROM memory_fts WHERE unit_id IN
             (SELECT id FROM memory_units WHERE path = ?1 AND id NOT IN
             (SELECT memory_id FROM memory_evidence_edges))",
            params![path],
        )?;
        // Delete memory units without provenance edges.
        tx.execute(
            "DELETE FROM memory_units WHERE path = ?1 AND id NOT IN
             (SELECT memory_id FROM memory_evidence_edges)",
            params![path],
        )?;
        // Orphan evidence units with provenance edges.
        tx.execute(
            "UPDATE evidence_units SET status = 'orphaned'
             WHERE path = ?1 AND status = 'active' AND id IN
             (SELECT evidence_id FROM memory_evidence_edges)",
            params![path],
        )?;
        // Delete evidence_fts rows for units being deleted.
        tx.execute(
            "DELETE FROM evidence_fts WHERE unit_id IN
             (SELECT id FROM evidence_units WHERE path = ?1 AND id NOT IN
             (SELECT evidence_id FROM memory_evidence_edges))",
            params![path],
        )?;
        // Delete evidence units without provenance edges.
        tx.execute(
            "DELETE FROM evidence_units WHERE path = ?1 AND id NOT IN
             (SELECT evidence_id FROM memory_evidence_edges)",
            params![path],
        )?;
        // Delete the indexed_files entry.
        tx.execute("DELETE FROM indexed_files WHERE path = ?1", params![path])?;
        schema::bump_index_generation(&tx)?;
        tx.commit()?;
        Ok(())
    }

    // ─────────────── FTS5 Query Helpers ───────────────

    /// Execute an FTS5 query against the memory_fts table and return raw
    /// candidates (before term-coverage filtering).
    pub(crate) fn fts5_memory_candidates(
        &self,
        fts_query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String, f64)>, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.content, m.path, bm25(memory_fts) as score
             FROM memory_fts JOIN memory_units m ON memory_fts.unit_id = m.id
             WHERE memory_fts MATCH ?1 AND m.status = 'active'
             ORDER BY score LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Execute an FTS5 query against the evidence_fts table.
    pub(crate) fn fts5_evidence_candidates(
        &self,
        fts_query: &str,
        limit: usize,
        include_superseded: bool,
    ) -> Result<Vec<(String, String, String, f64)>, DbError> {
        let conn = self.conn.lock().unwrap();
        let sql = if include_superseded {
            "SELECT e.id, e.content, e.path, bm25(evidence_fts) as score
             FROM evidence_fts JOIN evidence_units e ON evidence_fts.unit_id = e.id
             WHERE evidence_fts MATCH ?1 AND e.status IN ('active', 'superseded')
             ORDER BY score LIMIT ?2"
        } else {
            "SELECT e.id, e.content, e.path, bm25(evidence_fts) as score
             FROM evidence_fts JOIN evidence_units e ON evidence_fts.unit_id = e.id
             WHERE evidence_fts MATCH ?1 AND e.status = 'active'
             ORDER BY score LIMIT ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Execute an FTS5 query against the skill_fts table.
    pub(crate) fn fts5_skill_candidates(
        &self,
        fts_query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String, String, f64)>, DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.skill_id, s.name, s.description, s.entry_path, bm25(skill_fts) as score
             FROM skill_fts JOIN skill_catalog s ON skill_fts.skill_id = s.skill_id
             WHERE skill_fts MATCH ?1 AND s.enabled = 1
             ORDER BY score LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, f64>(4)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    // ─────────────── Embedding / Vector Store ───────────────

    /// 同步写入一条嵌入向量（内部同步，异步 wrapper 见 write_embedding_async）。
    pub(crate) fn write_embedding_sync(
        &self,
        doc_ref: &str,
        chunk: usize,
        model: &str,
        dim: usize,
        vec: &EmbeddingVector,
    ) -> Result<(), DbError> {
        if vec.len() != dim {
            return Err(DbError::Embedding(format!(
                "vector length mismatch: got {} expected dim {}",
                vec.len(),
                dim
            )));
        }

        let mut bytes = Vec::with_capacity(dim * 4);
        for v in vec {
            bytes.extend_from_slice(&v.to_le_bytes());
        }

        let now_ms = Utc::now().timestamp_millis();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT OR REPLACE INTO document_embeddings
               (doc_ref, chunk_index, embedding_model, embedding_dim, vector_blob, created_at_ms)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            params![
                doc_ref,
                chunk as i64,
                model,
                dim as i64,
                bytes.as_slice(),
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// 异步写入一条嵌入向量（spawn_blocking 包同步 SQLite 操作）。
    pub async fn write_embedding(
        &self,
        doc_ref: String,
        chunk: usize,
        model: String,
        dim: usize,
        vec: EmbeddingVector,
    ) -> Result<(), DbError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.write_embedding_sync(&doc_ref, chunk, &model, dim, &vec)
        })
        .await
        .map_err(|e| DbError::Embedding(format!("spawn_blocking join: {e}")))?
    }

    /// 同步 brute-force cosine kNN（内部同步）。
    /// Fail-soft: 若 document_embeddings 表不存在直接返回空 Vec（不报错）。
    pub(crate) fn search_bruteforce_cosine_sync(
        &self,
        query_vec: &EmbeddingVector,
        top_k: usize,
        model_filter: &str,
    ) -> Result<Vec<(String, f32)>, DbError> {
        let conn = self.conn.lock().unwrap();

        let table_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='document_embeddings'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if !table_exists {
            return Ok(Vec::new());
        }

        let mut stmt = match conn.prepare(
            "SELECT doc_ref, chunk_index, vector_blob FROM document_embeddings WHERE embedding_model = ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Ok(Vec::new()),
        };

        let qdim = query_vec.len();
        let mut per_doc_best: HashMap<String, f32> = HashMap::new();

        let rows = stmt.query_map(params![model_filter], |row| {
            let doc_ref: String = row.get(0)?;
            let _chunk: i64 = row.get(1)?;
            let blob: Vec<u8> = row.get(2)?;
            Ok((doc_ref, blob))
        });

        let rows = match rows {
            Ok(r) => r,
            Err(_) => return Ok(Vec::new()),
        };

        for row in rows {
            let (doc_ref, blob) = match row {
                Ok(r) => r,
                Err(_) => continue,
            };
            if blob.len() != qdim * 4 {
                continue;
            }
            let mut doc_vec = Vec::with_capacity(qdim);
            for chunk in blob.chunks_exact(4) {
                let bytes: [u8; 4] = [chunk[0], chunk[1], chunk[2], chunk[3]];
                doc_vec.push(f32::from_le_bytes(bytes));
            }
            let sim = cosine_similarity(query_vec, &doc_vec);
            let entry = per_doc_best.entry(doc_ref.clone()).or_insert(f32::NEG_INFINITY);
            if sim > *entry {
                *entry = sim;
            }
        }

        let mut ranked: Vec<(f32, String)> = per_doc_best.into_iter().map(|(d, s)| (s, d)).collect();
        ranked.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ranked.truncate(top_k);

        Ok(ranked.into_iter().map(|(s, d)| (d, s)).collect())
    }

    /// 异步 brute-force cosine kNN。
    pub async fn search_bruteforce_cosine(
        &self,
        query_vec: EmbeddingVector,
        top_k: usize,
        model_filter: String,
    ) -> Result<Vec<(String, f32)>, DbError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.search_bruteforce_cosine_sync(&query_vec, top_k, &model_filter)
        })
        .await
        .map_err(|e| DbError::Embedding(format!("spawn_blocking join: {e}")))?
    }

    /// Helper: 把 f32 slice 转成小端字节 Vec（用于测试/对外工具）。
    pub fn vec_to_le_bytes(vec: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(vec.len() * 4);
        for v in vec {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }
}

use crate::embedding::EmbeddingModel;
use crate::retrievers::{
    load_evidence_results_in_order, load_memory_results_in_order, reciprocal_rank_fusion,
    retrieve_fts_evidence_ids_only, retrieve_fts_memory_ids_only,
};

/// Hybrid 检索的单条结果（与 RetrievalResult 对齐但简化）。
#[derive(Debug, Clone)]
pub struct RetrievedUnit {
    pub unit_id: String,
    pub path: String,
    pub content: String,
    pub source: ResultSource,
}

impl RetrievedUnit {
    /// Format a slice of retrieved units for injection into the system
    /// prompt (mirrors `LegacyRetriever::format_for_prompt`).
    pub fn format_for_prompt(units: &[RetrievedUnit]) -> String {
        if units.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Relevant Memory from Past Sessions\n\n");
        for unit in units {
            let label = if unit.path.is_empty() {
                "memory"
            } else {
                unit.path.as_str()
            };
            out.push_str(&format!("- **{}**: {}\n", label, unit.content));
        }
        out.push('\n');
        out
    }
}

impl MemoryDatabase {
    /// Hybrid RRF 检索主入口（Memory 管道）。
    ///
    /// Fail-Open 规则（任何失败不 throw，静默降级纯 FTS）：
    ///   - emb=None / enable_embedding=false / No API key → vec_list = []
    ///   - Embedding HTTP 超时/429/网络错误 → vec_list = []
    ///   - Vector store 表缺失 → vec_list = []
    pub async fn retrieve_hybrid_memory(
        &self,
        query: &str,
        top_k: usize,
        emb: Option<&Arc<dyn EmbeddingModel + Send + Sync>>,
    ) -> Result<Vec<RetrievedUnit>, DbError> {
        let candidate_limit = top_k * 2;
        let fts_ids = retrieve_fts_memory_ids_only(self, query, candidate_limit);

        let vector_ids: Vec<String> = if let Some(model) = emb {
            match model.embed_texts(&[query.to_string()]).await {
                Ok(vecs) if !vecs.is_empty() => {
                    let qvec = vecs.into_iter().next().unwrap_or_default();
                    match self
                        .search_bruteforce_cosine(qvec, candidate_limit, model.model_id().to_string())
                        .await
                    {
                        Ok(ranked) => ranked.into_iter().map(|(id, _)| id).collect(),
                        Err(_) => Vec::new(),
                    }
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let fused = reciprocal_rank_fusion(&fts_ids, &vector_ids, top_k, 60.0);
        let results = load_memory_results_in_order(self, &fused);
        Ok(results
            .into_iter()
            .map(|r| RetrievedUnit {
                unit_id: r.unit_id,
                path: r.path,
                content: r.content,
                source: r.source,
            })
            .collect())
    }

    /// Hybrid RRF 检索主入口（Evidence 管道）。
    pub async fn retrieve_hybrid_evidence(
        &self,
        query: &str,
        top_k: usize,
        include_superseded: bool,
        emb: Option<&Arc<dyn EmbeddingModel + Send + Sync>>,
    ) -> Result<Vec<RetrievedUnit>, DbError> {
        let candidate_limit = top_k * 2;
        let fts_ids =
            retrieve_fts_evidence_ids_only(self, query, candidate_limit, include_superseded);

        let vector_ids: Vec<String> = if let Some(model) = emb {
            match model.embed_texts(&[query.to_string()]).await {
                Ok(vecs) if !vecs.is_empty() => {
                    let qvec = vecs.into_iter().next().unwrap_or_default();
                    match self
                        .search_bruteforce_cosine(qvec, candidate_limit, model.model_id().to_string())
                        .await
                    {
                        Ok(ranked) => ranked.into_iter().map(|(id, _)| id).collect(),
                        Err(_) => Vec::new(),
                    }
                }
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let fused = reciprocal_rank_fusion(&fts_ids, &vector_ids, top_k, 60.0);
        let results = load_evidence_results_in_order(self, &fused);
        Ok(results
            .into_iter()
            .map(|r| RetrievedUnit {
                unit_id: r.unit_id,
                path: r.path,
                content: r.content,
                source: r.source,
            })
            .collect())
    }
}

/// Parse an RFC3339 timestamp, falling back to epoch on failure.
fn parse_ts(s: &str) -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| chrono::DateTime::from_timestamp(0, 0).unwrap_or_else(|| Utc::now()))
}

impl Clone for MemoryDatabase {
    fn clone(&self) -> Self {
        Self {
            conn: Arc::clone(&self.conn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_memory_unit(id: &str, content: &str) -> MemoryUnit {
        let now = Utc::now();
        MemoryUnit {
            id: id.to_string(),
            path: "MEMORY.md".to_string(),
            section: format!("#{id}"),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Workspace,
            status: UnitStatus::Active,
            content: content.to_string(),
            content_hash: "abc123".to_string(),
            updated_at: now,
            created_at: now,
        }
    }

    fn make_evidence_unit(id: &str, content: &str, rollout_id: &str) -> EvidenceUnit {
        let now = Utc::now();
        EvidenceUnit {
            id: id.to_string(),
            rollout_id: rollout_id.to_string(),
            path: "summary.md".to_string(),
            section: format!("#{id}"),
            scope: MemoryScope::Workspace,
            status: EvidenceStatus::Active,
            content: content.to_string(),
            content_hash: "def456".to_string(),
            occurred_at: now,
            created_at: now,
            superseded_by: None,
            superseded_at: None,
            rollout_available: true,
            rollout_expired_at: None,
            subchunk_index: 0,
        }
    }

    #[test]
    fn upsert_and_get_memory_unit() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let unit = make_memory_unit("mem_test", "Rust release workflow requires cargo build");
        db.upsert_memory_unit(&unit).unwrap();
        let got = db.get_memory_unit("mem_test").unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().content, unit.content);
        assert_eq!(db.index_generation().unwrap(), 2);
    }

    #[test]
    fn orphan_memory_unit_preserves_row() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit(
            "mem_orphan",
            "test content",
        ))
        .unwrap();
        db.orphan_memory_unit("mem_orphan").unwrap();
        let got = db.get_memory_unit("mem_orphan").unwrap().unwrap();
        assert_eq!(got.status, UnitStatus::Orphaned);
    }

    #[test]
    fn upsert_and_supersede_evidence() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit("mem_new", "new approach"))
            .unwrap();
        db.upsert_evidence_unit(&make_evidence_unit(
            "ev_old",
            "old approach with cargo",
            "rollout_001",
        ))
        .unwrap();
        db.supersede_evidence("ev_old", "mem_new").unwrap();
        let got = db.get_evidence_unit("ev_old").unwrap().unwrap();
        assert_eq!(got.status, EvidenceStatus::Superseded);
        assert_eq!(got.superseded_by.as_deref(), Some("mem_new"));
        assert!(got.superseded_at.is_some());
    }

    #[test]
    fn rollout_expiry_marks_unavailable() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_evidence_unit(&make_evidence_unit(
            "ev_expired",
            "old evidence",
            "rollout_002",
        ))
        .unwrap();
        db.mark_rollout_expired("ev_expired").unwrap();
        let got = db.get_evidence_unit("ev_expired").unwrap().unwrap();
        assert!(!got.rollout_available);
        assert!(got.rollout_expired_at.is_some());
    }

    #[test]
    fn provenance_edges() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit("mem_a", "fact a")).unwrap();
        db.upsert_evidence_unit(&make_evidence_unit(
            "ev_a",
            "evidence a",
            "rollout_003",
        ))
        .unwrap();
        let edge = ProvenanceEdge {
            memory_id: "mem_a".to_string(),
            evidence_id: "ev_a".to_string(),
            relation: EdgeRelation::Supports,
            created_at: Utc::now(),
        };
        db.add_provenance_edge(&edge).unwrap();
        let edges = db.edges_for_memory("mem_a").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, EdgeRelation::Supports);

        let superseding = db.superseding_memories("ev_a").unwrap();
        assert!(superseding.is_empty());
    }

    #[test]
    fn skill_catalog_upsert_and_get() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let now = Utc::now();
        let skill = SkillCatalogEntry {
            skill_id: "skill_release".to_string(),
            name: "Release Workflow".to_string(),
            description: "Guide the release process for the project".to_string(),
            when_to_use: "When publishing a new version".to_string(),
            triggers: vec!["release".to_string(), "publish".to_string()],
            scope: MemoryScope::Workspace,
            enabled: true,
            required_capabilities: vec!["exec".to_string()],
            entry_path: "skills/release/SKILL.md".to_string(),
            content_hash: "hash123".to_string(),
            created_at: now,
            updated_at: now,
        };
        db.upsert_skill(&skill).unwrap();
        let got = db.get_skill("skill_release").unwrap().unwrap();
        assert_eq!(got.name, "Release Workflow");
        assert_eq!(got.triggers.len(), 2);
    }

    #[test]
    fn delete_file_orphans_units_with_edges() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit("mem_with_edge", "fact")).unwrap();
        db.upsert_evidence_unit(&make_evidence_unit(
            "ev_for_mem",
            "evidence",
            "rollout_004",
        ))
        .unwrap();
        db.add_provenance_edge(&ProvenanceEdge {
            memory_id: "mem_with_edge".to_string(),
            evidence_id: "ev_for_mem".to_string(),
            relation: EdgeRelation::Supports,
            created_at: Utc::now(),
        })
        .unwrap();
        db.upsert_memory_unit(&make_memory_unit("mem_no_edge", "fact 2")).unwrap();

        db.delete_file_and_orphan("MEMORY.md").unwrap();

        // Unit with edge is orphaned, not deleted.
        let orphaned = db.get_memory_unit("mem_with_edge").unwrap().unwrap();
        assert_eq!(orphaned.status, UnitStatus::Orphaned);
        // Unit without edge is deleted.
        let deleted = db.get_memory_unit("mem_no_edge").unwrap();
        assert!(deleted.is_none());
    }

    #[test]
    fn indexed_file_roundtrip() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let file = IndexedFile {
            path: "MEMORY.md".to_string(),
            source_kind: SourceKind::Memory,
            mtime: 1234567890,
            size: 1024,
            content_hash: "abc".to_string(),
            index_generation: 1,
            last_indexed_at: Utc::now(),
        };
        db.upsert_indexed_file(&file).unwrap();
        let got = db.get_indexed_file("MEMORY.md").unwrap().unwrap();
        assert_eq!(got.content_hash, "abc");
        assert_eq!(got.source_kind, SourceKind::Memory);
    }

    #[test]
    fn fts5_memory_search_returns_candidates() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit(
            "mem_rust",
            "The project uses Rust for systems programming",
        ))
        .unwrap();
        db.upsert_memory_unit(&make_memory_unit(
            "mem_python",
            "Scripts are written in Python",
        ))
        .unwrap();

        let candidates = db.fts5_memory_candidates("rust", 10).unwrap();
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].0, "mem_rust");
    }

    #[test]
    fn fts5_evidence_excludes_superseded_by_default() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_evidence_unit(&make_evidence_unit(
            "ev_active",
            "cargo build failed on linux",
            "r1",
        ))
        .unwrap();
        db.upsert_evidence_unit(&make_evidence_unit(
            "ev_super",
            "cargo build failed on mac",
            "r2",
        ))
        .unwrap();
        db.upsert_memory_unit(&make_memory_unit("mem_fix", "fixed")).unwrap();
        db.supersede_evidence("ev_super", "mem_fix").unwrap();

        let active_only = db.fts5_evidence_candidates("cargo", 10, false).unwrap();
        assert_eq!(active_only.len(), 1);
        assert_eq!(active_only[0].0, "ev_active");

        let with_super = db.fts5_evidence_candidates("cargo", 10, true).unwrap();
        assert_eq!(with_super.len(), 2);
    }

    /// Fail-open regression: `retrieve_hybrid_memory` with `emb=None` must
    /// still return FTS results (degrades to pure FTS5), never error. This
    /// is the project invariant: embedding unavailability must not block
    /// a turn.
    #[tokio::test]
    async fn retrieve_hybrid_memory_degrades_to_fts_when_emb_none() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit(
            "mem_rust",
            "Rust release workflow requires cargo build on stable",
        ))
        .unwrap();
        db.upsert_memory_unit(&make_memory_unit(
            "mem_release",
            "Release pipeline publishes crates after tests pass",
        ))
        .unwrap();

        // emb=None → vector_ids=[] → RRF over pure FTS results.
        let results = db
            .retrieve_hybrid_memory("cargo build", 5, None)
            .await
            .expect("fail-open: emb=None must not error");
        assert!(
            !results.is_empty(),
            "FTS should find the cargo-build memory even without embeddings"
        );
        assert!(results.iter().any(|r| r.content.contains("cargo build")));
    }

    /// Fail-open regression: when an embedding model is supplied but the
    /// vector store has no embeddings table rows (or `embed_texts` would
    /// fail), the hybrid path still returns FTS results rather than
    /// erroring or returning empty. Uses a stub model whose `embed_texts`
    /// returns an empty vector list — the same shape a real backend yields
    /// on a transient API error.
    #[tokio::test]
    async fn retrieve_hybrid_memory_degrades_to_fts_when_embed_returns_empty() {
        use async_trait::async_trait;

        struct EmptyEmbedding;
        #[async_trait]
        impl EmbeddingModel for EmptyEmbedding {
            fn model_id(&self) -> &str {
                "stub-empty"
            }
            fn dimension(&self) -> usize {
                1536
            }
            async fn embed_texts(
                &self,
                _texts: &[String],
            ) -> Result<Vec<EmbeddingVector>, crate::embedding::EmbeddingError> {
                Ok(Vec::new()) // simulates "no vectors" / transient failure
            }
        }

        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit(
            "mem_deploy",
            "deploy step runs cargo build then ships the binary",
        ))
        .unwrap();

        let emb: Arc<dyn EmbeddingModel + Send + Sync> = Arc::new(EmptyEmbedding);
        let results = db
            .retrieve_hybrid_memory("cargo build", 5, Some(&emb))
            .await
            .expect("fail-open: empty embed result must not error");
        assert!(!results.is_empty());
        assert!(results.iter().any(|r| r.content.contains("cargo build")));
    }
}
