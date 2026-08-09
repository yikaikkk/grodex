//! Structured state capsule — captures live session state at compaction time.
//!
//! Following Grok's `CompactionStateContext` + `<system-reminder>` pattern:
//! after compaction produces a summary, a state capsule is appended that
//! preserves non-conversation state (skills, tools, environment, etc.)
//! that the model needs to continue working.

/// Structured runtime authority data (doc 11 §9.4). Populated from
/// Tool journal, Todo/Plan store, file tracker, process supervisor,
/// sub-agent coordinator, Input Queue, MCP/Skill manager, and
/// MemorySnapshot. The LLM can only supplement semantic fields
/// (objective, decision_summary, unresolved_errors), not forge
/// Runtime state.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CapsuleAuthority {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_user_intent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_steps: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edited_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_processes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approvals: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_snapshot_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
}

impl CapsuleAuthority {
    /// Whether the authority block carries any structured data.
    pub fn is_empty(&self) -> bool {
        self.objective.is_none()
            && self.latest_user_intent.is_none()
            && self.pending_inputs.is_empty()
            && self.completed_steps.is_empty()
            && self.pending_steps.is_empty()
            && self.edited_files.is_empty()
            && self.observed_files.is_empty()
            && self.active_processes.is_empty()
            && self.background_tasks.is_empty()
            && self.subagents.is_empty()
            && self.approvals.is_empty()
            && self.selected_skills.is_empty()
            && self.memory_snapshot_refs.is_empty()
            && self.unresolved_errors.is_empty()
            && self.next_action.is_none()
    }
}

/// Builder for the structured state capsule.
///
/// The capsule is a `<system-reminder>` block appended after the
/// compaction summary. It captures live session state that cannot
/// be inferred from the conversation summary alone.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StateCapsule {
    sections: Vec<CapsuleSection>,
    /// Structured runtime authority (doc 11 §9.4). Rendered before
    /// free-form sections so the model sees authoritative state first.
    #[serde(default)]
    pub authority: CapsuleAuthority,
}

/// A section in the state capsule.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CapsuleSection {
    title: String,
    content: String,
}

impl StateCapsule {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install structured runtime authority (doc 11 §9.4).
    pub fn with_authority(&mut self, auth: CapsuleAuthority) {
        self.authority = auth;
    }

    /// Add a section to the capsule.
    pub fn add_section(&mut self, title: impl Into<String>, content: impl Into<String>) {
        self.sections.push(CapsuleSection {
            title: title.into(),
            content: content.into(),
        });
    }

    /// Add environment information.
    pub fn with_environment(&mut self, os: &str, shell: &str, cwd: &str, date: &str) {
        let content = format!("- OS: {os}\n- Shell: {shell}\n- Working Directory: {cwd}\n- Date: {date}");
        self.add_section("Environment", content);
    }

    /// Add available tools.
    pub fn with_tools(&mut self, tool_names: &[String]) {
        if !tool_names.is_empty() {
            let content = tool_names
                .iter()
                .map(|n| format!("- `{n}`"))
                .collect::<Vec<_>>()
                .join("\n");
            self.add_section("Available Tools", content);
        }
    }

    /// Add edited files.
    pub fn with_edited_files(&mut self, paths: &[String]) {
        if !paths.is_empty() {
            let content = paths.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
            self.add_section("Files Edited This Session", content);
        }
    }

    /// Render the complete state capsule as a system reminder.
    ///
    /// Authority fields are rendered first (doc 11 §9.4), followed by
    /// the legacy free-form sections.
    pub fn render(&self) -> String {
        let authority_block = self.render_authority();
        let has_sections = !self.sections.is_empty();

        if authority_block.is_empty() && !has_sections {
            return String::new();
        }

        let mut out = String::from("<system-reminder>\n");
        if !authority_block.is_empty() {
            out.push_str(&authority_block);
        }
        for section in &self.sections {
            out.push_str(&format!("\n## {}\n{}\n", section.title, section.content));
        }
        out.push_str("\n</system-reminder>");
        out
    }

    fn render_authority(&self) -> String {
        if self.authority.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str("\n## Runtime Authority\n");

        if let Some(obj) = &self.authority.objective {
            out.push_str(&format!("- Objective: {obj}\n"));
        }
        if let Some(intent) = &self.authority.latest_user_intent {
            out.push_str(&format!("- Latest User Intent: {intent}\n"));
        }
        render_authority_list(&mut out, "Pending Inputs", &self.authority.pending_inputs);
        render_authority_list(&mut out, "Completed Steps", &self.authority.completed_steps);
        render_authority_list(&mut out, "Pending Steps", &self.authority.pending_steps);
        render_authority_list(&mut out, "Edited Files", &self.authority.edited_files);
        render_authority_list(&mut out, "Observed Files", &self.authority.observed_files);
        render_authority_list(&mut out, "Active Processes", &self.authority.active_processes);
        render_authority_list(&mut out, "Background Tasks", &self.authority.background_tasks);
        render_authority_list(&mut out, "Subagents", &self.authority.subagents);
        render_authority_list(&mut out, "Approvals", &self.authority.approvals);
        render_authority_list(&mut out, "Selected Skills", &self.authority.selected_skills);
        render_authority_list(&mut out, "Memory Snapshot Refs", &self.authority.memory_snapshot_refs);
        render_authority_list(&mut out, "Unresolved Errors", &self.authority.unresolved_errors);
        if let Some(next) = &self.authority.next_action {
            out.push_str(&format!("- Next Action: {next}\n"));
        }
        out
    }

    /// Whether the capsule has any content.
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty() && self.authority.is_empty()
    }
}

fn render_authority_list(out: &mut String, label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let joined = items.iter().map(|i| format!("- {i}")).collect::<Vec<_>>().join("\n  ");
    out.push_str(&format!("- {label}:\n  {joined}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_capsule_renders_empty() {
        let capsule = StateCapsule::new();
        assert!(capsule.render().is_empty());
    }

    #[test]
    fn capsule_with_sections() {
        let mut capsule = StateCapsule::new();
        capsule.with_environment("linux", "bash", "/home/user/project", "2024-01-01");
        capsule.with_tools(&["read_file".into(), "exec".into()]);

        let rendered = capsule.render();
        assert!(rendered.contains("<system-reminder>"));
        assert!(rendered.contains("## Environment"));
        assert!(rendered.contains("## Available Tools"));
        assert!(rendered.contains("read_file"));
        assert!(rendered.contains("linux"));
    }

    #[test]
    fn authority_rendered_before_sections() {
        let mut capsule = StateCapsule::new();
        let mut auth = CapsuleAuthority::default();
        auth.objective = Some("ship v2".into());
        auth.edited_files = vec!["src/lib.rs".into()];
        capsule.with_authority(auth);
        capsule.add_section("Notes", "remember to test");

        let rendered = capsule.render();
        let auth_pos = rendered.find("## Runtime Authority").unwrap();
        let section_pos = rendered.find("## Notes").unwrap();
        assert!(auth_pos < section_pos, "authority must render before sections");
        assert!(rendered.contains("Objective: ship v2"));
        assert!(rendered.contains("Edited Files:"));
        assert!(rendered.contains("src/lib.rs"));
    }

    #[test]
    fn authority_only_capsule_renders() {
        let mut capsule = StateCapsule::new();
        let mut auth = CapsuleAuthority::default();
        auth.next_action = Some("run tests".into());
        auth.pending_steps = vec!["step 1".into()];
        capsule.with_authority(auth);

        assert!(!capsule.is_empty());
        let rendered = capsule.render();
        assert!(rendered.contains("<system-reminder>"));
        assert!(rendered.contains("Next Action: run tests"));
        assert!(rendered.contains("Pending Steps:"));
    }

    #[test]
    fn authority_is_empty_default() {
        assert!(CapsuleAuthority::default().is_empty());
    }

    #[test]
    fn empty_authority_does_not_render_block() {
        let mut capsule = StateCapsule::new();
        capsule.with_authority(CapsuleAuthority::default());
        capsule.add_section("Only Section", "hi");
        let rendered = capsule.render();
        assert!(!rendered.contains("## Runtime Authority"));
        assert!(rendered.contains("## Only Section"));
    }

    #[test]
    fn authority_serializes_and_round_trips() {
        let mut auth = CapsuleAuthority::default();
        auth.objective = Some("obj".into());
        auth.edited_files = vec!["a.rs".into(), "b.rs".into()];
        let json = serde_json::to_string(&auth).unwrap();
        let back: CapsuleAuthority = serde_json::from_str(&json).unwrap();
        assert_eq!(back.objective.as_deref(), Some("obj"));
        assert_eq!(back.edited_files, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }
}
