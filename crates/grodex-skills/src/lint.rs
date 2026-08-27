//! Skill Lint — discoverability and quality checks for skills (Design 08 §7).
//!
//! The `lint_warning_count` column already exists in the `skills` SQLite table
//! (schema.rs line 49). This module provides the lint logic that populates it.
//!
//! Lint findings are categorised by severity:
//! - **Error**: skill is unusable; must fix before indexing.
//! - **Warning**: skill works but has quality / discoverability issues.
//! - **Info**: suggestion for better discoverability.

use crate::skill::Skill;
use std::collections::HashMap;

/// Severity of a lint finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity {
    /// Skill is unusable; must fix before indexing.
    Error,
    /// Skill works but has quality issues; should fix.
    Warning,
    /// Suggestion for better discoverability.
    Info,
}

/// Categorized lint codes for skill quality and discoverability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintCode {
    /// Missing name in frontmatter.
    MissingName,
    /// Missing or empty description.
    MissingDescription,
    /// Description too short (< 10 chars) for discoverability.
    DescriptionTooShort,
    /// Description too long (> 200 chars).
    DescriptionTooLong,
    /// Missing trigger keywords.
    MissingTriggers,
    /// Body content is empty.
    EmptyBody,
    /// Body content too short (< 50 chars).
    BodyTooShort,
    /// No examples provided.
    MissingExamples,
    /// Name doesn't follow kebab-case convention.
    InvalidNameFormat,
    /// Duplicate skill name detected.
    DuplicateName,
}

/// A single lint finding.
#[derive(Debug, Clone)]
pub struct LintFinding {
    pub severity: LintSeverity,
    pub code: LintCode,
    pub message: String,
}

/// Result of linting a skill.
#[derive(Debug, Clone)]
pub struct LintReport {
    pub skill_name: String,
    pub findings: Vec<LintFinding>,
}

impl LintReport {
    pub fn error_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == LintSeverity::Error)
            .count()
    }
    pub fn warning_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.severity == LintSeverity::Warning)
            .count()
    }
    pub fn is_indexable(&self) -> bool {
        self.error_count() == 0
    }
}

/// Lint a single skill for quality and discoverability issues.
///
/// The `Skill` struct (see `skill.rs`) exposes `name`, `description`, and
/// `content` (the markdown body). There is no dedicated `triggers` or
/// `examples` field, so those checks are adapted to scan the body content
/// heuristically for trigger / example sections.
pub fn lint_skill(skill: &Skill) -> LintReport {
    let mut findings = Vec::new();

    // 1. name exists and follows kebab-case
    if skill.name.is_empty() || skill.name == "unknown" {
        findings.push(LintFinding {
            severity: LintSeverity::Error,
            code: LintCode::MissingName,
            message: "Skill is missing a name in frontmatter".into(),
        });
    } else if !is_kebab_case(&skill.name) {
        findings.push(LintFinding {
            severity: LintSeverity::Warning,
            code: LintCode::InvalidNameFormat,
            message: format!(
                "Skill name '{}' does not follow kebab-case convention (lowercase, digits, hyphens)",
                skill.name
            ),
        });
    }

    // 2. description exists, 10-200 chars
    if skill.description.is_empty() || skill.description == "A Grodex skill" {
        findings.push(LintFinding {
            severity: LintSeverity::Error,
            code: LintCode::MissingDescription,
            message: "Skill is missing a description".into(),
        });
    } else if skill.description.len() < 10 {
        findings.push(LintFinding {
            severity: LintSeverity::Warning,
            code: LintCode::DescriptionTooShort,
            message: format!(
                "Description too short ({} chars < 10); harms discoverability",
                skill.description.len()
            ),
        });
    } else if skill.description.len() > 200 {
        findings.push(LintFinding {
            severity: LintSeverity::Warning,
            code: LintCode::DescriptionTooLong,
            message: format!(
                "Description too long ({} chars > 200)",
                skill.description.len()
            ),
        });
    }

    // 3-5. Body checks (progressive disclosure: skip when content is None
    //      = not loaded at discover time. If content is Some(""), body is
    //      explicitly empty → report error).
    if let Some(content_str) = skill.content.as_deref() {
        if !has_trigger_section(content_str) {
            findings.push(LintFinding {
                severity: LintSeverity::Warning,
                code: LintCode::MissingTriggers,
                message: "No trigger keywords section found; add '## Triggers' or '## When to Use' for better discoverability".into(),
            });
        }

        // 4. body non-empty, >= 50 chars
        if content_str.is_empty() {
            findings.push(LintFinding {
                severity: LintSeverity::Error,
                code: LintCode::EmptyBody,
                message: "Skill body content is empty".into(),
            });
        } else if content_str.len() < 50 {
            findings.push(LintFinding {
                severity: LintSeverity::Warning,
                code: LintCode::BodyTooShort,
                message: format!(
                    "Skill body too short ({} chars < 50)",
                    content_str.len()
                ),
            });
        }

        // 5. examples present (Info level)
        if !has_examples(content_str) {
            findings.push(LintFinding {
                severity: LintSeverity::Info,
                code: LintCode::MissingExamples,
                message: "No examples found; consider adding '## Examples' or a code block for clarity".into(),
            });
        }
    }

    LintReport {
        skill_name: skill.name.clone(),
        findings,
    }
}

/// Lint a collection of skills, checking for duplicates and cross-skill issues.
pub fn lint_catalog(skills: &[Skill]) -> Vec<LintReport> {
    let mut reports: Vec<LintReport> = skills.iter().map(lint_skill).collect();

    // Count occurrences of each name to flag duplicates.
    let mut name_count: HashMap<&str, usize> = HashMap::new();
    for s in skills {
        *name_count.entry(s.name.as_str()).or_default() += 1;
    }

    // Append DuplicateName finding to every report whose name appears > 1 time.
    for report in &mut reports {
        if let Some(count) = name_count.get(report.skill_name.as_str()) {
            if *count > 1 {
                report.findings.push(LintFinding {
                    severity: LintSeverity::Error,
                    code: LintCode::DuplicateName,
                    message: format!(
                        "Duplicate skill name '{}' detected {} times",
                        report.skill_name, count
                    ),
                });
            }
        }
    }

    reports
}

// ── Helpers ─────────────────────────────────────────────────────

/// Check if a name follows kebab-case: lowercase ascii letters, digits, and
/// hyphens; must not start or end with a hyphen; must not be empty.
fn is_kebab_case(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Heuristically detect a triggers / when-to-use / keywords section in the
/// skill body. The `Skill` struct has no dedicated triggers field, so we scan
/// for headings like `## Triggers`, `## When to Use`, or `## Keywords`.
fn has_trigger_section(content: &str) -> bool {
    content.lines().any(|l| {
        let l = l.trim().to_lowercase();
        l.starts_with("## trigger")
            || l.starts_with("## when")
            || l.starts_with("## keyword")
    })
}

/// Heuristically detect examples: a fenced code block (```) or an
/// `## Example` / `## Examples` heading.
fn has_examples(content: &str) -> bool {
    if content.contains("```") {
        return true;
    }
    content.lines().any(|l| {
        l.trim()
            .to_lowercase()
            .starts_with("## example")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, SkillSource};
    use std::path::PathBuf;

    fn make_skill(name: &str, description: &str, content: &str) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            content: Some(content.into()),
            source: SkillSource::Project,
            path: PathBuf::from("test.md"),
            content_hash: None,
            trusted: true,
        }
    }

    const GOOD_BODY: &str = "\
## When to Use
Use this skill when you need to deploy the application to a production server.

## Steps
1. Run the deploy script.
2. Verify the deployment.

```bash
deploy.sh --prod
```
";

    #[test]
    fn complete_skill_has_no_errors() {
        let skill = make_skill(
            "deploy-app",
            "Deploys the application to the production environment",
            GOOD_BODY,
        );
        let report = lint_skill(&skill);
        assert_eq!(report.error_count(), 0);
        assert_eq!(report.warning_count(), 0);
        assert!(report.is_indexable());
    }

    #[test]
    fn missing_name_is_error() {
        let skill = make_skill(
            "",
            "A valid description here",
            "Some body content that is long enough to pass the fifty char minimum.",
        );
        let report = lint_skill(&skill);
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == LintCode::MissingName && f.severity == LintSeverity::Error));
        assert!(!report.is_indexable());
    }

    #[test]
    fn unknown_name_is_error() {
        let skill = make_skill(
            "unknown",
            "A valid description here",
            "Some body content that is long enough to pass the fifty char minimum.",
        );
        let report = lint_skill(&skill);
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == LintCode::MissingName));
    }

    #[test]
    fn description_too_short_is_warning() {
        let skill = make_skill(
            "my-skill",
            "short",
            "Some body content that is long enough to pass the fifty char minimum.",
        );
        let report = lint_skill(&skill);
        assert!(report.findings.iter().any(|f| {
            f.code == LintCode::DescriptionTooShort && f.severity == LintSeverity::Warning
        }));
        // Warnings alone don't block indexing.
        assert!(report.is_indexable());
    }

    #[test]
    fn description_too_long_is_warning() {
        let long_desc = "x".repeat(201);
        let skill = make_skill("my-skill", &long_desc, GOOD_BODY);
        let report = lint_skill(&skill);
        assert!(report.findings.iter().any(|f| {
            f.code == LintCode::DescriptionTooLong && f.severity == LintSeverity::Warning
        }));
    }

    #[test]
    fn empty_body_is_error() {
        let skill = make_skill("my-skill", "A valid description here", "");
        let report = lint_skill(&skill);
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == LintCode::EmptyBody && f.severity == LintSeverity::Error));
        assert!(!report.is_indexable());
    }

    #[test]
    fn body_too_short_is_warning() {
        let skill = make_skill("my-skill", "A valid description here", "tiny");
        let report = lint_skill(&skill);
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == LintCode::BodyTooShort && f.severity == LintSeverity::Warning));
    }

    #[test]
    fn duplicate_name_is_error_in_catalog() {
        let skill1 = make_skill(
            "dup-skill",
            "Description one is long enough",
            "Body content that is long enough to pass the minimum.",
        );
        let skill2 = make_skill(
            "dup-skill",
            "Description two is long enough",
            "Body content that is long enough to pass the minimum.",
        );
        let reports = lint_catalog(&[skill1, skill2]);
        assert_eq!(reports.len(), 2);
        for report in &reports {
            assert!(report
                .findings
                .iter()
                .any(|f| f.code == LintCode::DuplicateName && f.severity == LintSeverity::Error));
            assert!(!report.is_indexable());
        }
    }

    #[test]
    fn no_duplicate_when_names_differ() {
        let skill1 = make_skill(
            "skill-alpha",
            "Description one is long enough",
            "Body content that is long enough to pass the minimum.",
        );
        let skill2 = make_skill(
            "skill-beta",
            "Description two is long enough",
            "Body content that is long enough to pass the minimum.",
        );
        let reports = lint_catalog(&[skill1, skill2]);
        for report in &reports {
            assert!(!report
                .findings
                .iter()
                .any(|f| f.code == LintCode::DuplicateName));
        }
    }

    #[test]
    fn missing_triggers_is_warning() {
        let skill = make_skill(
            "my-skill",
            "A valid description here",
            "Some body content without a triggers section but long enough to pass.",
        );
        let report = lint_skill(&skill);
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == LintCode::MissingTriggers && f.severity == LintSeverity::Warning));
    }

    #[test]
    fn missing_examples_is_info() {
        let skill = make_skill(
            "my-skill",
            "A valid description here",
            "## When to Use\nUse when needed.\nNo code block or examples here at all.",
        );
        let report = lint_skill(&skill);
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == LintCode::MissingExamples && f.severity == LintSeverity::Info));
        // Info findings never block indexing.
        assert!(report.is_indexable());
    }

    #[test]
    fn invalid_name_format_is_warning() {
        let skill = make_skill(
            "My_Skill",
            "A valid description here",
            "Some body content that is long enough to pass the fifty char minimum.",
        );
        let report = lint_skill(&skill);
        assert!(report
            .findings
            .iter()
            .any(|f| f.code == LintCode::InvalidNameFormat && f.severity == LintSeverity::Warning));
    }

    #[test]
    fn kebab_case_detection() {
        assert!(is_kebab_case("deploy-app"));
        assert!(is_kebab_case("my-skill-123"));
        assert!(is_kebab_case("single"));
        assert!(!is_kebab_case(""));
        assert!(!is_kebab_case("-leading"));
        assert!(!is_kebab_case("trailing-"));
        assert!(!is_kebab_case("My_Skill"));
        assert!(!is_kebab_case("CamelCase"));
        assert!(!is_kebab_case("has space"));
    }

    #[test]
    fn warning_count_populated_correctly() {
        let skill = make_skill(
            "my-skill",
            "short", // DescriptionTooShort warning
            "tiny",  // BodyTooShort warning + MissingTriggers warning + MissingExamples info
        );
        let report = lint_skill(&skill);
        assert_eq!(report.error_count(), 0);
        assert!(report.warning_count() >= 3);
        assert!(report.is_indexable());
    }
}
