//! Skill — a reusable instruction set loaded from the filesystem.
//!
//! Skills are markdown files with optional YAML frontmatter. They provide
//! domain knowledge and workflows to the model without being executable.

use sha2::Digest;
use std::path::PathBuf;

/// Where a skill was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// Built into the agent binary.
    Builtin,
    /// From the user's global config directory.
    User,
    /// From the current project's `.grodex/skills/` directory.
    Project,
}

/// A discovered skill.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Canonical name (derived from filename or frontmatter).
    pub name: String,
    /// Short description for listing (from frontmatter or first heading).
    pub description: String,
    /// Full skill content (markdown body).
    pub content: String,
    /// Where this skill was discovered.
    pub source: SkillSource,
    /// Filesystem path to the skill file.
    pub path: PathBuf,
    /// SHA-256 content hash for trust verification. Changes when the
    /// skill file is modified, requiring re-confirmation.
    pub content_hash: Option<String>,
}

impl Skill {
    /// Parse a skill from raw markdown text.
    ///
    /// Supports simple YAML frontmatter delimited by `---`:
    /// ```markdown
    /// ---
    /// name: my-skill
    /// description: Does something useful
    /// ---
    /// # Skill content
    /// ```
    pub fn from_markdown(path: PathBuf, source: SkillSource, raw: &str) -> Self {
        let (name, description, content) = Self::parse_frontmatter(raw);

        let name = name.unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        });

        let description = description.unwrap_or_else(|| {
            // Use first heading or first line as description.
            content
                .lines()
                .find(|l| l.starts_with('#'))
                .map(|l| l.trim_start_matches('#').trim().to_string())
                .unwrap_or_else(|| "A Grodex skill".to_string())
        });

        Self {
            name,
            description,
            content: content.to_string(),
            source,
            path,
            content_hash: Some(format!("{:x}", sha2::Sha256::digest(raw.as_bytes()))),
        }
    }

    /// Parse YAML frontmatter from markdown.
    /// Returns (name, description, body_content).
    fn parse_frontmatter(raw: &str) -> (Option<String>, Option<String>, &str) {
        let trimmed = raw.trim();
        if !trimmed.starts_with("---") {
            return (None, None, trimmed);
        }

        // Find closing `---`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_with_name() {
        let raw = "---\nname: test-skill\ndescription: A test\n---\n# Hello\nContent here";
        let skill = Skill::from_markdown(PathBuf::from("test.md"), SkillSource::Project, raw);
        assert_eq!(skill.name, "test-skill");
        assert_eq!(skill.description, "A test");
        assert!(skill.content.contains("# Hello"));
        assert!(!skill.content.contains("---"));
    }

    #[test]
    fn no_frontmatter_uses_filename() {
        let raw = "# My Skill\nSome content";
        let skill = Skill::from_markdown(PathBuf::from("my-skill.md"), SkillSource::User, raw);
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "My Skill");
    }
}
