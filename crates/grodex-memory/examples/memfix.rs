//! One-shot repair: drop & recreate the FTS5 virtual tables after a manual
//! shadow-table cleanup corrupted them, then clear stale pending proposals.
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db = args.get(1).cloned().unwrap_or_else(|| {
        let h = std::env::var("HOME").unwrap_or_default();
        format!("{h}/.grodex/memory.db")
    });
    let conn = rusqlite::Connection::open(Path::new(&db)).expect("open db");
    for t in ["memory_fts", "evidence_fts", "skill_fts"] {
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {t};"));
    }
    grodex_memory::apply_schema(&conn).expect("recreate schema (incl. FTS)");
    conn.execute_batch("DELETE FROM memory_proposals WHERE status='pending';")
        .expect("clear pending proposals");
    drop(conn);

    // Verify FTS is actually writable through the same stack the app uses.
    let mdb = grodex_memory::MemoryDatabase::open(Path::new(&db)).expect("open MemoryDatabase");
    use grodex_memory::types::*;
    let now = chrono::Utc::now();
    mdb.upsert_memory_unit(&MemoryUnit {
        id: "mem_ftstest".into(),
        path: "__test__".into(),
        section: "#t".into(),
        kind: MemoryKind::Preference,
        scope: MemoryScope::Global,
        status: UnitStatus::Active,
        content: "记住叫我ZZZTest".into(),
        content_hash: "ftstest".into(),
        updated_at: now,
        created_at: now,
    })
    .expect("upsert (FTS write) must succeed");
    let got = mdb.get_memory_unit("mem_ftstest").expect("read").expect("unit exists");
    println!("FTS write OK -> {}", got.content);
    mdb.orphan_memory_unit("mem_ftstest").expect("orphan test unit");
    println!("fts rebuilt + pending proposals cleared at {db}");
}
