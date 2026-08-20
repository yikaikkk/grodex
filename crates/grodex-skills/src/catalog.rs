//! SkillCatalog — registry of all discovered skills.
//!
//! Provides listing, lookup, and prompt formatting for system prompt assembly.

use crate::discovery::SkillDiscovery;
use crate::lint::{lint_catalog, LintReport};
use crate::skill::{Skill, SkillSource};
use std::path::{Path, PathBuf};

/// A collection of all discovered skills.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    /// Lint reports from the last discovery run. Skills with error-level
    /// findings are excluded from the catalog entirely.
    lint_reports: Vec<LintReport>,
}

/// A frozen snapshot of a single skill at Turn-start time.
/// Once a Turn begins, the skill content cannot change mid-Turn even
/// if the underlying file is modified on disk (Design Doc 08 §6).
#[derive(Debug, Clone)]
pub struct SkillSnapshot {
    pub name: String,
    pub source: SkillSource,
    pub path: PathBuf,
    pub content_hash: String,
    pub content: String,
}

impl SkillCatalog {
    /// Discover all skills from standard locations.
    /// Runs lint during discovery: skills with error-level findings
    /// are excluded from the catalog (Design Doc 08 §7: "Lint 接入").
    pub fn discover(cwd: &Path) -> Self {
        let mut skills = Vec::new();
        // Project first (higher priority), then user.
        skills.extend(SkillDiscovery::discover_project(cwd));
        skills.extend(SkillDiscovery::discover_user());
        // Deduplicate by name (first wins = project).
        let mut seen = std::collections::HashSet::new();
        skills.retain(|s| seen.insert(s.name.clone()));
        skills.sort_by(|a, b| a.name.cmp(&b.name));

        // Run lint and filter out error-level skills.
        let reports = lint_catalog(&skills);
        let error_names: std::collections::HashSet<&str> = reports
            .iter()
            .filter(|r| !r.is_indexable())
            .map(|r| r.skill_name.as_str())
            .collect();
        skills.retain(|s| !error_names.contains(s.name.as_str()));

        Self {
            skills,
            lint_reports: reports,
        }
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
    /// Includes the full skill content so the model can actually
    /// follow the instructions (Design Doc 08 §6: "正文加载").
    pub fn format_for_prompt(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut out = String::from("## Available Skills\n\n");
        for skill in &self.skills {
            out.push_str(&format!(
                "### {}\n**Description**: {}\n\n{}\n\n",
                skill.name, skill.description, skill.content
            ));
        }
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

    /// Freeze a Turn-level snapshot of all skills (Design Doc 08 §6).
    /// Once a Turn starts, the snapshot is immutable — mid-Turn file
    /// changes cannot affect the in-progress Turn.
    pub fn snapshot(&self) -> Vec<SkillSnapshot> {
        self.skills
            .iter()
            .map(|s| SkillSnapshot {
                name: s.name.clone(),
                source: s.source,
                path: s.path.clone(),
                content_hash: s.content_hash.clone().unwrap_or_default(),
                content: s.content.clone(),
            })
            .collect()
    }

    /// Return the lint reports from the last discovery run.
    pub fn lint_reports(&self) -> &[LintReport] {
        &self.lint_reports
    }

    /// Detect whether any skill content has changed since a prior
    /// snapshot set (Design Doc 08 §6: "变更检测"). Returns true if
    /// any hash differs or the set of skill names changed.
    pub fn has_changed_since(&self, prev: &[SkillSnapshot]) -> bool {
        if self.skills.len() != prev.len() {
            return true;
        }
        for skill in &self.skills {
            let Some(prev_skill) = prev.iter().find(|p| p.name == skill.name) else {
                return true;
            };
            let current_hash = skill.content_hash.as_deref().unwrap_or("");
            if current_hash != prev_skill.content_hash {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::SkillDiscovery;
    use crate::skill::SkillSource;
    use std::io::Write;

    /// Build a project-only catalog (without discover_user leakage from
    /// whatever `~/.grodex/skills` the developer has locally).
    fn project_catalog(cwd: &std::path::Path) -> SkillCatalog {
        let mut skills = SkillDiscovery::discover_project(cwd);
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        let mut seen = std::collections::HashSet::new();
        skills.retain(|s| seen.insert(s.name.clone()));
        let reports = crate::lint::lint_catalog(&skills);
        let error_names: std::collections::HashSet<&str> = reports
            .iter()
            .filter(|r| !r.is_indexable())
            .map(|r| r.skill_name.as_str())
            .collect();
        skills.retain(|s| !error_names.contains(s.name.as_str()));
        SkillCatalog {
            skills,
            lint_reports: reports,
        }
    }

    #[test]
    fn empty_catalog_when_no_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = project_catalog(dir.path());
        assert!(catalog.is_empty());
    }

    /// Modern layout: one subdirectory per skill, SKILL.md inside.
    #[test]
    fn discover_directory_based_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".grodex").join("skills");

        // Skill 1: deploy/ with SKILL.md + frontmatter
        let deploy_dir = skills_dir.join("deploy");
        std::fs::create_dir_all(&deploy_dir).unwrap();
        let mut f = std::fs::File::create(deploy_dir.join("SKILL.md")).unwrap();
        writeln!(
            f,
            "---\nname: deploy\ndescription: Deploy the app\n---\n# Deploy\nRun `deploy.sh`"
        )
        .unwrap();
        // Supporting files (should be ignored during loading but present in dir).
        std::fs::create_dir_all(deploy_dir.join("assets")).unwrap();
        std::fs::write(deploy_dir.join("assets").join("helper.sh"), "#!/bin/sh\necho hi").unwrap();

        // Skill 2: test/ with README.md (no SKILL.md fallback)
        let test_dir = skills_dir.join("test");
        std::fs::create_dir_all(&test_dir).unwrap();
        let mut f2 = std::fs::File::create(test_dir.join("README.md")).unwrap();
        writeln!(f2, "# Test Skill\nRun `cargo test` always").unwrap();

        let catalog = project_catalog(dir.path());
        assert_eq!(catalog.len(), 2, "expected 2 directory-based skills, got {:?}", catalog.list().iter().map(|s| s.name.as_str()).collect::<Vec<_>>());

        let deploy = catalog.find("deploy").expect("deploy skill not found");
        assert_eq!(deploy.description, "Deploy the app");
        assert!(deploy.content.contains("Run `deploy.sh`"));
        assert!(deploy
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n == "SKILL.md"));

        let test = catalog.find("test").expect("test skill not found");
        assert_eq!(test.description, "Test Skill");
        assert!(test.content.contains("cargo test"));

        let prompt = catalog.format_for_prompt();
        assert!(prompt.contains("deploy"));
        assert!(prompt.contains("test"));
    }

    /// Legacy back-compat: loose *.md files directly under skills/ still work.
    #[test]
    fn discover_legacy_single_file_skills() {
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

        let catalog = project_catalog(dir.path());
        assert_eq!(catalog.len(), 2, "legacy *.md files should still load");

        let deploy = catalog.find("deploy").unwrap();
        assert_eq!(deploy.description, "Deploy the app");

        let prompt = catalog.format_for_prompt();
        assert!(prompt.contains("deploy"));
        assert!(prompt.contains("test"));
    }

    /// Mixed layout: directory-based and legacy single-file skills coexist.
    /// Project-level duplicates are resolved by name (first wins).
    #[test]
    fn mixed_layout_directory_and_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join(".grodex").join("skills");

        // Dir-based deploy.
        let deploy_dir = skills_dir.join("deploy");
        std::fs::create_dir_all(&deploy_dir).unwrap();
        let mut f = std::fs::File::create(deploy_dir.join("SKILL.md")).unwrap();
        writeln!(f, "# Dir Deploy\nDir version").unwrap();

        // Legacy deploy.md — should be de-duplicated out.
        let mut fl = std::fs::File::create(skills_dir.join("deploy.md")).unwrap();
        writeln!(fl, "# File Deploy\nLegacy version").unwrap();

        let catalog = project_catalog(dir.path());
        assert_eq!(catalog.len(), 1, "expected one de-duplicated deploy, got {:?}",
            catalog.list().iter().map(|s| s.name.as_str()).collect::<Vec<_>>());
        assert!(catalog.find("deploy").is_some());
    }

    // Ensure SkillSource default compiles and works (keeps derive happy).
    #[test]
    fn skill_source_default() {
        let s = <SkillSource as Default>::default();
        assert_eq!(s, SkillSource::Project);
    }
}
