//! Memory management surface for the desktop UI — read/delete/maintain the
//! `grodex-memory` database (`~/.grodex/memory.db`).

use std::path::PathBuf;

use grodex_memory::MemoryDatabase;
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRow {
    pub id: String,
    pub status: String,
    pub kind: String,
    pub scope: String,
    pub content: String,
    pub updated_at: String,
    pub created_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRow {
    pub conflict_id: String,
    pub left_memory_id: String,
    pub right_memory_id: String,
    pub relation: String,
    pub status: String,
    pub reason: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryOverview {
    pub units: Vec<MemoryRow>,
    pub conflicts: Vec<ConflictRow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceReport {
    pub units: usize,
    pub conflicts_pending: usize,
    pub governance_ok: bool,
    pub consolidation_ok: bool,
}

fn db_path() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("GRODEX_MEMORY_DB") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or_else(|| "无法解析 HOME".to_string())?;
    Ok(home.join(".grodex").join("memory.db"))
}

fn open_db() -> Result<MemoryDatabase, String> {
    let p = db_path()?;
    if !p.exists() {
        return Err(format!("记忆库不存在：{}（先跑过至少一次会话才会生成）", p.display()));
    }
    MemoryDatabase::open(&p).map_err(|e| format!("打开记忆库失败 ({}): {e}", p.display()))
}

fn ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339()
}

/// List all memory units + conflict rows for the management panel.
#[tauri::command]
pub fn list_memories() -> Result<MemoryOverview, String> {
    let db = open_db()?;
    let mut units = Vec::new();
    for u in db
        .list_all_memory_units()
        .map_err(|e| format!("读取记忆失败: {e}"))?
    {
        units.push(MemoryRow {
            id: u.id,
            status: format!("{:?}", u.status).to_lowercase(),
            kind: format!("{:?}", u.kind).to_lowercase(),
            scope: format!("{:?}", u.scope).to_lowercase(),
            content: u.content,
            updated_at: ts(u.updated_at),
            created_at: ts(u.created_at),
        });
    }
    let mut conflicts = Vec::new();
    for c in db
        .list_all_conflicts()
        .map_err(|e| format!("读取冲突失败: {e}"))?
    {
        conflicts.push(ConflictRow {
            conflict_id: c.conflict_id,
            left_memory_id: c.left_memory_id,
            right_memory_id: c.right_memory_id,
            relation: format!("{:?}", c.relation).to_lowercase(),
            status: format!("{:?}", c.status).to_lowercase(),
            reason: c.reason,
        });
    }
    Ok(MemoryOverview { units, conflicts })
}

/// Soft-delete a memory unit (status → orphaned; excluded from retrieval).
#[tauri::command]
pub fn delete_memory(id: String) -> Result<(), String> {
    let db = open_db()?;
    db.orphan_memory_unit(&id)
        .map_err(|e| format!("删除记忆失败: {e}"))
}

/// Run one governance + consolidation maintenance pass (detect/resolve
/// conflicts, promote identity candidates, purge stale candidates).
#[tauri::command]
pub fn run_memory_maintenance() -> Result<MaintenanceReport, String> {
    let db = open_db()?;
    // Governance returns a `GovernanceReport` (fail-open internally); its
    // `errors` count is the real success signal, not a hard-coded `true`.
    let gov = db.run_governance_pass(None, None);
    let cons = db.run_consolidation_pass().is_ok();

    // Re-read counts AFTER the passes so the report reflects the result of
    // the maintenance just run (units may have been quarantined/purged,
    // conflicts may have been auto-resolved).
    let units = db
        .list_all_memory_units()
        .map(|v| v.len())
        .unwrap_or(0);
    let conflicts_pending = db.list_pending_conflicts().map(|v| v.len()).unwrap_or(0);

    Ok(MaintenanceReport {
        units,
        conflicts_pending,
        governance_ok: gov.errors == 0,
        consolidation_ok: cons,
    })
}
