//! Grodex Skills — skill discovery and management.
//!
//! Skills are reusable instruction sets loaded from `.grodex/skills/`
//! directories. They provide domain knowledge and workflows to the
//! model without being executable tools.

pub mod catalog;
pub mod discovery;
pub mod lint;
pub mod skill;

pub use catalog::{SkillCatalog, SkillSnapshot};
pub use discovery::SkillDiscovery;
pub use lint::{lint_catalog, lint_skill, LintCode, LintFinding, LintReport, LintSeverity};
pub use skill::{Skill, SkillSource};
