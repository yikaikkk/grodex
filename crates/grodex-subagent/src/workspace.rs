//! WorkspaceManager — file isolation and parallel write governance.
//!
//! Design Doc 12 §18: `workspace_mode` supports 4 modes:
//!
//! | Mode             | Write ability     | Use case                     |
//! |------------------|-------------------|------------------------------|
//! | shared_readonly  | No writes         | Search, analysis, review     |
//! | shared_write     | Shared dir writes | Serial or non-conflicting    |
//! | worktree         | Independent Git   | Parallel coding (recommended)|
//! | ephemeral        | Temp dir, cleaned | Generation, experiments      |
//!
//! Rules:
//! 1. Readonly sandbox guarantees concurrency safety.
//! 2. `shared_write` must enter the workspace write scheduler.
//! 3. Structured file tools use normalized path locks.
//! 4. Bash in writable sandbox gets workspace-level write lease.
//! 5. Worktree commits changed-files manifest + base revision.
//! 6. No auto-merge of conflicts.
//! 7. Worktree loss → fail-closed, no silent fallback to shared.
//! 8. Only explicitly-allowed readonly tasks can downgrade to shared.

use crate::node::AgentId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The workspace isolation mode for a child agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    /// No writes allowed (search, analysis, review).
    SharedReadonly,
    /// Writes to the shared workspace directory.
    SharedWrite,
    /// Independent Git worktree (parallel coding default).
    Worktree,
    /// Ephemeral temp directory, cleaned up on task end.
    Ephemeral,
}

impl Default for WorkspaceMode {
    fn default() -> Self {
        Self::SharedReadonly
    }
}

/// A workspace lease — granted to an agent for the duration of its task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceLease {
    /// The agent that holds this lease.
    pub agent_id: AgentId,
    /// The workspace mode.
    pub mode: WorkspaceMode,
    /// The root directory for this workspace.
    pub root: PathBuf,
    /// For worktree mode: the base Git revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    /// When the lease was granted.
    pub granted_at: DateTime<Utc>,
    /// Whether a write lease is held (shared_write or worktree with writes).
    pub has_write_lease: bool,
}

/// Handle to a Git worktree entry.
#[derive(Debug, Clone)]
pub struct WorktreeHandle {
    pub path: PathBuf,
    pub base_revision: Option<String>,
    pub is_git: bool,
}

/// Errors from the WorkspaceManager.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum WorkspaceError {
    #[error("agent {0} already holds a workspace lease")]
    AlreadyLeased(AgentId),
    #[error("agent {0} does not hold a workspace lease")]
    NotLeased(AgentId),
    #[error("write denied: agent {0} has readonly workspace")]
    WriteDenied(AgentId),
    #[error("workspace path conflict: {0}")]
    PathConflict(String),
    #[error("worktree creation failed: {0}")]
    WorktreeCreationFailed(String),
    #[error("lease for agent {0} already released")]
    AlreadyReleased(AgentId),
}

/// WorkspaceManager — manages workspace leases and write locks.
///
/// Tracks which agent has which workspace mode, enforces readonly
/// constraints, and serializes shared_write access via write locks.
#[derive(Debug)]
pub struct WorkspaceManager {
    /// Shared root workspace directory (the project root).
    root_workspace: PathBuf,
    /// Active leases, keyed by agent id.
    leases: HashMap<AgentId, WorkspaceLease>,
    /// Path-level write locks (normalized path → holding agent).
    /// Used exclusively for shared_write mode.
    path_locks: HashMap<PathBuf, AgentId>,
    /// TempDir handles for ephemeral mode (keeps ownership so temp dirs
    /// are cleaned up on drop or revoke).
    tempdir_handles: HashMap<AgentId, tempfile::TempDir>,
    /// Worktree entries for worktree mode.
    worktree_entries: HashMap<AgentId, WorktreeHandle>,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root_workspace: root,
            leases: HashMap::new(),
            path_locks: HashMap::new(),
            tempdir_handles: HashMap::new(),
            worktree_entries: HashMap::new(),
        }
    }

    /// Grant a workspace lease to an agent.
    ///
    /// - `SharedReadonly`: shares the root_workspace, no writes allowed.
    /// - `SharedWrite`: shares the root_workspace with path-level locks.
    /// - `Worktree`: creates a detached `git worktree` under `<root>/.agent-worktrees/<short-id>`.
    ///   Fails if root_workspace is not a Git repository.
    /// - `Ephemeral`: creates a temp directory that is cleaned up on revoke.
    pub fn grant_lease(
        &mut self,
        agent_id: AgentId,
        mode: WorkspaceMode,
        _parent_agent: Option<AgentId>,
    ) -> Result<WorkspaceLease, WorkspaceError> {
        if self.leases.contains_key(&agent_id) {
            return Err(WorkspaceError::AlreadyLeased(agent_id));
        }

        let (root, has_write_lease, base_revision) = match mode {
            WorkspaceMode::SharedReadonly => (self.root_workspace.clone(), false, None),
            WorkspaceMode::SharedWrite => (self.root_workspace.clone(), true, None),
            WorkspaceMode::Worktree => {
                let git_dir = self.root_workspace.join(".git");
                if !git_dir.exists() {
                    return Err(WorkspaceError::WorktreeCreationFailed(
                        "root is not a git repository".into(),
                    ));
                }

                let agent_id_short = short_agent_id(&agent_id);
                let worktrees_dir = self.root_workspace.join(".agent-worktrees");
                let worktree_path = worktrees_dir.join(&agent_id_short);

                if worktree_path.exists() {
                    return Err(WorkspaceError::WorktreeCreationFailed(format!(
                        "worktree path already exists: {}",
                        worktree_path.display()
                    )));
                }

                if let Err(e) = std::fs::create_dir_all(&worktrees_dir) {
                    return Err(WorkspaceError::WorktreeCreationFailed(format!(
                        "failed to create worktrees dir: {e}"
                    )));
                }

                let output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&self.root_workspace)
                    .arg("worktree")
                    .arg("add")
                    .arg(&worktree_path)
                    .arg("--detach")
                    .output()
                    .map_err(|e| {
                        WorkspaceError::WorktreeCreationFailed(format!(
                            "failed to spawn git worktree add: {e}"
                        ))
                    })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                    return Err(WorkspaceError::WorktreeCreationFailed(format!(
                        "git worktree add failed: {stderr}"
                    )));
                }

                let is_git_repo = worktree_path.join(".git").exists()
                    || (worktree_path.join(".git").is_file());

                self.worktree_entries.insert(
                    agent_id,
                    WorktreeHandle {
                        path: worktree_path.clone(),
                        base_revision: None,
                        is_git: is_git_repo,
                    },
                );

                (worktree_path, true, None)
            }
            WorkspaceMode::Ephemeral => {
                let tempdir = tempfile::Builder::new()
                    .prefix("grodex-ephemeral-")
                    .tempdir_in(std::env::temp_dir())
                    .map_err(|e| {
                        WorkspaceError::WorktreeCreationFailed(format!(
                            "failed to create ephemeral tempdir: {e}"
                        ))
                    })?;
                let ephemeral_root = tempdir.path().to_path_buf();
                self.tempdir_handles.insert(agent_id, tempdir);
                (ephemeral_root, true, None)
            }
        };

        let lease = WorkspaceLease {
            agent_id,
            mode,
            root,
            base_revision,
            granted_at: Utc::now(),
            has_write_lease,
        };
        self.leases.insert(agent_id, lease.clone());
        Ok(lease)
    }

    /// Acquire a path-level write lock (shared_write mode only).
    ///
    /// - If the agent's lease mode is not `SharedWrite` → returns `WriteDenied`.
    /// - If another agent already holds the normalized path → returns `PathConflict("locked by {other}")`.
    /// - Idempotent if the agent already holds the lock.
    pub fn acquire_path_lock(
        &mut self,
        agent_id: &AgentId,
        path: &Path,
    ) -> Result<(), WorkspaceError> {
        let lease = self
            .leases
            .get(agent_id)
            .ok_or(WorkspaceError::NotLeased(*agent_id))?;

        if lease.mode != WorkspaceMode::SharedWrite {
            return Err(WorkspaceError::WriteDenied(*agent_id));
        }

        let normalized = normalize_path(&path.to_path_buf());
        if let Some(&holder) = self.path_locks.get(&normalized) {
            if holder != *agent_id {
                return Err(WorkspaceError::PathConflict(format!(
                    "locked by {holder}"
                )));
            }
            return Ok(());
        }

        self.path_locks.insert(normalized, *agent_id);
        Ok(())
    }

    /// Release a path-level write lock held by the given agent.
    ///
    /// No-op if the path is not locked by this agent.
    pub fn release_path_lock(&mut self, agent_id: &AgentId, path: &Path) {
        let normalized = normalize_path(&path.to_path_buf());
        if let Some(&holder) = self.path_locks.get(&normalized) {
            if holder == *agent_id {
                self.path_locks.remove(&normalized);
            }
        }
    }

    /// Revoke a workspace lease, releasing all associated resources.
    ///
    /// 1. Removes any path_locks held by the agent.
    /// 2. For Worktree: runs `git worktree remove --force <path>`, then
    ///    tries to remove the empty worktree parent directory entries.
    ///    Fail-soft: errors are printed via eprintln and do not abort.
    /// 3. For Ephemeral: drops the TempDir handle (cleans up the temp dir).
    /// 4. Removes the lease from the map.
    pub fn revoke_lease(&mut self, agent_id: &AgentId) -> Result<(), WorkspaceError> {
        let lease = self
            .leases
            .remove(agent_id)
            .ok_or(WorkspaceError::NotLeased(*agent_id))?;

        let locked_paths: Vec<PathBuf> = self
            .path_locks
            .iter()
            .filter(|(_, holder)| **holder == *agent_id)
            .map(|(p, _)| p.clone())
            .collect();
        for p in locked_paths {
            self.path_locks.remove(&p);
        }

        match lease.mode {
            WorkspaceMode::Worktree => {
                if let Some(handle) = self.worktree_entries.remove(agent_id) {
                    let worktree_path = &handle.path;
                    let remove_output = std::process::Command::new("git")
                        .arg("-C")
                        .arg(&self.root_workspace)
                        .arg("worktree")
                        .arg("remove")
                        .arg("--force")
                        .arg(worktree_path)
                        .output();

                    match remove_output {
                        Ok(output) if output.status.success() => {}
                        Ok(output) => {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            eprintln!(
                                "[WorkspaceManager] git worktree remove --force {} failed: {}",
                                worktree_path.display(),
                                stderr
                            );
                        }
                        Err(e) => {
                            eprintln!(
                                "[WorkspaceManager] failed to spawn git worktree remove for {}: {}",
                                worktree_path.display(),
                                e
                            );
                        }
                    }

                    if worktree_path.exists() {
                        if let Err(e) = std::fs::remove_dir_all(worktree_path) {
                            eprintln!(
                                "[WorkspaceManager] failed to remove worktree dir {}: {}",
                                worktree_path.display(),
                                e
                            );
                        }
                    }

                    let worktrees_dir = self.root_workspace.join(".agent-worktrees");
                    if worktrees_dir.exists() {
                        if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
                            if entries.count() == 0 {
                                if let Err(e) = std::fs::remove_dir(&worktrees_dir) {
                                    eprintln!(
                                        "[WorkspaceManager] failed to remove empty worktrees dir {}: {}",
                                        worktrees_dir.display(),
                                        e
                                    );
                                }
                            }
                        }
                    }
                }
            }
            WorkspaceMode::Ephemeral => {
                self.tempdir_handles.remove(agent_id);
            }
            WorkspaceMode::SharedReadonly | WorkspaceMode::SharedWrite => {}
        }

        Ok(())
    }

    /// Atomic write-op preparation: verify lease → acquire path lock →
    /// return a `WriteLockGuard` that calls `release_path_lock` on drop.
    pub fn begin_write_op(
        &mut self,
        agent_id: &AgentId,
        path: &Path,
    ) -> Result<WriteLockGuard<'_>, WorkspaceError> {
        let lease = self
            .leases
            .get(agent_id)
            .ok_or(WorkspaceError::NotLeased(*agent_id))?;
        if !lease.has_write_lease {
            return Err(WorkspaceError::WriteDenied(*agent_id));
        }
        self.acquire_path_lock(agent_id, path)?;
        Ok(WriteLockGuard {
            manager: self,
            agent_id: *agent_id,
            path: path.to_path_buf(),
        })
    }

    /// Check if an agent has write permission for a path.
    pub fn can_write(&self, agent_id: &AgentId, _path: &Path) -> bool {
        self.leases
            .get(agent_id)
            .map(|lease| lease.has_write_lease)
            .unwrap_or(false)
    }

    /// Get the workspace lease for an agent.
    pub fn lease(&self, agent_id: &AgentId) -> Option<&WorkspaceLease> {
        self.leases.get(agent_id)
    }

    /// Number of active leases.
    pub fn lease_count(&self) -> usize {
        self.leases.len()
    }

    /// Whether the workspace write lock is held (for shared_write mode,
    /// returns true if any path locks currently exist).
    pub fn write_lock_held(&self) -> bool {
        !self.path_locks.is_empty()
    }

    /// List all active leases (for UI/diagnostics).
    pub fn leases(&self) -> Vec<&WorkspaceLease> {
        self.leases.values().collect()
    }
}

/// RAII guard returned by `WorkspaceManager::begin_write_op`.
/// Releases the acquired path lock automatically when dropped, so a
/// tool execution cannot forget to release the lock (even on panic).
pub struct WriteLockGuard<'a> {
    manager: &'a mut WorkspaceManager,
    agent_id: AgentId,
    path: PathBuf,
}

impl<'a> WriteLockGuard<'a> {
    pub fn agent_id(&self) -> &AgentId { &self.agent_id }
    pub fn path(&self) -> &Path { &self.path }
}

impl Drop for WriteLockGuard<'_> {
    fn drop(&mut self) {
        self.manager.release_path_lock(&self.agent_id, &self.path);
    }
}

/// Return a short identifier derived from an AgentId (first 8 hex chars).
fn short_agent_id(id: &AgentId) -> String {
    let full = id.to_string();
    let clean: String = full.chars().filter(|c| c.is_ascii_hexdigit()).take(8).collect();
    if clean.is_empty() {
        "unknown".into()
    } else {
        clean
    }
}

/// Normalize a path for consistent lock keying.
fn normalize_path(path: &PathBuf) -> PathBuf {
    match path.canonicalize() {
        Ok(canon) => canon,
        Err(_) => {
            let mut normalized = PathBuf::new();
            for component in path.components() {
                use std::path::Component;
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        normalized.pop();
                    }
                    other => normalized.push(other.as_os_str()),
                }
            }
            normalized
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aid() -> AgentId {
        AgentId::new()
    }

    fn mgr() -> WorkspaceManager {
        let tmp = tempfile::tempdir().unwrap();
        WorkspaceManager::new(tmp.path().to_path_buf())
    }

    #[test]
    fn grant_readonly_lease() {
        let mut m = mgr();
        let a = aid();
        let lease = m.grant_lease(a, WorkspaceMode::SharedReadonly, None).unwrap();
        assert!(!lease.has_write_lease);
        assert_eq!(lease.mode, WorkspaceMode::SharedReadonly);
    }

    #[test]
    fn grant_shared_write_lease() {
        let mut m = mgr();
        let a = aid();
        let lease = m.grant_lease(a, WorkspaceMode::SharedWrite, None).unwrap();
        assert!(lease.has_write_lease);
    }

    #[test]
    fn shared_write_can_acquire_path_lock() {
        let mut m = mgr();
        let a = aid();
        m.grant_lease(a, WorkspaceMode::SharedWrite, None).unwrap();
        let path = PathBuf::from("/some/file.txt");
        m.acquire_path_lock(&a, &path).unwrap();
        assert!(m.write_lock_held());
    }

    #[test]
    fn release_shared_write_frees_lock() {
        let mut m = mgr();
        let a = aid();
        let b = aid();
        m.grant_lease(a, WorkspaceMode::SharedWrite, None).unwrap();
        let path = PathBuf::from("/x");
        m.acquire_path_lock(&a, &path).unwrap();
        m.revoke_lease(&a).unwrap();
        assert!(!m.write_lock_held());
        m.grant_lease(b, WorkspaceMode::SharedWrite, None).unwrap();
        m.acquire_path_lock(&b, &path).unwrap();
    }

    #[test]
    fn worktree_fails_without_git_repo() {
        let mut m = mgr();
        let a = aid();
        let err = m.grant_lease(a, WorkspaceMode::Worktree, None).unwrap_err();
        assert!(matches!(err, WorkspaceError::WorktreeCreationFailed(_)));
    }

    #[test]
    fn ephemeral_gets_temp_dir() {
        let mut m = mgr();
        let a = aid();
        let lease = m.grant_lease(a, WorkspaceMode::Ephemeral, None).unwrap();
        assert!(lease.root.to_string_lossy().contains("grodex-ephemeral-"));
        assert!(lease.has_write_lease);
        assert!(lease.root.exists());
        let ephemeral_root = lease.root.clone();
        m.revoke_lease(&a).unwrap();
        assert!(!ephemeral_root.exists());
    }

    #[test]
    fn readonly_cannot_acquire_path_lock() {
        let mut m = mgr();
        let a = aid();
        m.grant_lease(a, WorkspaceMode::SharedReadonly, None).unwrap();
        let path = PathBuf::from("/some/file.txt");
        let err = m.acquire_path_lock(&a, &path).unwrap_err();
        assert_eq!(err, WorkspaceError::WriteDenied(a));
    }

    #[test]
    fn non_shared_write_cannot_acquire_path_lock() {
        let mut m = mgr();
        let a = aid();
        m.grant_lease(a, WorkspaceMode::Ephemeral, None).unwrap();
        let path = PathBuf::from("/some/file.txt");
        let err = m.acquire_path_lock(&a, &path).unwrap_err();
        assert_eq!(err, WorkspaceError::WriteDenied(a));
    }

    #[test]
    fn path_lock_serializes_conflicting_agents() {
        let mut m = mgr();
        let a = aid();
        let b = aid();
        m.grant_lease(a, WorkspaceMode::SharedWrite, None).unwrap();
        m.grant_lease(b, WorkspaceMode::SharedWrite, None).unwrap();

        let path = PathBuf::from("/shared/file.txt");
        m.acquire_path_lock(&a, &path).unwrap();

        let err = m.acquire_path_lock(&b, &path).unwrap_err();
        assert!(matches!(err, WorkspaceError::PathConflict(_)));

        m.acquire_path_lock(&a, &path).unwrap();
    }

    #[test]
    fn release_path_lock_allows_other_agent() {
        let mut m = mgr();
        let a = aid();
        let b = aid();
        m.grant_lease(a, WorkspaceMode::SharedWrite, None).unwrap();
        m.grant_lease(b, WorkspaceMode::SharedWrite, None).unwrap();

        let path = PathBuf::from("/shared/file.txt");
        m.acquire_path_lock(&a, &path).unwrap();
        m.release_path_lock(&a, &path);
        m.acquire_path_lock(&b, &path).unwrap();
    }

    #[test]
    fn revoke_lease_releases_all_path_locks() {
        let mut m = mgr();
        let a = aid();
        m.grant_lease(a, WorkspaceMode::SharedWrite, None).unwrap();
        m.acquire_path_lock(&a, &PathBuf::from("/f1")).unwrap();
        m.acquire_path_lock(&a, &PathBuf::from("/f2")).unwrap();
        m.revoke_lease(&a).unwrap();
        assert_eq!(m.lease_count(), 0);
        assert!(!m.write_lock_held());
    }

    #[test]
    fn double_grant_fails() {
        let mut m = mgr();
        let a = aid();
        m.grant_lease(a, WorkspaceMode::SharedReadonly, None).unwrap();
        assert!(m.grant_lease(a, WorkspaceMode::SharedReadonly, None).is_err());
    }

    #[test]
    fn revoke_unknown_agent_fails() {
        let mut m = mgr();
        let a = aid();
        assert!(m.revoke_lease(&a).is_err());
    }

    #[test]
    fn can_write_checks_lease() {
        let mut m = mgr();
        let a = aid();
        let b = aid();
        m.grant_lease(a, WorkspaceMode::SharedReadonly, None).unwrap();
        m.grant_lease(b, WorkspaceMode::SharedWrite, None).unwrap();
        assert!(!m.can_write(&a, &PathBuf::from("/x")));
        assert!(m.can_write(&b, &PathBuf::from("/x")));
    }

    #[test]
    fn workspace_mode_round_trips_json() {
        let modes = vec![
            WorkspaceMode::SharedReadonly,
            WorkspaceMode::SharedWrite,
            WorkspaceMode::Worktree,
            WorkspaceMode::Ephemeral,
        ];
        for mode in modes {
            let json = serde_json::to_string(&mode).unwrap();
            let back: WorkspaceMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }
}
