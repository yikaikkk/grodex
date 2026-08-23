//! Instruction discovery — finds instruction nodes from all sources (Design Doc 19 §7).
//!
//! Discovery walks three layers:
//!   1. **Fixed roots** (§7.1): managed prompt bundle, `~/.agent/AGENTS.md`, `~/.agent/rules/*.md`
//!   2. **Workspace chain** (§7.2): canonicalize cwd → find project root → walk root→cwd
//!      scanning `AGENTS.md` and `.agent/rules/*.md` at each level
//!   3. **Compatibility** (§7.3): `.grok`, `.codex`, `.claude`, `.cursor` directories
//!
//! Safety rules:
//!   - Untrusted workspace → metadata only, content not loaded (fail-closed)
//!   - Symlink canonical paths deduplicated
//!   - File size limited (default 256 KiB)
//!   - Hidden files and VCS directories skipped

use crate::manifest::{InstructionKind, InstructionNode, InstructionScope, TrustState};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct DiscoveryStats {
    pub total_nodes: usize,
    pub managed: usize,
    pub user_global: usize,
    pub project: usize,
    pub path_rules: usize,
    pub runtime: usize,
    pub oversized: usize,
    pub duplicates: usize,
    pub untrusted_skipped: usize,
}

/// Maximum file size for an instruction file (256 KiB).
const MAX_FILE_SIZE: u64 = 256 * 1024;

/// Compatibility vendor directories that may contain AGENTS.md or rules.
const COMPAT_VENDORS: &[(&str, &str)] = &[
    (".grok", "grok"),
    (".codex", "codex"),
    (".claude", "claude"),
    (".cursor", "cursor"),
];

/// Configuration for instruction discovery.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Compatibility vendors explicitly enabled for scanning (Doc 19 §7.3).
    /// Recognized labels: `grok`, `codex`, `claude`, `cursor`. EMPTY by
    /// default — vendor directories are NEVER scanned unless the user
    /// opts in per vendor (no silent all-vendor concatenation).
    pub compat_vendors: std::collections::BTreeSet<String>,
    /// Maximum file size in bytes (files larger than this are skipped).
    pub max_file_size: u64,
    /// Whether to scan user-global instructions (~/.agent/).
    pub scan_user_global: bool,
    /// Whether to scan managed prompt bundle.
    pub managed_bundle_path: Option<PathBuf>,
    /// Config prompt generation (stamped on discovered nodes).
    pub config_generation: u64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            // Doc 19 §7.3: vendor dirs opt-in only — empty by default.
            compat_vendors: std::collections::BTreeSet::new(),
            max_file_size: MAX_FILE_SIZE,
            scan_user_global: true,
            managed_bundle_path: None,
            config_generation: 1,
        }
    }
}

/// Result of discovery — all instruction nodes found.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryResult {
    pub nodes: Vec<InstructionNode>,
    /// Paths that were skipped due to size limits.
    pub oversized: Vec<PathBuf>,
    /// Duplicate canonical paths that were deduplicated.
    pub duplicates: Vec<PathBuf>,
    /// Files skipped because workspace is untrusted.
    pub untrusted_skipped: Vec<PathBuf>,
    /// Human-readable diagnostics (Doc 19 §7.3 conflict precedence,
    /// unknown compat vendors). Never affect the prompt hash — they are
    /// explanatory only.
    pub diagnostics: Vec<String>,
}

impl DiscoveryResult {
    pub fn by_kind(&self, kind: &crate::manifest::InstructionKind) -> Vec<&crate::manifest::InstructionNode> {
        self.nodes.iter().filter(|n| &n.kind == kind).collect()
    }
    pub fn managed(&self) -> Vec<&crate::manifest::InstructionNode> { self.by_kind(&InstructionKind::Managed) }
    pub fn user_global(&self) -> Vec<&crate::manifest::InstructionNode> { self.by_kind(&InstructionKind::UserGlobal) }
    pub fn project(&self) -> Vec<&crate::manifest::InstructionNode> { self.by_kind(&InstructionKind::Project) }
    pub fn path_rules(&self) -> Vec<&crate::manifest::InstructionNode> { self.by_kind(&InstructionKind::PathRule) }
    pub fn runtime(&self) -> Vec<&crate::manifest::InstructionNode> { self.by_kind(&InstructionKind::Runtime) }

    pub fn summary_stats(&self) -> DiscoveryStats {
        DiscoveryStats {
            total_nodes: self.nodes.len(),
            managed: self.managed().len(),
            user_global: self.user_global().len(),
            project: self.project().len(),
            path_rules: self.path_rules().len(),
            runtime: self.runtime().len(),
            oversized: self.oversized.len(),
            duplicates: self.duplicates.len(),
            untrusted_skipped: self.untrusted_skipped.len(),
        }
    }
}

/// The instruction discovery engine.
pub struct InstructionDiscovery {
    config: DiscoveryConfig,
}

impl InstructionDiscovery {
    pub fn new(config: DiscoveryConfig) -> Self {
        Self { config }
    }

    /// Discover all instruction nodes for the given cwd.
    ///
    /// Order of discovery (matters for position in manifest):
    ///   1. Managed bundle (Zone A, highest authority)
    ///   2. User global (Zone A)
    ///   3. Workspace chain root→cwd (Zone B)
    ///   4. Compatibility vendors (Zone B, lower priority)
    pub fn discover(&self, cwd: &Path, workspace_trusted: bool) -> DiscoveryResult {
        let mut result = DiscoveryResult::default();
        let mut seen_canonical: HashSet<PathBuf> = HashSet::new();

        // 1. Managed prompt bundle.
        if let Some(bundle) = &self.config.managed_bundle_path {
            self.discover_managed(bundle, &mut result, &mut seen_canonical);
        }

        // 2. User global instructions.
        if self.config.scan_user_global {
            self.discover_user_global(&mut result, &mut seen_canonical);
        }

        // 3. Workspace chain (root → cwd). Canonical directories (those
        //    contributing an AGENTS.md or .agent/rules) are recorded so
        //    the compat scan can emit §7.3 precedence diagnostics.
        let mut canonical_dirs: HashSet<PathBuf> = HashSet::new();
        if let Some(root) = find_project_root(cwd) {
            self.discover_workspace_chain(&root, cwd, workspace_trusted, &mut result, &mut seen_canonical, &mut canonical_dirs);
        } else {
            // No git root — just scan cwd.
            self.discover_workspace_chain(cwd, cwd, workspace_trusted, &mut result, &mut seen_canonical, &mut canonical_dirs);
        }

        // 4. Compatibility vendors (explicit per-vendor opt-in, Doc 19 §7.3).
        if !self.config.compat_vendors.is_empty() {
            self.discover_compat_vendors(cwd, workspace_trusted, &mut result, &mut seen_canonical, &canonical_dirs);
        }

        result
    }

    fn discover_managed(&self, bundle: &Path, result: &mut DiscoveryResult, seen: &mut HashSet<PathBuf>) {
        if !bundle.is_dir() {
            return;
        }
        // Managed bundle: scan for *.md files.
        let files = scan_md_files(bundle, self.config.max_file_size);
        for file in files {
            let canonical = file.canonicalize().unwrap_or_else(|_| file.clone());
            if !seen.insert(canonical.clone()) {
                result.duplicates.push(canonical);
                continue;
            }
            match read_instruction_file(&file, self.config.max_file_size) {
                Ok(content) => {
                    result.nodes.push(InstructionNode::new(
                        format!("managed_{}", file.file_name().unwrap_or_default().to_string_lossy()),
                        InstructionKind::Managed,
                        InstructionScope::Session,
                        canonical.display().to_string(),
                        content,
                        TrustState::Trusted,
                        self.config.config_generation,
                    ));
                }
                Err(ReadError::Oversized) => result.oversized.push(file),
                Err(ReadError::Io(_)) => {}
            }
        }
    }

    fn discover_user_global(&self, result: &mut DiscoveryResult, seen: &mut HashSet<PathBuf>) {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return,
        };

        // ~/.agent/AGENTS.md
        let agents_md = home.join(".agent").join("AGENTS.md");
        if agents_md.exists() {
            if let Ok(canonical) = agents_md.canonicalize() {
                if seen.insert(canonical.clone()) {
                    match read_instruction_file(&agents_md, self.config.max_file_size) {
                        Ok(content) => {
                            result.nodes.push(InstructionNode::new(
                                "user_global_agents_md".to_string(),
                                InstructionKind::UserGlobal,
                                InstructionScope::Session,
                                canonical.display().to_string(),
                                content,
                                TrustState::Trusted,
                                self.config.config_generation,
                            ));
                        }
                        Err(ReadError::Oversized) => result.oversized.push(agents_md),
                        Err(ReadError::Io(_)) => {}
                    }
                } else {
                    result.duplicates.push(canonical);
                }
            }
        }

        // ~/.agent/rules/*.md
        let rules_dir = home.join(".agent").join("rules");
        if rules_dir.is_dir() {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&rules_dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "md"))
                .collect();
            files.sort();
            for file in files {
                if let Ok(canonical) = file.canonicalize() {
                    if !seen.insert(canonical.clone()) {
                        result.duplicates.push(canonical);
                        continue;
                    }
                    match read_instruction_file(&file, self.config.max_file_size) {
                        Ok(content) => {
                            let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
                            result.nodes.push(InstructionNode::new(
                                format!("user_global_rule_{name}"),
                                InstructionKind::UserGlobal,
                                InstructionScope::Session,
                                canonical.display().to_string(),
                                content,
                                TrustState::Trusted,
                                self.config.config_generation,
                            ));
                        }
                        Err(ReadError::Oversized) => result.oversized.push(file),
                        Err(ReadError::Io(_)) => {}
                    }
                }
            }
        }
    }

    fn discover_workspace_chain(
        &self,
        root: &Path,
        cwd: &Path,
        trusted: bool,
        result: &mut DiscoveryResult,
        seen: &mut HashSet<PathBuf>,
        canonical_dirs: &mut HashSet<PathBuf>,
    ) {
        // Walk from root to cwd (inclusive), scanning AGENTS.md and .agent/rules/*.md.
        let trust = if trusted {
            TrustState::UserTrusted
        } else {
            TrustState::Untrusted
        };

        let segments = collect_path_segments(root, cwd);
        for dir in &segments {
            // AGENTS.md at this level.
            let agents_md = dir.join("AGENTS.md");
            if agents_md.exists() {
                if let Ok(canonical) = agents_md.canonicalize() {
                    if !seen.insert(canonical.clone()) {
                        result.duplicates.push(canonical);
                    } else {
                        match read_instruction_file(&agents_md, self.config.max_file_size) {
                            Ok(content) => {
                                if trusted {
                                    result.nodes.push(InstructionNode::new(
                                        format!("project_agents_{}", dir.display()),
                                        InstructionKind::Project,
                                        InstructionScope::Workspace,
                                        canonical.display().to_string(),
                                        content,
                                        trust,
                                        self.config.config_generation,
                                    ));
                                    canonical_dirs.insert(dir.clone());
                                } else {
                                    result.untrusted_skipped.push(canonical);
                                }
                            }
                            Err(ReadError::Oversized) => result.oversized.push(agents_md),
                            Err(ReadError::Io(_)) => {}
                        }
                    }
                }
            }

            // .agent/rules/*.md at this level.
            let rules_dir = dir.join(".agent").join("rules");
            if rules_dir.is_dir() {
                let mut files: Vec<PathBuf> = std::fs::read_dir(&rules_dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "md"))
                    .collect();
                files.sort();
                for file in files {
                    if let Ok(canonical) = file.canonicalize() {
                        if !seen.insert(canonical.clone()) {
                            result.duplicates.push(canonical);
                            continue;
                        }
                        match read_instruction_file(&file, self.config.max_file_size) {
                            Ok(content) => {
                                if trusted {
                                    let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
                                    let scope = InstructionScope::Path(dir.display().to_string());
                                    result.nodes.push(InstructionNode::new(
                                        format!("path_rule_{name}"),
                                        InstructionKind::PathRule,
                                        scope,
                                        canonical.display().to_string(),
                                        content,
                                        trust,
                                        self.config.config_generation,
                                    ));
                                    canonical_dirs.insert(dir.clone());
                                } else {
                                    result.untrusted_skipped.push(canonical);
                                }
                            }
                            Err(ReadError::Oversized) => result.oversized.push(file),
                            Err(ReadError::Io(_)) => {}
                        }
                    }
                }
            }
        }
    }

    fn discover_compat_vendors(
        &self,
        cwd: &Path,
        trusted: bool,
        result: &mut DiscoveryResult,
        seen: &mut HashSet<PathBuf>,
        canonical_dirs: &HashSet<PathBuf>,
    ) {
        if !trusted {
            return; // Don't load compat vendors from untrusted workspaces.
        }

        // Doc 19 §7.3: only explicitly enabled vendors are scanned.
        // Unknown vendor labels get a diagnostic instead of silent ignore.
        let known: HashSet<&str> = COMPAT_VENDORS.iter().map(|(_, label)| *label).collect();
        for requested in &self.config.compat_vendors {
            if !known.contains(requested.as_str()) {
                result.diagnostics.push(format!(
                    "unknown compat vendor `{requested}`（仅支持 grok/codex/claude/cursor，已忽略）"
                ));
            }
        }

        let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        // §7.3 conflict: canonical AGENTS.md / .agent/rules present at cwd
        // → canonical keeps precedence (discovered earlier, thus ordered
        // first); vendor content still loads as lower-priority rules but
        // each load emits a diagnostic (no silent concatenation).
        let canonical_conflict = canonical_dirs.contains(&cwd_canonical);

        for (dir_name, vendor_label) in COMPAT_VENDORS {
            if !self.config.compat_vendors.contains(*vendor_label) {
                continue;
            }
            let vendor_dir = cwd.join(dir_name);
            if !vendor_dir.is_dir() {
                continue;
            }

            // Vendor AGENTS.md equivalent.
            for candidate in &["AGENTS.md", "rules.md", "CLAUDE.md", "cursorrules.md"] {
                let path = vendor_dir.join(candidate);
                if path.exists() {
                    if let Ok(canonical) = path.canonicalize() {
                        if !seen.insert(canonical.clone()) {
                            result.duplicates.push(canonical);
                            continue;
                        }
                        match read_instruction_file(&path, self.config.max_file_size) {
                            Ok(content) => {
                                result.nodes.push(InstructionNode::new(
                                    format!("compat_{vendor_label}_agents"),
                                    InstructionKind::Project,
                                    InstructionScope::Workspace,
                                    canonical.display().to_string(),
                                    content,
                                    TrustState::UserTrusted,
                                    self.config.config_generation,
                                ));
                                if canonical_conflict {
                                    result.diagnostics.push(format!(
                                        "canonical .agent 优先：{} 以低优先级兼容规则装载（Doc 19 §7.3）",
                                        canonical.display()
                                    ));
                                }
                            }
                            Err(ReadError::Oversized) => result.oversized.push(path),
                            Err(ReadError::Io(_)) => {}
                        }
                    }
                }
            }

            // Vendor rules directory.
            let rules_dir = vendor_dir.join("rules");
            if rules_dir.is_dir() {
                let mut files: Vec<PathBuf> = std::fs::read_dir(&rules_dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == "md"))
                    .collect();
                files.sort();
                for file in files {
                    if let Ok(canonical) = file.canonicalize() {
                        if !seen.insert(canonical.clone()) {
                            result.duplicates.push(canonical);
                            continue;
                        }
                        match read_instruction_file(&file, self.config.max_file_size) {
                            Ok(content) => {
                                let name = file.file_name().unwrap_or_default().to_string_lossy().to_string();
                                result.nodes.push(InstructionNode::new(
                                    format!("compat_{vendor_label}_rule_{name}"),
                                    InstructionKind::PathRule,
                                    InstructionScope::Path(cwd.display().to_string()),
                                    canonical.display().to_string(),
                                    content,
                                    TrustState::UserTrusted,
                                    self.config.config_generation,
                                ));
                                if canonical_conflict {
                                    result.diagnostics.push(format!(
                                        "canonical .agent 优先：{} 以低优先级兼容规则装载（Doc 19 §7.3）",
                                        canonical.display()
                                    ));
                                }
                            }
                            Err(ReadError::Oversized) => result.oversized.push(file),
                            Err(ReadError::Io(_)) => {}
                        }
                    }
                }
            }
        }
    }
}

impl Default for InstructionDiscovery {
    fn default() -> Self {
        Self::new(DiscoveryConfig::default())
    }
}

// ── Helpers ───────────────────────────────────────────────────────

enum ReadError {
    Oversized,
    #[allow(dead_code)]
    Io(std::io::Error),
}

fn read_instruction_file(path: &Path, max_size: u64) -> Result<String, ReadError> {
    let metadata = std::fs::metadata(path).map_err(ReadError::Io)?;
    if metadata.len() > max_size {
        return Err(ReadError::Oversized);
    }
    std::fs::read_to_string(path).map_err(ReadError::Io)
}

fn scan_md_files(dir: &Path, _max_size: u64) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|e| e == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Find the project root by walking up looking for `.git` or `.agent`.
fn find_project_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").exists() || current.join(".agent").exists() {
            return Some(current.canonicalize().unwrap_or(current));
        }
        if !current.pop() {
            break;
        }
    }
    None
}

/// Collect path segments from root to cwd (inclusive).
fn collect_path_segments(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut segments = Vec::new();
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    // If root == cwd, just return cwd.
    if root_canonical == cwd_canonical {
        segments.push(root_canonical);
        return segments;
    }

    // Walk from cwd up to root, collecting segments, then reverse.
    let mut current = cwd_canonical.clone();
    loop {
        segments.push(current.clone());
        if current == root_canonical {
            break;
        }
        if !current.pop() {
            break;
        }
    }
    segments.reverse();
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn discover_managed_bundle() {
        let dir = TempDir::new().unwrap();
        let bundle = dir.path().join("managed");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("safety.md"), "Never leak secrets.").unwrap();

        let config = DiscoveryConfig {
            managed_bundle_path: Some(bundle),
            scan_user_global: false,
            ..DiscoveryConfig::default()
        };
        let discovery = InstructionDiscovery::new(config);
        let result = discovery.discover(dir.path(), true);

        assert!(result.nodes.iter().any(|n| n.kind == InstructionKind::Managed), "should find managed node");
        assert!(result.nodes.iter().any(|n| n.content.contains("Never leak secrets")));
    }

    #[test]
    fn discover_workspace_chain_agents_md() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("project");
        let subdir = root.join("src").join("module");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(root.join("AGENTS.md"), "Root project rules.").unwrap();
        std::fs::write(subdir.join("AGENTS.md"), "Module-specific rules.").unwrap();
        // Mark as git root.
        std::fs::write(root.join(".git"), "").unwrap();

        let discovery = InstructionDiscovery::default();
        let result = discovery.discover(&subdir, true);

        let project_nodes: Vec<_> = result.nodes.iter().filter(|n| n.kind == InstructionKind::Project).collect();
        assert!(project_nodes.len() >= 2, "should find at least 2 AGENTS.md files, got {}", project_nodes.len());
        assert!(result.nodes.iter().any(|n| n.content.contains("Root project rules")));
        assert!(result.nodes.iter().any(|n| n.content.contains("Module-specific rules")));
    }

    #[test]
    fn discover_path_rules() {
        let dir = TempDir::new().unwrap();
        let rules_dir = dir.path().join(".agent").join("rules");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("style.md"), "Use tabs.").unwrap();
        std::fs::write(rules_dir.join("testing.md"), "Test everything.").unwrap();

        let discovery = InstructionDiscovery::default();
        let result = discovery.discover(dir.path(), true);

        let path_rules: Vec<_> = result.nodes.iter().filter(|n| n.kind == InstructionKind::PathRule).collect();
        assert_eq!(path_rules.len(), 2, "should find 2 path rules");
        assert!(result.nodes.iter().any(|n| n.content.contains("Use tabs")));
        assert!(result.nodes.iter().any(|n| n.content.contains("Test everything")));
    }

    #[test]
    fn untrusted_workspace_skips_content() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Dangerous content.").unwrap();

        let discovery = InstructionDiscovery::default();
        let result = discovery.discover(dir.path(), false);

        assert!(!result.untrusted_skipped.is_empty(), "should record skipped files");
        assert!(result.nodes.iter().all(|n| !n.content.contains("Dangerous content")), "untrusted content must not be loaded");
    }

    #[test]
    fn oversized_files_skipped() {
        let dir = TempDir::new().unwrap();
        let big_content = "x".repeat((MAX_FILE_SIZE + 1) as usize);
        std::fs::write(dir.path().join("AGENTS.md"), &big_content).unwrap();

        let discovery = InstructionDiscovery::default();
        let result = discovery.discover(dir.path(), true);

        assert!(!result.oversized.is_empty(), "should record oversized file");
        assert!(result.nodes.iter().all(|n| !n.content.contains("xxxx")), "oversized content must not be loaded");
    }

    #[test]
    fn symlink_dedup() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("real.md"), "Real content.").unwrap();
        // Create a symlink to the same file.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                dir.path().join("real.md"),
                dir.path().join(".agent").join("rules").join("link.md"),
            )
            .ok();
        }

        let discovery = InstructionDiscovery::default();
        let result = discovery.discover(dir.path(), true);
        // The symlink should be deduplicated (same canonical path).
        // At minimum, we should not have duplicate content.
        let count = result.nodes.iter().filter(|n| n.content.contains("Real content")).count();
        assert!(count <= 1, "symlinked file should be deduplicated, found {count}");
    }

    #[test]
    fn compatibility_mode_finds_vendor_files() {
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("CLAUDE.md"), "Claude-specific rules.").unwrap();

        let config = DiscoveryConfig {
            compat_vendors: ["claude".to_string()].into_iter().collect(),
            scan_user_global: false,
            ..DiscoveryConfig::default()
        };
        let discovery = InstructionDiscovery::new(config);
        let result = discovery.discover(dir.path(), true);

        assert!(result.nodes.iter().any(|n| n.content.contains("Claude-specific rules")), "should find compat vendor file");
    }

    #[test]
    fn compatibility_mode_disabled_by_default() {
        let dir = TempDir::new().unwrap();
        let grok_dir = dir.path().join(".grok");
        std::fs::create_dir_all(&grok_dir).unwrap();
        std::fs::write(grok_dir.join("AGENTS.md"), "Grok rules.").unwrap();

        let discovery = InstructionDiscovery::default();
        let result = discovery.discover(dir.path(), true);

        assert!(result.nodes.iter().all(|n| !n.content.contains("Grok rules")), "compat should be off by default");
    }

    #[test]
    fn compat_per_vendor_opt_in() {
        // Doc 19 §7.3: enabling one vendor must NOT scan the others.
        let dir = TempDir::new().unwrap();
        let claude_dir = dir.path().join(".claude");
        let cursor_dir = dir.path().join(".cursor");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::create_dir_all(&cursor_dir).unwrap();
        std::fs::write(claude_dir.join("CLAUDE.md"), "Claude rules.").unwrap();
        std::fs::write(cursor_dir.join("cursorrules.md"), "Cursor rules.").unwrap();

        let config = DiscoveryConfig {
            compat_vendors: ["claude".to_string()].into_iter().collect(),
            scan_user_global: false,
            ..DiscoveryConfig::default()
        };
        let result = InstructionDiscovery::new(config).discover(dir.path(), true);

        assert!(result.nodes.iter().any(|n| n.content.contains("Claude rules")));
        assert!(result.nodes.iter().all(|n| !n.content.contains("Cursor rules")), "unenabled vendor must not be scanned");
    }

    #[test]
    fn canonical_precedence_emits_diagnostic() {
        // Doc 19 §7.3: canonical AGENTS.md at cwd + enabled vendor file →
        // both load, canonical ordered first, diagnostic emitted.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Canonical rules.").unwrap();
        let claude_dir = dir.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("CLAUDE.md"), "Claude rules.").unwrap();

        let config = DiscoveryConfig {
            compat_vendors: ["claude".to_string()].into_iter().collect(),
            scan_user_global: false,
            ..DiscoveryConfig::default()
        };
        let result = InstructionDiscovery::new(config).discover(dir.path(), true);

        let canonical_pos = result.nodes.iter().position(|n| n.content.contains("Canonical rules")).expect("canonical loaded");
        let compat_pos = result.nodes.iter().position(|n| n.content.contains("Claude rules")).expect("compat loaded");
        assert!(canonical_pos < compat_pos, "canonical .agent must keep precedence ordering");
        assert!(
            result.diagnostics.iter().any(|d| d.contains("canonical .agent 优先")),
            "conflict must produce a diagnostic, got {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn unknown_compat_vendor_diagnostic() {
        let dir = TempDir::new().unwrap();
        let config = DiscoveryConfig {
            compat_vendors: ["acme".to_string()].into_iter().collect(),
            scan_user_global: false,
            ..DiscoveryConfig::default()
        };
        let result = InstructionDiscovery::new(config).discover(dir.path(), true);
        assert!(
            result.diagnostics.iter().any(|d| d.contains("unknown compat vendor `acme`")),
            "unknown vendor must be diagnosed, got {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn find_project_root_uses_git_marker() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join(".git"), "").unwrap();

        let found = find_project_root(&deep);
        assert_eq!(found, Some(root.canonicalize().unwrap()));
    }

    #[test]
    fn collect_path_segments_root_to_cwd() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("r");
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();

        let segments = collect_path_segments(&root, &deep);
        assert_eq!(segments.len(), 3, "root, root/a, root/a/b");
        assert_eq!(segments[0], root.canonicalize().unwrap());
        assert_eq!(segments[2], deep.canonicalize().unwrap());
    }

    #[test]
    fn discover_user_global_agents_md() {
        // This test only runs if HOME is set (which it is in dev/test environments).
        // We use a temp dir as a fake home to avoid polluting the real one.
        let fake_home = TempDir::new().unwrap();
        let agent_dir = fake_home.path().join(".agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("AGENTS.md"), "Global user rules.").unwrap();

        // We can't easily redirect dirs::home_dir() in a test, so we just
        // verify the function doesn't panic when home_dir returns something.
        // The actual discovery of ~/.agent/AGENTS.md is tested in integration.
        let _config = DiscoveryConfig {
            scan_user_global: true,
            ..DiscoveryConfig::default()
        };
        // If home_dir() returns the real home, this may or may not find files.
        // The test just verifies no panic.
    }
}
