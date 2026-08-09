//! SkillCatalog — registry of all discovered skills.
//!
//! Provides listing, lookup, and prompt formatting for system prompt assembly.

use crate::discovery::SkillDiscovery;
use crate::skill::Skill;
use std::path::Path;

/// A collection of all discovered skills.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
}

impl SkillCatalog {
    /// Discover all skills from standard locations.
    pub fn discover(cwd: &Path) -> Self {
        let mut skills = Vec::new();
        // Project first (higher priority), then user.
        skills.extend(SkillDiscovery::discover_project(cwd));
        skills.extend(SkillDiscovery::discover_user());
        // Deduplicate by name (first wins = project).
        let mut seen = std::collections::HashSet::new();
        skills.retain(|s| seen.insert(s.name.clone()));
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self { skills }
    }

    /// List all skill references.
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    /// Find a skill by name.
    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Number of skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the catalog is empty.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Format all skills as a compact prompt listing.
    pub fn format_for_prompt(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut out = String::from("## Available Skills\n\n");
        for skill in &self.skills {
            out.push_str(&format!("- **{}**: {}\n", skill.name, skill.description));
        }
        out.push('\n');
        out
    }

    /// Get the full content of all skills suitable for injection into the system prompt.
    pub fn format_all_content(&self) -> String {
        let mut out = String::new();
        for skill in &self.skills {
            out.push_str(&format!("## Skill: {}\n{}\n\n", skill.name, skill.content));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_catalog_when_no_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = SkillCatalog::discover(dir.path());
        assert!(catalog.is_empty());
    }

    #[test]
    fn discover_project_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".grodex").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let mut f = std::fs::File::create(skills_dir.join("deploy.md")).unwrap();
        writeln!(
            f,
            "---\nname: deploy\ndescription: Deploy the app\n---\n# Deploy\nRun `deploy.sh`"
        )
        .unwrap();

        let mut f2 = std::fs::File::create(skills_dir.join("test.md")).unwrap();
        writeln!(f2, "# Test\nRun `cargo test`").unwrap();

        let catalog = SkillCatalog::discover(dir.path());
        assert_eq!(catalog.len(), 2);

        let deploy = catalog.find("deploy").unwrap();
        assert_eq!(deploy.description, "Deploy the app");

        let prompt = catalog.format_for_prompt();
        assert!(prompt.contains("deploy"));
        assert!(prompt.contains("test"));
    }
}
