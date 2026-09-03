//! Static context loader — reads hand-curated MEMORY.md files and exposes
//! them as a stable prompt prefix.
//!
//! Unlike the indexer pipeline (scan_directory → reconcile → parse →
//! upsert), the StaticContextLoader does NOT write to the memory DB and
//! does NOT participate in FTS / vector / consolidation. Its sole job is to
//! surface user-authored memory files so the model sees them every turn
//! without busting prompt caching (the content is stable across turns
//! unless the file changes).

use std::path::{Path, PathBuf};
use sha2::{Digest, Sha256};

/// A loaded static context snapshot.
#[derive(Debug, Clone, Default)]
pub struct StaticContext {
    /// Concatenated, section-headed content ready for prompt injection.
    pub content: String,
    /// SHA-256 of `content`; changes when any source file changes.
    pub hash: String,
    /// Source files that contributed (existing ones only).
    pub sources: Vec<PathBuf>,
}

impl StaticContext {
    /// True when no source file was found (nothing to inject).
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty() || self.content.trim().is_empty()
    }
}

/// Loads `~/.grodex/MEMORY.md` (global) and `<workspace>/MEMORY.md`
/// (workspace) into a single stable prompt prefix.
///
/// Order: global first, then workspace, so workspace-specific notes appear
/// closer to the user turn. Missing files are silently skipped.
pub struct StaticContextLoader;

impl StaticContextLoader {
    /// Load static memory from the default locations.
    ///
    /// - `home_dir`: the user home (`dirs::home_dir()`).
    /// - `workspace`: the current project root.
    pub fn load(home_dir: &Path, workspace: &Path) -> StaticContext {
        let global_path = home_dir.join(".grodex").join("MEMORY.md");
        let workspace_path = workspace.join("MEMORY.md");

        let mut ctx = StaticContext::default();
        let mut hasher = Sha256::new();

        // ── Global MEMORY.md ───────────────────────────────────────
        if let Ok(raw) = std::fs::read_to_string(&global_path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                ctx.content.push_str("## Global Memory\n\n");
                ctx.content.push_str(trimmed);
                ctx.content.push_str("\n\n");
                hasher.update(trimmed.as_bytes());
                ctx.sources.push(global_path);
            }
        }

        // ── Workspace MEMORY.md ───────────────────────────────────
        if let Ok(raw) = std::fs::read_to_string(&workspace_path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                ctx.content.push_str("## Workspace Memory\n\n");
                ctx.content.push_str(trimmed);
                ctx.content.push('\n');
                hasher.update(trimmed.as_bytes());
                ctx.sources.push(workspace_path);
            }
        }

        ctx.hash = {
            let digest = hasher.finalize();
            let mut s = String::with_capacity(16);
            for b in &digest[..8] {
                use std::fmt::Write as _;
                let _ = write!(s, "{:02x}", b);
            }
            s
        };
        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        write!(f, "{body}").unwrap();
    }

    #[test]
    fn load_merges_global_and_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();

        write(&home.join(".grodex").join("MEMORY.md"), "global fact\n");
        write(&ws.join("MEMORY.md"), "workspace note\n");

        let ctx = StaticContextLoader::load(&home, &ws);
        assert!(!ctx.is_empty());
        assert_eq!(ctx.sources.len(), 2);
        assert!(ctx.content.contains("Global Memory"));
        assert!(ctx.content.contains("global fact"));
        assert!(ctx.content.contains("Workspace Memory"));
        assert!(ctx.content.contains("workspace note"));
        assert!(!ctx.hash.is_empty());
    }

    #[test]
    fn load_workspace_only() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();

        write(&ws.join("MEMORY.md"), "just workspace\n");
        let ctx = StaticContextLoader::load(&home, &ws);
        assert!(!ctx.is_empty());
        assert_eq!(ctx.sources.len(), 1);
        assert!(!ctx.content.contains("Global Memory"));
        assert!(ctx.content.contains("Workspace Memory"));
    }

    #[test]
    fn load_empty_when_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        let ctx = StaticContextLoader::load(&home, &ws);
        assert!(ctx.is_empty());
        assert!(ctx.sources.is_empty());
    }

    #[test]
    fn hash_changes_when_content_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();

        write(&ws.join("MEMORY.md"), "v1\n");
        let ctx1 = StaticContextLoader::load(&home, &ws);

        write(&ws.join("MEMORY.md"), "v2\n");
        let ctx2 = StaticContextLoader::load(&home, &ws);

        assert_ne!(ctx1.hash, ctx2.hash, "hash must change when content changes");
    }
}
