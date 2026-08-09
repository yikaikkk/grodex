//! SkillDiscovery — scans directories for .md skill files.

use crate::skill::{Skill, SkillSource};
use std::path::Path;

/// Discover skills from standard locations.
pub struct SkillDiscovery;

impl SkillDiscovery {
    /// Scan a directory for `.md` skill files.
    pub fn scan_dir(dir: &Path, source: SkillSource) -> Vec<Skill> {
        let mut skills = Vec::new();
        let skills_dir = dir.join("skills");

        if !skills_dir.is_dir() {
            return skills;
        }

        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let skill = Skill::from_markdown(path.clone(), source, &content);
                        skills.push(skill);
                    }
                }
            }
        }

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Discover skills from the user's global `.grodex/` directory.
    pub fn discover_user() -> Vec<Skill> {
        if let Some(home) = dirs::home_dir() {
            Self::scan_dir(&home.join(".grodex"), SkillSource::User)
        } else {
            Vec::new()
        }
    }

    /// Discover skills from the project's `.grodex/` directory.
    /// Walks up from `cwd` to find the nearest `.grodex/skills/`.
    pub fn discover_project(cwd: &Path) -> Vec<Skill> {
        let mut current = cwd.to_path_buf();
        loop {
            let grodex_dir = current.join(".grodex");
            if grodex_dir.join("skills").is_dir() {
                return Self::scan_dir(&grodex_dir, SkillSource::Project);
            }
            if !current.pop() {
                break;
            }
        }
        Vec::new()
    }
}
