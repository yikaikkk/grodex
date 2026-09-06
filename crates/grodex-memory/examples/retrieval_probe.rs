//! One-shot retrieval probe (diagnostic): run the basic three-way retrieval
//! against a real DB and print what FTS finds for each query.
use std::path::Path;

use grodex_memory::retrievers::{retrieve_all, RetrievalConfig};
use grodex_memory::MemoryDatabase;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: retrieval_probe <memory.db> <query>...");
        std::process::exit(2);
    }
    let db = MemoryDatabase::open(Path::new(&args[1])).expect("open db");
    let cfg = RetrievalConfig::default();

    for q in &args[2..] {
        println!("\n########## query: {q} ##########");
        for (superseded, tag) in [(false, "active-only"), (true, "incl-superseded")] {
            let r = retrieve_all(&db, &cfg, q, true, true, true, superseded);
            for d in &r.diagnostics {
                println!(
                    "  [{tag}] source={:?} fts={:?}\n         candidates={} qualified={} returned={} codes={:?}",
                    d.source,
                    d.fts_query,
                    d.candidate_count,
                    d.qualified_count,
                    d.returned_count,
                    d.reason_codes
                );
            }
            println!("  [{tag}] MEMORY:");
            for m in &r.memory {
                println!("    - {} | {}", m.unit_id, first(&m.content, 90));
            }
            println!("  [{tag}] EVIDENCE:");
            for e in &r.evidence {
                println!("    - {} | {}", e.unit_id, first(&e.content, 70));
            }
        }
    }
}

fn first(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if t.len() < s.chars().count() {
        format!("{t}…")
    } else {
        t
    }
}
