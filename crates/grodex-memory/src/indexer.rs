//! Incremental index watcher + reconciliation + consolidation journal.
//!
//! Design 08 §7-§8: the memory index is a derived projection of Markdown
//! files. This module provides:
//!
//! - **File scanning**: walk a directory tree, find `.md` files, compute
//!   metadata (mtime, size, SHA-256 content hash).
//! - **Reconciliation**: diff scanned files against the `indexed_files`
//!   table to produce a `ReconciliationDiff` (new / changed / deleted).
//!   The caller decides what to parse and index.
//! - **Consolidation journal**: a state machine for Phase 2 consolidation
//!   transactions (PREPARED → DB_APPLIED → COMPLETED / FAILED), ensuring
//!   crash-safe consolidation with audit trails.
//!
//! The watcher itself is polling-based (no inotify/FSEvents dependency).
//! A background task calls `reconcile` at a configurable interval; the
//! first call after startup is a full scan, subsequent calls are incremental.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::database::MemoryDatabase;
use crate::schema::{bump_index_generation, read_index_generation};
use crate::types::{IndexedFile, SourceKind};

// ─────────────────── File Scanning ───────────────────

/// Metadata for a file discovered during a directory scan.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    /// Absolute path to the file.
    pub path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// Last modification time (Unix epoch seconds).
    pub mtime: i64,
    /// SHA-256 content hash.
    pub content_hash: String,
    /// Canonical path string (used as the indexed_files key).
    pub key: String,
}

/// Walk a directory tree and collect all `.md` files with their metadata.
///
/// Skips hidden files/directories (starting with `.`) and common VCS
/// directories (`.git`, `.svn`, `.hg`, `node_modules`, `target`).
///
/// **Deprecated**: the .md scan → reconcile → parse pipeline is no longer
/// wired into `reindex_memory` (Phase 2 of the memory redesign). Hand-curated
/// MEMORY.md files are now surfaced via [`StaticContextLoader`](crate::static_context::StaticContextLoader)
/// and rollouts drive evidence extraction. Retained for offline eval /
/// migration tooling only.
#[deprecated(note = "use StaticContextLoader for MEMORY.md surfacing; rollout_extractor for evidence")]
#[allow(deprecated)]
pub fn scan_directory(root: &Path) -> Vec<ScannedFile> {
    let mut results = Vec::new();
    let root = match root.canonicalize() {
        Ok(p) => p,
        Err(_) => return results,
    };
    walk_dir(&root, &mut results);
    results
}

fn walk_dir(dir: &Path, results: &mut Vec<ScannedFile>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Skip hidden files and VCS/build directories.
        if name_str.starts_with('.') {
            continue;
        }
        if matches!(
            name_str.as_ref(),
            ".git" | ".svn" | ".hg" | "node_modules" | "target" | "__pycache__"
        ) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };

        if metadata.is_dir() {
            walk_dir(&path, results);
        } else if metadata.is_file() && path.extension().map(|e| e == "md").unwrap_or(false) {
            if let Some(scanned) = scan_single_file(&path) {
                results.push(scanned);
            }
        }
    }
}

/// Compute metadata for a single file.
fn scan_single_file(path: &Path) -> Option<ScannedFile> {
    let metadata = std::fs::metadata(path).ok()?;
    let content = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let hash = format!("{:x}", hasher.finalize());
    let key = path.to_string_lossy().to_string();
    Some(ScannedFile {
        path: path.to_path_buf(),
        size: metadata.len(),
        mtime: metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        content_hash: hash,
        key,
    })
}

// ─────────────────── Reconciliation ───────────────────

/// The diff between a directory scan and the current `indexed_files` table.
#[derive(Debug, Clone, Default)]
pub struct ReconciliationDiff {
    /// Files that exist on disk but not in the index.
    pub new_files: Vec<ScannedFile>,
    /// Files whose mtime/size/hash changed since last indexing.
    pub changed_files: Vec<ScannedFile>,
    /// Files in the index but no longer on disk (deleted or moved).
    pub deleted_files: Vec<String>,
}

impl ReconciliationDiff {
    /// Total number of changes (new + changed + deleted).
    pub fn total_changes(&self) -> usize {
        self.new_files.len() + self.changed_files.len() + self.deleted_files.len()
    }

    /// Whether the diff is empty (no changes needed).
    pub fn is_empty(&self) -> bool {
        self.total_changes() == 0
    }
}

/// Compare scanned files against the `indexed_files` table in the database.
///
/// Returns a diff describing what needs to be indexed, re-indexed, or
/// orphaned. The caller is responsible for parsing the Markdown content
/// and calling the appropriate `MemoryDatabase` methods.
pub fn reconcile(db: &MemoryDatabase, scanned: &[ScannedFile]) -> ReconciliationDiff {
    let mut diff = ReconciliationDiff::default();

    // Build a map of scanned files by key.
    let scanned_map: HashMap<&str, &ScannedFile> =
        scanned.iter().map(|f| (f.key.as_str(), f)).collect();

    // Load all indexed files from the database.
    let indexed = match list_indexed_files(db) {
        Ok(files) => files,
        Err(_) => return diff,
    };

    // Find new and changed files.
    for file in scanned {
        match indexed.get(&file.key) {
            None => {
                diff.new_files.push(file.clone());
            }
            Some(existing) => {
                if existing.content_hash != file.content_hash
                    || existing.size != file.size as i64
                    || existing.mtime != file.mtime
                {
                    diff.changed_files.push(file.clone());
                }
            }
        }
    }

    // Find deleted files (in index but not on disk).
    for key in indexed.keys() {
        if !scanned_map.contains_key(key.as_str()) {
            diff.deleted_files.push(key.clone());
        }
    }

    diff
}

/// List all indexed files from the database, keyed by path.
fn list_indexed_files(db: &MemoryDatabase) -> Result<HashMap<String, IndexedFile>, crate::database::DbError> {
    // We use a direct SQL query through the database's connection.
    // Since MemoryDatabase wraps Arc<Mutex<Connection>>, we need a method
    // to list all files. We'll add one if it doesn't exist.
    let files = db.list_all_indexed_files()?;
    let mut map = HashMap::new();
    for f in files {
        map.insert(f.path.clone(), f);
    }
    Ok(map)
}

/// Apply a reconciliation diff to the database.
///
/// - New files: caller should parse and insert their units, then call
///   `upsert_indexed_file`.
/// - Changed files: caller should re-parse and update their units, then
///   call `upsert_indexed_file`.
/// - Deleted files: orphan/delete their units via `delete_file_and_orphan`.
///
/// This function handles the deleted files and bumps `index_generation`
/// if any changes were made. The caller handles new/changed file content
/// parsing and calls `upsert_indexed_file` for each.
pub fn apply_deletions(db: &MemoryDatabase, diff: &ReconciliationDiff) -> Result<u64, crate::database::DbError> {
    if diff.deleted_files.is_empty() {
        return Ok(read_index_generation_via_db(db));
    }
    for path in &diff.deleted_files {
        db.delete_file_and_orphan(path)?;
    }
    // Bump index_generation since we modified the index.
    let new_gen = db.bump_generation()?;
    Ok(new_gen)
}

/// Read the current index_generation through the database.
fn read_index_generation_via_db(db: &MemoryDatabase) -> u64 {
    db.read_generation().unwrap_or(1)
}

// ─────────────────── Consolidation Journal ───────────────────

/// State of a consolidation transaction (Design 08 §8).
///
/// The state machine is:
/// ```text
/// PREPARED ──→ DB_APPLIED ──→ COMPLETED
///     │            │
///     └────────────┴──→ FAILED
/// ```
///
/// - **PREPARED**: the consolidation plan is computed (input hash, section,
///   manifest) but no database changes have been applied yet.
/// - **DB_APPLIED**: the memory unit has been written/updated in the
///   database; the transaction is waiting for verification.
/// - **COMPLETED**: the consolidation is verified and finalized.
/// - **FAILED**: an error occurred at any stage; the transaction is
///   abandoned. The caller must decide whether to roll back DB changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsolidationState {
    Prepared,
    DbApplied,
    Completed,
    Failed,
}

impl ConsolidationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::DbApplied => "db_applied",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "prepared" => Some(Self::Prepared),
            "db_applied" => Some(Self::DbApplied),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Whether a transition from `self` to `target` is valid.
    pub fn can_transition_to(&self, target: Self) -> bool {
        matches!(
            (self, target),
            (Self::Prepared, Self::DbApplied)
                | (Self::Prepared, Self::Failed)
                | (Self::DbApplied, Self::Completed)
                | (Self::DbApplied, Self::Failed)
        )
    }
}

/// A consolidation transaction record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationTx {
    pub tx_id: String,
    pub state: ConsolidationState,
    pub memory_unit_id: Option<String>,
    pub section: Option<String>,
    pub input_hash: String,
    pub output_hash: Option<String>,
    pub manifest_json: String,
    pub created_at: chrono::DateTime<Utc>,
    pub updated_at: chrono::DateTime<Utc>,
}

impl ConsolidationTx {
    /// Create a new consolidation transaction in the PREPARED state.
    pub fn new_prepared(
        tx_id: impl Into<String>,
        memory_unit_id: Option<String>,
        section: Option<String>,
        input_hash: impl Into<String>,
        manifest_json: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            tx_id: tx_id.into(),
            state: ConsolidationState::Prepared,
            memory_unit_id,
            section,
            input_hash: input_hash.into(),
            output_hash: None,
            manifest_json: manifest_json.into(),
            created_at: now,
            updated_at: now,
        }
    }
}

// ── Database operations for consolidation transactions ──

impl MemoryDatabase {
    /// Insert a new consolidation transaction (must be in PREPARED state).
    pub fn create_consolidation_tx(
        &self,
        tx: &ConsolidationTx,
    ) -> Result<(), crate::database::DbError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO consolidation_transactions
               (tx_id, state, memory_unit_id, section, input_hash, output_hash,
                manifest_json, created_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"#,
            params![
                tx.tx_id,
                tx.state.as_str(),
                tx.memory_unit_id,
                tx.section,
                tx.input_hash,
                tx.output_hash,
                tx.manifest_json,
                tx.created_at.to_rfc3339(),
                tx.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Transition a consolidation transaction to a new state.
    ///
    /// Validates the state transition is legal; returns an error if not.
    /// Updates `updated_at` and optionally sets `output_hash`.
    pub fn transition_consolidation_tx(
        &self,
        tx_id: &str,
        target: ConsolidationState,
        output_hash: Option<&str>,
    ) -> Result<(), crate::database::DbError> {
        let conn = self.conn.lock().unwrap();
        let current_state_str: String = conn
            .query_row(
                "SELECT state FROM consolidation_transactions WHERE tx_id = ?1",
                params![tx_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    crate::database::DbError::Query("consolidation tx not found".into())
                }
                other => other.into(),
            })?;

        let current = ConsolidationState::from_str(&current_state_str)
            .ok_or_else(|| crate::database::DbError::Query(
                format!("unknown consolidation state: {current_state_str}")
            ))?;

        if !current.can_transition_to(target) {
            return Err(crate::database::DbError::Query(format!(
                "invalid consolidation transition: {current_state_str} → {}",
                target.as_str()
            )));
        }

        let now = Utc::now().to_rfc3339();
        match output_hash {
            Some(hash) => {
                conn.execute(
                    "UPDATE consolidation_transactions SET state = ?1, output_hash = ?2, updated_at = ?3 WHERE tx_id = ?4",
                    params![target.as_str(), hash, now, tx_id],
                )?;
            }
            None => {
                conn.execute(
                    "UPDATE consolidation_transactions SET state = ?1, updated_at = ?2 WHERE tx_id = ?3",
                    params![target.as_str(), now, tx_id],
                )?;
            }
        }
        Ok(())
    }

    /// Get a consolidation transaction by ID.
    pub fn get_consolidation_tx(
        &self,
        tx_id: &str,
    ) -> Result<Option<ConsolidationTx>, crate::database::DbError> {
        let conn = self.conn.lock().unwrap();
        let tx = conn
            .query_row(
                "SELECT tx_id, state, memory_unit_id, section, input_hash, output_hash,
                 manifest_json, created_at, updated_at
                 FROM consolidation_transactions WHERE tx_id = ?1",
                params![tx_id],
                |row| {
                    let state_str: String = row.get(1)?;
                    let state = ConsolidationState::from_str(&state_str)
                        .unwrap_or(ConsolidationState::Failed);
                    let created_str: String = row.get(7)?;
                    let updated_str: String = row.get(8)?;
                    Ok(ConsolidationTx {
                        tx_id: row.get(0)?,
                        state,
                        memory_unit_id: row.get(2)?,
                        section: row.get(3)?,
                        input_hash: row.get(4)?,
                        output_hash: row.get(5)?,
                        manifest_json: row.get(6)?,
                        created_at: chrono::DateTime::parse_from_rfc3339(&created_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                        updated_at: chrono::DateTime::parse_from_rfc3339(&updated_str)
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    })
                },
            )
            .optional()?;
        Ok(tx)
    }

    /// List all consolidation transactions in a given state.
    pub fn list_consolidation_txs_by_state(
        &self,
        state: ConsolidationState,
    ) -> Result<Vec<ConsolidationTx>, crate::database::DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tx_id, state, memory_unit_id, section, input_hash, output_hash,
             manifest_json, created_at, updated_at
             FROM consolidation_transactions WHERE state = ?1 ORDER BY created_at",
        )?;
        let txs = stmt
            .query_map(params![state.as_str()], |row| {
                let state_str: String = row.get(1)?;
                let state = ConsolidationState::from_str(&state_str)
                    .unwrap_or(ConsolidationState::Failed);
                let created_str: String = row.get(7)?;
                let updated_str: String = row.get(8)?;
                Ok(ConsolidationTx {
                    tx_id: row.get(0)?,
                    state,
                    memory_unit_id: row.get(2)?,
                    section: row.get(3)?,
                    input_hash: row.get(4)?,
                    output_hash: row.get(5)?,
                    manifest_json: row.get(6)?,
                    created_at: chrono::DateTime::parse_from_rfc3339(&created_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                    updated_at: chrono::DateTime::parse_from_rfc3339(&updated_str)
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(txs)
    }

    /// List all indexed files (used by reconciliation).
    pub fn list_all_indexed_files(
        &self,
    ) -> Result<Vec<IndexedFile>, crate::database::DbError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT path, source_kind, mtime, size, content_hash, index_generation,
             last_indexed_at FROM indexed_files ORDER BY path",
        )?;
        let files = stmt
            .query_map([], |row| {
                Ok(IndexedFile {
                    path: row.get(0)?,
                    source_kind: SourceKind::from_str(&row.get::<_, String>(1)?)
                        .unwrap_or(SourceKind::Memory),
                    mtime: row.get(2)?,
                    size: row.get(3)?,
                    content_hash: row.get(4)?,
                    index_generation: row.get::<_, i64>(5)? as u64,
                    last_indexed_at: chrono::DateTime::parse_from_rfc3339(
                        &row.get::<_, String>(6)?,
                    )
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(files)
    }

    /// Bump the index_generation counter (wraps the schema function).
    pub fn bump_generation(&self) -> Result<u64, crate::database::DbError> {
        let conn = self.conn.lock().unwrap();
        Ok(bump_index_generation(&conn)?)
    }

    /// Read the current index_generation counter.
    pub fn read_generation(&self) -> Result<u64, crate::database::DbError> {
        let conn = self.conn.lock().unwrap();
        Ok(read_index_generation(&conn)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::MemoryDatabase;
    use crate::schema::apply_schema;
    use rusqlite::Connection;

    fn make_db() -> MemoryDatabase {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        MemoryDatabase::from_conn(conn)
    }

    // ── File scanning tests ──

    #[test]
    fn scan_finds_markdown_files() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.md"), "# A\ncontent").unwrap();
        std::fs::write(dir.path().join("b.txt"), "not markdown").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("c.md"), "# C").unwrap();

        let files = scan_directory(dir.path());
        let names: Vec<&str> = files.iter().map(|f| f.path.file_name().unwrap().to_str().unwrap()).collect();
        assert!(names.contains(&"a.md"));
        assert!(names.contains(&"c.md"));
        assert!(!names.contains(&"b.txt"));
    }

    #[test]
    fn scan_skips_hidden_and_vcs() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config.md"), "git").unwrap();
        std::fs::create_dir(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("node_modules").join("pkg.md"), "pkg").unwrap();
        std::fs::write(dir.path().join(".hidden.md"), "hidden").unwrap();
        std::fs::write(dir.path().join("visible.md"), "visible").unwrap();

        let files = scan_directory(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.file_name().unwrap(), "visible.md");
    }

    #[test]
    fn scanned_file_has_correct_hash() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.md"), "hello world").unwrap();

        let files = scan_directory(dir.path());
        assert_eq!(files.len(), 1);

        // Verify hash matches manual computation.
        let mut hasher = Sha256::new();
        hasher.update(b"hello world");
        let expected = format!("{:x}", hasher.finalize());
        assert_eq!(files[0].content_hash, expected);
        assert_eq!(files[0].size, 11);
    }

    // ── Reconciliation tests ──

    #[test]
    fn reconcile_detects_new_files() {
        let db = make_db();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("new.md"), "# New").unwrap();

        let scanned = scan_directory(dir.path());
        let diff = reconcile(&db, &scanned);

        assert_eq!(diff.new_files.len(), 1);
        assert!(diff.changed_files.is_empty());
        assert!(diff.deleted_files.is_empty());
    }

    #[test]
    fn reconcile_detects_changed_files() {
        let db = make_db();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("existing.md");
        std::fs::write(&path, "original content").unwrap();

        // First scan + index.
        let scanned = scan_directory(dir.path());
        let file = &scanned[0];
        db.upsert_indexed_file(&IndexedFile {
            path: file.key.clone(),
            source_kind: SourceKind::Memory,
            mtime: file.mtime,
            size: file.size as i64,
            content_hash: file.content_hash.clone(),
            index_generation: 1,
            last_indexed_at: Utc::now(),
        })
        .unwrap();

        // Modify the file.
        std::fs::write(&path, "modified content").unwrap();
        let re_scanned = scan_directory(dir.path());
        let diff = reconcile(&db, &re_scanned);

        assert!(diff.new_files.is_empty());
        assert_eq!(diff.changed_files.len(), 1);
        assert!(diff.deleted_files.is_empty());
    }

    #[test]
    fn reconcile_detects_deleted_files() {
        let db = make_db();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("will_delete.md");
        std::fs::write(&path, "content").unwrap();

        // First scan + index.
        let scanned = scan_directory(dir.path());
        let file = &scanned[0];
        db.upsert_indexed_file(&IndexedFile {
            path: file.key.clone(),
            source_kind: SourceKind::Memory,
            mtime: file.mtime,
            size: file.size as i64,
            content_hash: file.content_hash.clone(),
            index_generation: 1,
            last_indexed_at: Utc::now(),
        })
        .unwrap();

        // Delete the file.
        std::fs::remove_file(&path).unwrap();
        let re_scanned = scan_directory(dir.path());
        let diff = reconcile(&db, &re_scanned);

        assert!(diff.new_files.is_empty());
        assert!(diff.changed_files.is_empty());
        assert_eq!(diff.deleted_files.len(), 1);
    }

    #[test]
    fn reconcile_empty_when_no_changes() {
        let db = make_db();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("stable.md");
        std::fs::write(&path, "stable content").unwrap();

        // First scan + index.
        let scanned = scan_directory(dir.path());
        let file = &scanned[0];
        db.upsert_indexed_file(&IndexedFile {
            path: file.key.clone(),
            source_kind: SourceKind::Memory,
            mtime: file.mtime,
            size: file.size as i64,
            content_hash: file.content_hash.clone(),
            index_generation: 1,
            last_indexed_at: Utc::now(),
        })
        .unwrap();

        // Re-scan without changes.
        let re_scanned = scan_directory(dir.path());
        let diff = reconcile(&db, &re_scanned);

        assert!(diff.is_empty());
    }

    #[test]
    fn apply_deletions_orphans_and_bumps_generation() {
        let db = make_db();
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("doomed.md");
        std::fs::write(&path, "content").unwrap();

        // Index the file.
        let scanned = scan_directory(dir.path());
        let file = &scanned[0];
        db.upsert_indexed_file(&IndexedFile {
            path: file.key.clone(),
            source_kind: SourceKind::Memory,
            mtime: file.mtime,
            size: file.size as i64,
            content_hash: file.content_hash.clone(),
            index_generation: 1,
            last_indexed_at: Utc::now(),
        })
        .unwrap();

        let gen_before = db.read_generation().unwrap();

        // Delete the file and apply.
        std::fs::remove_file(&path).unwrap();
        let re_scanned = scan_directory(dir.path());
        let diff = reconcile(&db, &re_scanned);
        apply_deletions(&db, &diff).unwrap();

        let gen_after = db.read_generation().unwrap();
        assert!(gen_after > gen_before, "generation should bump after deletions");

        // The indexed_files entry should be gone.
        assert!(db.get_indexed_file(&file.key).unwrap().is_none());
    }

    // ── Consolidation transaction tests ──

    #[test]
    fn consolidation_state_transitions() {
        assert!(ConsolidationState::Prepared.can_transition_to(ConsolidationState::DbApplied));
        assert!(ConsolidationState::Prepared.can_transition_to(ConsolidationState::Failed));
        assert!(ConsolidationState::DbApplied.can_transition_to(ConsolidationState::Completed));
        assert!(ConsolidationState::DbApplied.can_transition_to(ConsolidationState::Failed));

        // Invalid transitions.
        assert!(!ConsolidationState::Prepared.can_transition_to(ConsolidationState::Completed));
        assert!(!ConsolidationState::Completed.can_transition_to(ConsolidationState::Prepared));
        assert!(!ConsolidationState::Failed.can_transition_to(ConsolidationState::DbApplied));
    }

    #[test]
    fn create_and_get_consolidation_tx() {
        let db = make_db();
        let tx = ConsolidationTx::new_prepared(
            "tx-001",
            Some("mem-unit-1".to_string()),
            Some("## Decisions".to_string()),
            "abc123",
            r#"{"source_files": 3}"#,
        );
        db.create_consolidation_tx(&tx).unwrap();

        let fetched = db.get_consolidation_tx("tx-001").unwrap().expect("tx should exist");
        assert_eq!(fetched.tx_id, "tx-001");
        assert_eq!(fetched.state, ConsolidationState::Prepared);
        assert_eq!(fetched.memory_unit_id.as_deref(), Some("mem-unit-1"));
        assert_eq!(fetched.input_hash, "abc123");
    }

    #[test]
    fn transition_prepared_to_db_applied() {
        let db = make_db();
        let tx = ConsolidationTx::new_prepared("tx-002", None, None, "input-hash", "{}");
        db.create_consolidation_tx(&tx).unwrap();

        db.transition_consolidation_tx("tx-002", ConsolidationState::DbApplied, Some("output-hash"))
            .unwrap();

        let fetched = db.get_consolidation_tx("tx-002").unwrap().unwrap();
        assert_eq!(fetched.state, ConsolidationState::DbApplied);
        assert_eq!(fetched.output_hash.as_deref(), Some("output-hash"));
    }

    #[test]
    fn transition_db_applied_to_completed() {
        let db = make_db();
        let tx = ConsolidationTx::new_prepared("tx-003", None, None, "input", "{}");
        db.create_consolidation_tx(&tx).unwrap();
        db.transition_consolidation_tx("tx-003", ConsolidationState::DbApplied, None).unwrap();
        db.transition_consolidation_tx("tx-003", ConsolidationState::Completed, None).unwrap();

        let fetched = db.get_consolidation_tx("tx-003").unwrap().unwrap();
        assert_eq!(fetched.state, ConsolidationState::Completed);
    }

    #[test]
    fn transition_to_failed_from_prepared() {
        let db = make_db();
        let tx = ConsolidationTx::new_prepared("tx-004", None, None, "input", "{}");
        db.create_consolidation_tx(&tx).unwrap();

        db.transition_consolidation_tx("tx-004", ConsolidationState::Failed, None).unwrap();

        let fetched = db.get_consolidation_tx("tx-004").unwrap().unwrap();
        assert_eq!(fetched.state, ConsolidationState::Failed);
    }

    #[test]
    fn invalid_transition_rejected() {
        let db = make_db();
        let tx = ConsolidationTx::new_prepared("tx-005", None, None, "input", "{}");
        db.create_consolidation_tx(&tx).unwrap();

        // PREPARED → COMPLETED is invalid (must go through DB_APPLIED).
        let result = db.transition_consolidation_tx("tx-005", ConsolidationState::Completed, None);
        assert!(result.is_err());
    }

    #[test]
    fn list_txs_by_state() {
        let db = make_db();
        for i in 0..3 {
            let tx = ConsolidationTx::new_prepared(
                format!("tx-prep-{i}"),
                None,
                None,
                "input",
                "{}",
            );
            db.create_consolidation_tx(&tx).unwrap();
        }
        let tx = ConsolidationTx::new_prepared("tx-done", None, None, "input", "{}");
        db.create_consolidation_tx(&tx).unwrap();
        db.transition_consolidation_tx("tx-done", ConsolidationState::DbApplied, None).unwrap();
        db.transition_consolidation_tx("tx-done", ConsolidationState::Completed, None).unwrap();

        let prepared = db.list_consolidation_txs_by_state(ConsolidationState::Prepared).unwrap();
        assert_eq!(prepared.len(), 3);

        let completed = db.list_consolidation_txs_by_state(ConsolidationState::Completed).unwrap();
        assert_eq!(completed.len(), 1);
    }

    #[test]
    fn crash_recovery_finds_stuck_transactions() {
        // Simulate: tx was PREPARED but never completed (crash before DB_APPLIED).
        let db = make_db();
        let tx = ConsolidationTx::new_prepared("tx-stuck", None, None, "input", "{}");
        db.create_consolidation_tx(&tx).unwrap();

        // On recovery, find all non-terminal transactions.
        let stuck = db.list_consolidation_txs_by_state(ConsolidationState::Prepared).unwrap();
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].tx_id, "tx-stuck");

        // Also check DB_APPLIED transactions (crash before COMPLETED).
        let tx2 = ConsolidationTx::new_prepared("tx-stuck-2", None, None, "input", "{}");
        db.create_consolidation_tx(&tx2).unwrap();
        db.transition_consolidation_tx("tx-stuck-2", ConsolidationState::DbApplied, None).unwrap();

        let stuck_applied = db.list_consolidation_txs_by_state(ConsolidationState::DbApplied).unwrap();
        assert_eq!(stuck_applied.len(), 1);
        assert_eq!(stuck_applied[0].tx_id, "tx-stuck-2");
    }
}
