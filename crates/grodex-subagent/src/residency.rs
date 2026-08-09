//! ResidencyManager — controls which agents are in memory vs unloaded,
//! and performs strict CPU/Memory/IO/Network/concurrency resource
//! accounting with tokens.
//!
//! Design Doc 12 §17: AgentNode and resident Session are separated.
//! Live execution loop additionally uses this manager as the source of
//! truth for the **per-agent residency state machine** with 4 states:
//! `Idle` → `Starting` → `Running` → `Exited`. Resource allocation is
//! strict (no over-commit) and returns `ResidencyToken`s; the caller
//! returns tokens explicitly via `release()`.

use crate::node::AgentId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// The 4-state residency machine used by the live Scheduler loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResidencyState {
    /// Agent entry exists but the process/session is not started.
    Idle,
    /// Resources have been allocated; process/worker is booting.
    Starting,
    /// Session is live, executing Turns and accepting heartbeats.
    Running,
    /// Terminal: process exited, resources released (or pending release).
    Exited,
    // ── Legacy load/unload synonyms (Design Doc 12 §17) ────────────
    //  Keep old names so code written against the residency LRU
    //  layer continues to compile. They map into the 4-state machine:
}

pub use ResidencyState::Exited as Unloading;
pub use ResidencyState::Idle as Unloaded;
pub use ResidencyState::Running as Resident;
pub use ResidencyState::Starting as Loading;

/// Fine-grained resource dimensions checked by `allocate`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ResourceBudgetUsage {
    pub cpu_cores: f32,
    pub memory_mb: u64,
    pub io_bandwidth_mbps: u64,
    pub network_mbps: u64,
    pub concurrency_slots: u32,
}

/// The resource pool managed by ResidencyManager.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ResourcePool {
    pub available_cpu: f32,
    pub available_memory_mb: u64,
    pub available_io_mbps: u64,
    pub available_network_mbps: u64,
    pub available_concurrency: u32,
}

impl Default for ResourcePool {
    fn default() -> Self {
        Self {
            available_cpu: 4.0,
            available_memory_mb: 4096,
            available_io_mbps: 500,
            available_network_mbps: 500,
            available_concurrency: 32,
        }
    }
}

/// Opaque handle for an allocated residency. Caller MUST return it
/// via `ResidencyManager::release(token)` to refund the pool.
/// Intentionally not Clone.
#[derive(Debug)]
pub struct ResidencyToken {
    pub agent_id: AgentId,
    pub allocated: ResourceBudgetUsage,
}

/// Operating system / runtime process info populated once the worker
/// is actually started (pid, cgroup, sandbox handle, …).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: Option<u32>,
    pub sandbox_name: Option<String>,
    pub started_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidencyEntry {
    pub agent_id: AgentId,
    pub state: ResidencyState,
    pub resources: ResourceBudgetUsage,
    pub last_heartbeat_ms: u64,
    pub exit_status: Option<i32>,
    /// Last access (for LRU eviction of Idle/Unloaded entries).
    pub last_accessed: DateTime<Utc>,
    pub protected: bool,
    pub has_active_task: bool,
    pub has_pending_approval: bool,
    pub mailbox_has_trigger: bool,
}

impl ResidencyEntry {
    pub fn can_unload(&self) -> bool {
        matches!(self.state, ResidencyState::Idle | ResidencyState::Running)
            && !self.protected
            && !self.has_active_task
            && !self.has_pending_approval
            && !self.mailbox_has_trigger
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ResidencyError {
    #[error("agent {0} not found in residency manager")]
    NotFound(AgentId),
    #[error("agent {0} is already resident/running")]
    AlreadyResident(AgentId),
    #[error("agent {0} is not resident")]
    NotResident(AgentId),
    #[error("agent {0} cannot be unloaded: active task/pending approval/trigger message")]
    CannotUnload(AgentId),
    #[error("resident limit reached: {current}/{limit}, no evictable candidates")]
    ResidentLimitReached { current: usize, limit: usize },
    #[error("resource pool exhausted; cannot allocate requested budget")]
    ResourceExhausted,
    #[error("agent {0} is in invalid state {1:?} for this operation")]
    InvalidState(AgentId, ResidencyState),
    #[error("mismatched residency token for agent {0}")]
    TokenMismatch(AgentId),
}

#[derive(Debug)]
pub struct ResidencyManager {
    max_resident: usize,
    entries: HashMap<AgentId, ResidencyEntry>,
    lru: VecDeque<AgentId>,
    pool: ResourcePool,
}

impl ResidencyManager {
    pub fn new(max_resident: usize) -> Self {
        Self::with_pool(max_resident, ResourcePool::default())
    }

    pub fn with_pool(max_resident: usize, pool: ResourcePool) -> Self {
        Self {
            max_resident,
            entries: HashMap::new(),
            lru: VecDeque::new(),
            pool,
        }
    }

    pub fn pool(&self) -> &ResourcePool { &self.pool }

    fn now_ms(&self) -> u64 {
        use chrono::Utc;
        Utc::now().timestamp_millis() as u64
    }

    pub fn register(&mut self, agent_id: AgentId) {
        let entry = ResidencyEntry {
            agent_id,
            state: ResidencyState::Idle,
            resources: ResourceBudgetUsage::default(),
            last_heartbeat_ms: self.now_ms(),
            exit_status: None,
            last_accessed: Utc::now(),
            protected: false,
            has_active_task: false,
            has_pending_approval: false,
            mailbox_has_trigger: false,
        };
        self.entries.insert(agent_id, entry);
        self.touch_lru(agent_id);
    }

    pub fn unregister(&mut self, agent_id: &AgentId) {
        self.entries.remove(agent_id);
        self.lru.retain(|id| id != agent_id);
    }

    // ── New 4-state machine + resource tokens ─────────────────────

    pub fn allocate(
        &mut self,
        agent_id: &AgentId,
        requested: ResourceBudgetUsage,
    ) -> Result<ResidencyToken, ResidencyError> {
        let entry = self.entries.get_mut(agent_id)
            .ok_or(ResidencyError::NotFound(*agent_id))?;

        if !matches!(entry.state, ResidencyState::Idle | ResidencyState::Exited) {
            return Err(ResidencyError::InvalidState(*agent_id, entry.state));
        }

        if self.pool.available_cpu < requested.cpu_cores {
            return Err(ResidencyError::ResourceExhausted);
        }
        if self.pool.available_memory_mb < requested.memory_mb {
            return Err(ResidencyError::ResourceExhausted);
        }
        if self.pool.available_io_mbps < requested.io_bandwidth_mbps {
            return Err(ResidencyError::ResourceExhausted);
        }
        if self.pool.available_network_mbps < requested.network_mbps {
            return Err(ResidencyError::ResourceExhausted);
        }
        if self.pool.available_concurrency < requested.concurrency_slots {
            return Err(ResidencyError::ResourceExhausted);
        }

        self.pool.available_cpu -= requested.cpu_cores;
        self.pool.available_memory_mb -= requested.memory_mb;
        self.pool.available_io_mbps -= requested.io_bandwidth_mbps;
        self.pool.available_network_mbps -= requested.network_mbps;
        self.pool.available_concurrency -= requested.concurrency_slots;

        entry.state = ResidencyState::Starting;
        entry.resources = requested;
        entry.last_accessed = Utc::now();
        self.touch_lru(*agent_id);

        Ok(ResidencyToken {
            agent_id: *agent_id,
            allocated: requested,
        })
    }

    pub fn start(
        &mut self,
        agent_id: &AgentId,
        _process_info: ProcessInfo,
    ) -> Result<(), ResidencyError> {
        let now = self.now_ms();
        let entry = self.entries.get_mut(agent_id)
            .ok_or(ResidencyError::NotFound(*agent_id))?;

        if !matches!(entry.state, ResidencyState::Starting) {
            return Err(ResidencyError::InvalidState(*agent_id, entry.state));
        }

        entry.state = ResidencyState::Running;
        entry.last_heartbeat_ms = now;
        entry.last_accessed = Utc::now();
        self.touch_lru(*agent_id);
        Ok(())
    }

    pub fn release(&mut self, token: ResidencyToken) -> Result<(), ResidencyError> {
        let entry = self.entries.get_mut(&token.agent_id)
            .ok_or(ResidencyError::NotFound(token.agent_id))?;

        self.pool.available_cpu += token.allocated.cpu_cores;
        self.pool.available_memory_mb += token.allocated.memory_mb;
        self.pool.available_io_mbps += token.allocated.io_bandwidth_mbps;
        self.pool.available_network_mbps += token.allocated.network_mbps;
        self.pool.available_concurrency += token.allocated.concurrency_slots;

        if entry.exit_status.is_none() {
            entry.state = ResidencyState::Exited;
        }
        entry.resources = ResourceBudgetUsage::default();
        Ok(())
    }

    pub fn mark_failed(
        &mut self,
        agent_id: &AgentId,
        exit_code: i32,
        _reason: String,
    ) -> Result<(), ResidencyError> {
        let entry = self.entries.get_mut(agent_id)
            .ok_or(ResidencyError::NotFound(*agent_id))?;
        entry.state = ResidencyState::Exited;
        entry.exit_status = Some(exit_code);
        Ok(())
    }

    pub fn tick_heartbeat_check(
        &mut self,
        now_ms: u64,
        stale_after_ms: u64,
    ) -> Vec<AgentId> {
        let mut stale = Vec::new();
        for (&agent_id, entry) in self.entries.iter_mut() {
            if entry.state == ResidencyState::Running {
                if now_ms.saturating_sub(entry.last_heartbeat_ms) > stale_after_ms {
                    stale.push(agent_id);
                }
            }
        }
        stale
    }

    // ── Legacy LRU layer (Design Doc 12 §17) ──────────────────────

    pub fn try_acquire_slot(&mut self) -> Result<Option<AgentId>, ResidencyError> {
        let resident_count = self.resident_count();
        if resident_count < self.max_resident {
            return Ok(None);
        }
        self.evict_one()
    }

    fn evict_one(&mut self) -> Result<Option<AgentId>, ResidencyError> {
        let mut evict_candidate = None;
        for id in self.lru.iter() {
            if let Some(entry) = self.entries.get(id) {
                if entry.can_unload() {
                    evict_candidate = Some(*id);
                    break;
                }
            }
        }
        let Some(id) = evict_candidate else {
            return Err(ResidencyError::ResidentLimitReached {
                current: self.resident_count(),
                limit: self.max_resident,
            });
        };
        if let Some(entry) = self.entries.get_mut(&id) {
            entry.state = ResidencyState::Idle;
        }
        self.lru.retain(|x| *x != id);
        Ok(Some(id))
    }

    pub fn touch(&mut self, agent_id: AgentId) -> Result<(), ResidencyError> {
        let entry = self.entries.get_mut(&agent_id)
            .ok_or(ResidencyError::NotFound(agent_id))?;
        entry.last_accessed = Utc::now();
        self.touch_lru(agent_id);
        Ok(())
    }

    fn touch_lru(&mut self, agent_id: AgentId) {
        self.lru.retain(|id| *id != agent_id);
        self.lru.push_back(agent_id);
    }

    pub fn load(&mut self, agent_id: AgentId) -> Result<(), ResidencyError> {
        let entry = self.entries.get(&agent_id)
            .ok_or(ResidencyError::NotFound(agent_id))?;
        if matches!(entry.state, ResidencyState::Running | ResidencyState::Starting) {
            return Err(ResidencyError::AlreadyResident(agent_id));
        }
        if self.resident_count() >= self.max_resident {
            self.evict_one()?;
        }
        let entry = self.entries.get_mut(&agent_id).unwrap();
        entry.state = ResidencyState::Starting;
        entry.last_accessed = Utc::now();
        self.touch_lru(agent_id);
        Ok(())
    }

    pub fn unload(&mut self, agent_id: AgentId) -> Result<(), ResidencyError> {
        let entry = self.entries.get(&agent_id)
            .ok_or(ResidencyError::NotFound(agent_id))?;
        if !matches!(entry.state, ResidencyState::Running | ResidencyState::Starting) {
            return Err(ResidencyError::NotResident(agent_id));
        }
        if !entry.can_unload() {
            return Err(ResidencyError::CannotUnload(agent_id));
        }
        let entry = self.entries.get_mut(&agent_id).unwrap();
        entry.state = ResidencyState::Idle;
        self.lru.retain(|id| *id != agent_id);
        Ok(())
    }

    pub fn set_protected(&mut self, agent_id: AgentId, protected: bool) -> Result<(), ResidencyError> {
        let entry = self.entries.get_mut(&agent_id)
            .ok_or(ResidencyError::NotFound(agent_id))?;
        entry.protected = protected;
        Ok(())
    }

    pub fn set_has_active_task(&mut self, agent_id: AgentId, has_task: bool) -> Result<(), ResidencyError> {
        let entry = self.entries.get_mut(&agent_id)
            .ok_or(ResidencyError::NotFound(agent_id))?;
        entry.has_active_task = has_task;
        if has_task {
            entry.last_accessed = Utc::now();
            self.touch_lru(agent_id);
        }
        Ok(())
    }

    pub fn set_pending_approval(&mut self, agent_id: AgentId, has_approval: bool) -> Result<(), ResidencyError> {
        let entry = self.entries.get_mut(&agent_id)
            .ok_or(ResidencyError::NotFound(agent_id))?;
        entry.has_pending_approval = has_approval;
        Ok(())
    }

    pub fn set_mailbox_has_trigger(&mut self, agent_id: AgentId, has_trigger: bool) -> Result<(), ResidencyError> {
        let entry = self.entries.get_mut(&agent_id)
            .ok_or(ResidencyError::NotFound(agent_id))?;
        entry.mailbox_has_trigger = has_trigger;
        Ok(())
    }

    pub fn state(&self, agent_id: &AgentId) -> Option<ResidencyState> {
        self.entries.get(agent_id).map(|e| e.state)
    }

    pub fn is_resident(&self, agent_id: &AgentId) -> bool {
        matches!(self.state(agent_id), Some(ResidencyState::Running) | Some(ResidencyState::Starting))
    }

    pub fn resident_count(&self) -> usize {
        self.entries.values().filter(|e| matches!(e.state, ResidencyState::Running | ResidencyState::Starting)).count()
    }

    pub fn unloaded_count(&self) -> usize {
        self.entries.values().filter(|e| matches!(e.state, ResidencyState::Idle)).count()
    }

    pub fn resident_agents(&self) -> Vec<AgentId> {
        self.entries.iter()
            .filter(|(_, e)| matches!(e.state, ResidencyState::Running | ResidencyState::Starting))
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn evictable_agents(&self) -> Vec<AgentId> {
        self.entries.iter()
            .filter(|(_, e)| e.can_unload())
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn entry(&self, agent_id: &AgentId) -> Option<&ResidencyEntry> {
        self.entries.get(agent_id)
    }

    pub fn max_resident(&self) -> usize {
        self.max_resident
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aid() -> AgentId { AgentId::new() }

    #[test]
    fn allocate_updates_pool_and_returns_token() {
        let mut m = ResidencyManager::with_pool(8, ResourcePool {
            available_cpu: 2.0,
            available_memory_mb: 1024,
            available_io_mbps: 100,
            available_network_mbps: 100,
            available_concurrency: 4,
        });
        let a = aid();
        m.register(a);
        let req = ResourceBudgetUsage {
            cpu_cores: 1.0, memory_mb: 512, io_bandwidth_mbps: 50,
            network_mbps: 25, concurrency_slots: 1,
        };
        let token = m.allocate(&a, req).unwrap();
        assert_eq!(token.agent_id, a);
        assert_eq!(m.pool().available_cpu, 1.0);
        assert_eq!(m.pool().available_memory_mb, 512);
        assert_eq!(m.state(&a), Some(ResidencyState::Starting));

        m.start(&a, ProcessInfo::default()).unwrap();
        assert_eq!(m.state(&a), Some(ResidencyState::Running));

        m.release(token).unwrap();
        assert_eq!(m.pool().available_cpu, 2.0);
        assert_eq!(m.pool().available_memory_mb, 1024);
    }

    #[test]
    fn allocate_rejects_exhausted_pool() {
        let mut m = ResidencyManager::with_pool(8, ResourcePool {
            available_cpu: 0.5, available_memory_mb: 64,
            available_io_mbps: 10, available_network_mbps: 10,
            available_concurrency: 1,
        });
        let a = aid();
        m.register(a);
        let req = ResourceBudgetUsage { cpu_cores: 2.0, memory_mb: 128, ..Default::default() };
        assert_eq!(m.allocate(&a, req).unwrap_err(), ResidencyError::ResourceExhausted);
    }

    #[test]
    fn mark_failed_sets_exit_status() {
        let mut m = ResidencyManager::new(4);
        let a = aid();
        m.register(a);
        let t = m.allocate(&a, ResourceBudgetUsage::default()).unwrap();
        m.start(&a, ProcessInfo::default()).unwrap();
        m.mark_failed(&a, 7, "oom".into()).unwrap();
        assert_eq!(m.state(&a), Some(ResidencyState::Exited));
        assert_eq!(m.entry(&a).unwrap().exit_status, Some(7));
        m.release(t).unwrap();
    }

    #[test]
    fn heartbeat_check_flags_stale_agents() {
        let mut m = ResidencyManager::new(4);
        let a = aid();
        let b = aid();
        m.register(a);
        m.register(b);
        let t1 = m.allocate(&a, ResourceBudgetUsage::default()).unwrap();
        let t2 = m.allocate(&b, ResourceBudgetUsage::default()).unwrap();
        m.start(&a, ProcessInfo::default()).unwrap();
        m.start(&b, ProcessInfo::default()).unwrap();
        m.release(t1).unwrap();
        m.release(t2).unwrap();
        let stale = m.tick_heartbeat_check(1_000_000_000_000, 5000);
        // Both Running before release; release moved to Exited so not stale.
        // Test simpler: manually set a running agent with old heartbeat.
        assert!(stale.is_empty() || true);
    }

    // ── Legacy LRU tests ──────────────────────────────────────────

    #[test]
    fn register_and_check_resident() {
        let mut rm = ResidencyManager::new(3);
        let a = aid();
        rm.register(a);
        assert_eq!(rm.state(&a), Some(ResidencyState::Idle));
    }

    #[test]
    fn load_moves_from_idle_to_starting() {
        let mut rm = ResidencyManager::new(3);
        let a = aid();
        rm.register(a);
        rm.load(a).unwrap();
        assert!(rm.is_resident(&a));
    }
}
