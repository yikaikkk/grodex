//! SkillDiscovery — scans directories for skill subdirectories.
//!
//! Expected layout:
//! ```text
//! <base>/skills/
//!   deploy/                ← one subdirectory per skill
//!     SKILL.md             ← preferred, explicit skill file
//!     assets/script.sh     ← optional supporting files (not loaded)
//!   test/
//!     README.md            ← fallback: README.md if SKILL.md missing
//! ```
//!
//! Back-compat: direct `*.md` files under `skills/` are still accepted
//! (loaded as legacy single-file skills) so existing users are not broken.

use crate::skill::{Skill, SkillSource};
use sha2::Digest;
use std::path::Path;

/// Discover skills from standard locations.
pub struct SkillDiscovery;

impl SkillDiscovery {
    /// Scan a `skills/` directory for skills.
    ///
    /// For each entry under `dir/skills/`:
    /// 1. If it is a **subdirectory**, look for `SKILL.md` (preferred) or `README.md` inside.
    ///    The skill name defaults to the subdirectory name.
    /// 2. If it is a `*.md` **file** directly under `skills/`, load it as a
    ///    legacy single-file skill (back-compat).
    pub fn scan_dir(dir: &Path, source: SkillSource, trusted: bool) -> Vec<Skill> {
        let mut skills = Vec::new();
        let skills_dir = dir.join("skills");

        if !skills_dir.is_dir() {
            return skills;
        }

        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(ft) => ft,
                    Err(_) => continue,
                };

                if file_type.is_dir() {
                    // Case 1: <skills>/<name>/  — modern directory-based skill
                    if let Some(skill) = Self::load_skill_from_dir(&path, source, trusted) {
                        skills.push(skill);
                    }
                } else if file_type.is_file()
                    && path.extension().is_some_and(|e| e == "md")
                {
                    // Case 2: <skills>/<name>.md  — legacy single-file skill (back-compat)
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let skill = Skill::from_markdown(path.clone(), source, &content, trusted);
                        skills.push(skill);
                    }
                }
            }
        }

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Load a directory-based skill.
    ///
    /// Tries `SKILL.md` first, then `README.md`. Filename stem is overridden
    /// by the enclosing directory name so `my-skill/SKILL.md` produces a skill
    /// named `my-skill` (consistent with the directory layout).
    fn load_skill_from_dir(skill_dir: &Path, source: SkillSource, trusted: bool) -> Option<Skill> {
        let candidates: [&str; 2] = ["SKILL.md", "README.md"];

        for candidate in &candidates {
            let md_path = skill_dir.join(candidate);
            if md_path.is_file() {
                if let Ok(content) = std::fs::read_to_string(&md_path) {
                    let (fm_name, fm_desc, body) = Skill::parse_frontmatter_static(&content);
                    let name = fm_name.unwrap_or_else(|| {
                        skill_dir
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown")
                            .to_string()
                    });
                    let description = fm_desc.unwrap_or_else(|| {
                        body.lines()
                            .find(|l| l.starts_with('#'))
                            .map(|l| l.trim_start_matches('#').trim().to_string())
                            .unwrap_or_else(|| "A Grodex skill".to_string())
                    });
                    let hash = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
                    return Some(Skill {
                        name,
                        description,
                        content: body.to_string(),
                        source,
                        path: md_path,
                        content_hash: Some(hash),
                        trusted,
                    });
                }
            }
        }

        None
    }

    /// Discover skills from the user's global `.grodex/` directory.
    /// User-level skills are always trusted (the user owns them).
    pub fn discover_user() -> Vec<Skill> {
        if let Some(home) = dirs::home_dir() {
            Self::scan_dir(&home.join(".grodex"), SkillSource::User, true)
        } else {
            Vec::new()
        }
    }

    /// Discover skills from the project's `.grodex/` directory.
    /// Walks up from `cwd` to find the nearest `.grodex/skills/`.
    /// Project skills are trusted only if the workspace is trusted
    /// (R14-6c: fail-closed for untrusted workspaces).
    pub fn discover_project(cwd: &Path, workspace_trusted: bool) -> Vec<Skill> {
        let mut current = cwd.to_path_buf();
        loop {
            let grodex_dir = current.join(".grodex");
            if grodex_dir.join("skills").is_dir() {
                return Self::scan_dir(&grodex_dir, SkillSource::Project, workspace_trusted);
            }
            if !current.pop() {
                break;
            }
        }
        Vec::new()
    }
}

/// Helper: expose parse_frontmatter for discovery.
impl Skill {
    /// Static (no-`self`) version of [`Skill::parse_frontmatter`] so discovery
    /// can reuse it when constructing skills from directory layouts.
    pub fn parse_frontmatter_static(raw: &str) -> (Option<String>, Option<String>, &str) {
        let trimmed = raw.trim();
        if !trimmed.starts_with("---") {
            return (None, None, trimmed);
        }

        let after_first = &trimmed[3..];
        if let Some(end) = after_first.find("\n---") {
            let frontmatter = &after_first[..end];
            let body = &after_first[end + 4..];

            let mut name = None;
            let mut description = None;

            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(value) = line.strip_prefix("name:") {
                    name = Some(value.trim().to_string());
                } else if let Some(value) = line.strip_prefix("description:") {
                    description = Some(value.trim().to_string());
                }
            }

            (name, description, body.trim())
        } else {
            (None, None, trimmed)
        }
    }
}

impl Default for SkillSource {
    fn default() -> Self {
        SkillSource::Project
    }
}
