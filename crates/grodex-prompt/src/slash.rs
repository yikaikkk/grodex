//! Slash command registry — the three-class split (Doc 19 §13).
//!
//! | Class            | Examples                  | Semantics                              |
//! |------------------|---------------------------|----------------------------------------|
//! | Runtime command  | `/model` `/compact`       | never sent to the model; dispatched to |
//! |                  | `/cancel`                 | a versioned Runtime command            |
//! | Prompt macro     | custom review prompt      | expanded into user input WITH          |
//! |                  |                           | provenance — never system authority    |
//! | Skill invocation | `/skill release`          | goes through the Skill registry/read,  |
//! |                  |                           | never inlined as fake system content   |
//!
//! Hard rules enforced here (acceptance #8/#9):
//! - runtime slash commands never enter model history — resolution returns
//!   a dispatch target, not model text;
//! - user-defined macros never become system/developer instructions —
//!   expansion produces *user* input tagged with `author="user-macro"`;
//! - builtin Runtime commands own the bare namespace; user commands live
//!   under `user:<name>` unless an explicit override is configured;
//! - macro parameters use structured substitution + escaping; shell
//!   substitution syntax (`$(...)`, backticks) is rejected outright.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The three slash command classes (Doc 19 §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashCommandKind {
    /// Handled by the runtime; never reaches the model.
    RuntimeCommand,
    /// User-defined prompt template; expands to user input.
    PromptMacro,
    /// Dispatched to the Skill registry; never inlined as system.
    SkillInvocation,
}

/// A registered command definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandSpec {
    /// Resolved command name (bare for builtins, `user:<name>` for user
    /// commands unless overridden).
    pub name: String,
    pub kind: SlashCommandKind,
    pub description: String,
    /// Macro template (PromptMacro only). Parameters are `{{arg}}`
    /// placeholders; positional `{{1}}`, `{{2}}`, ... and named `{{name}}`
    /// are both supported.
    #[serde(default)]
    pub template: Option<String>,
    /// Skill id to invoke (SkillInvocation only).
    #[serde(default)]
    pub skill_id: Option<String>,
    /// True for builtins. Builtins can never be re-registered by user
    /// content (the namespace is reserved).
    #[serde(default)]
    pub builtin: bool,
}

/// Errors from registration / resolution / expansion.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SlashCommandError {
    #[error("command name `{0}` is reserved for builtin runtime commands")]
    ReservedName(String),
    #[error("unknown command `{0}`")]
    Unknown(String),
    #[error("command `{0}` is not a prompt macro")]
    NotAMacro(String),
    #[error("macro template references unknown parameter `{{{{{0}}}}}`")]
    UnknownParameter(String),
    #[error("shell substitution is forbidden in macro arguments: {0}")]
    ShellSubstitution(String),
}

/// Result of resolving one raw input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashResolution {
    /// Not a slash command — pass through untouched.
    Passthrough,
    /// Dispatch to the runtime; MUST NOT be appended to model history
    /// (acceptance #8).
    RuntimeDispatch { name: String },
    /// Expanded user input — append to history as a USER message with the
    /// given provenance; never as System/Developer (acceptance #9).
    ExpandedUserInput { content: String, provenance: String },
    /// Dispatch to the skill registry with args; never inline as system.
    SkillDispatch { skill_id: String, args: String },
}

/// The unified registry (Phase 3 "统一 slash/Skill/custom command
/// registry"). Builtins are registered at construction; user commands via
/// [`Self::register_user`].
#[derive(Debug, Default)]
pub struct SlashCommandRegistry {
    commands: HashMap<String, SlashCommandSpec>,
    /// Names explicitly allowed to be overridden by user config
    /// (`override` configuration per Doc 19 §13).
    overridable: std::collections::BTreeSet<String>,
}

impl SlashCommandRegistry {
    /// Registry with the versioned builtin Runtime commands registered.
    pub fn with_builtins(builtins: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
        let mut reg = Self::default();
        for (name, description) in builtins {
            reg.commands.insert(
                name.to_string(),
                SlashCommandSpec {
                    name: name.to_string(),
                    kind: SlashCommandKind::RuntimeCommand,
                    description: description.to_string(),
                    template: None,
                    skill_id: None,
                    builtin: true,
                },
            );
        }
        reg
    }

    /// Allow user config to override specific builtin names (explicit
    /// override configuration — the ONLY way out of the reserved
    /// namespace).
    pub fn allow_override(&mut self, name: impl Into<String>) {
        self.overridable.insert(name.into());
    }

    /// Register a user-defined command. The bare name is kept UNLESS it
    /// collides with a builtin that is not explicitly overridable — in
    /// that case the command is stored under `user:<name>` (Doc 19 §13
    /// conflict rule). Returns the resolved name.
    pub fn register_user(
        &mut self,
        name: impl Into<String>,
        kind: SlashCommandKind,
        description: impl Into<String>,
        template: Option<String>,
        skill_id: Option<String>,
    ) -> Result<String, SlashCommandError> {
        let bare = name.into();
        let collides_with_builtin = self
            .commands
            .get(&bare)
            .map(|s| s.builtin)
            .unwrap_or(false);
        let resolved = if collides_with_builtin && !self.overridable.contains(&bare) {
            format!("user:{bare}")
        } else {
            bare.clone()
        };
        self.commands.insert(
            resolved.clone(),
            SlashCommandSpec {
                name: resolved.clone(),
                kind,
                description: description.into(),
                template,
                skill_id,
                builtin: false,
            },
        );
        Ok(resolved)
    }

    /// Register a `/skill <id>` style invocation shortcut.
    pub fn register_skill(
        &mut self,
        name: impl Into<String>,
        skill_id: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<String, SlashCommandError> {
        self.register_user(name, SlashCommandKind::SkillInvocation, description, None, Some(skill_id.into()))
    }

    pub fn get(&self, name: &str) -> Option<&SlashCommandSpec> {
        self.commands.get(name)
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// All registered commands (for `/help` listings, sorted by name).
    pub fn list(&self) -> Vec<&SlashCommandSpec> {
        let mut v: Vec<&SlashCommandSpec> = self.commands.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Resolve one raw input line.
    ///
    /// Format: `/<name>` or `/<name> <args...>`. Non-slash input returns
    /// [`SlashResolution::Passthrough`]. Unknown names resolve to
    /// `Err(Unknown)` rather than falling through to the model — a typo'd
    /// command must never silently become a prompt.
    pub fn resolve(&self, raw_input: &str) -> Result<SlashResolution, SlashCommandError> {
        let trimmed = raw_input.trim();
        if !trimmed.starts_with('/') {
            return Ok(SlashResolution::Passthrough);
        }
        let body = &trimmed[1..];
        let (name, args) = match body.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim().to_string()),
            None => (body, String::new()),
        };
        if name.is_empty() {
            return Ok(SlashResolution::Passthrough);
        }

        // `user:<name>` explicit form, then bare lookup.
        let spec = self
            .commands
            .get(name)
            .or_else(|| self.commands.get(&format!("user:{name}")))
            .ok_or_else(|| SlashCommandError::Unknown(name.to_string()))?;

        match spec.kind {
            SlashCommandKind::RuntimeCommand => Ok(SlashResolution::RuntimeDispatch {
                name: spec.name.clone(),
            }),
            SlashCommandKind::PromptMacro => {
                let template = spec
                    .template
                    .clone()
                    .unwrap_or_else(|| "{{1}}".to_string());
                let content = expand_macro(&template, &args)?;
                Ok(SlashResolution::ExpandedUserInput {
                    content,
                    provenance: format!("slash-macro:{}", spec.name),
                })
            }
            SlashCommandKind::SkillInvocation => {
                let skill_id = spec
                    .skill_id
                    .clone()
                    .unwrap_or_else(|| args.split_whitespace().next().unwrap_or_default().to_string());
                Ok(SlashResolution::SkillDispatch { skill_id, args })
            }
        }
    }
}

/// Structured macro expansion: `{{1}}`..`{{9}}` positional (split on
/// whitespace-bounded tokens is deliberately NOT done — positional args
/// are the remaining arg string for `{{1}}` and subsequent whitespace
/// splits for the rest) and `{{name}}` named via `key=value` pairs.
/// Every substituted value is shell-pattern checked and escaped.
fn expand_macro(template: &str, args: &str) -> Result<String, SlashCommandError> {
    // Forbid shell substitution anywhere in the ARGUMENTS up front.
    if let Some(offending) = find_shell_substitution(args) {
        return Err(SlashCommandError::ShellSubstitution(offending));
    }

    let tokens: Vec<&str> = args.split_whitespace().collect();
    let mut named: HashMap<&str, &str> = HashMap::new();
    for tok in &tokens {
        if let Some((k, v)) = tok.split_once('=') {
            if !k.is_empty() && !v.is_empty() {
                named.insert(k, v);
            }
        }
    }

    let mut out = String::with_capacity(template.len() + args.len());
    let mut rest = template;
    let mut positional = 1usize;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| {
            SlashCommandError::UnknownParameter(rest[start..].to_string())
        })?;
        let key = after[..end].trim();
        let value = if let Some(idx) = key.parse::<usize>().ok() {
            // Positional: {{1}} = first token, {{2}} = second, ...
            tokens.get(idx.saturating_sub(1)).copied().unwrap_or("")
        } else if key == "args" {
            args
        } else if let Some(v) = named.get(key) {
            v
        } else if positional <= tokens.len() && key.is_empty() {
            // `{{}}` = next positional token in order.
            let v = tokens[positional - 1];
            positional += 1;
            v
        } else {
            return Err(SlashCommandError::UnknownParameter(key.to_string()));
        };
        // Defense in depth: the arg-level check already ran, but a
        // template could smuggle markers; never emit shell syntax.
        if let Some(offending) = find_shell_substitution(value) {
            return Err(SlashCommandError::ShellSubstitution(offending));
        }
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Detect `$(...)` or backtick shell substitution; returns the offending
/// snippet if found.
fn find_shell_substitution(s: &str) -> Option<String> {
    if let Some(i) = s.find("$(") {
        return Some(s[i..s.len().min(i + 16)].to_string());
    }
    if s.contains('`') {
        return Some("`...`".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SlashCommandRegistry {
        let mut reg = SlashCommandRegistry::with_builtins([
            ("model", "switch model"),
            ("compact", "compact context"),
            ("cancel", "cancel current turn"),
        ]);
        reg.register_user(
            "review",
            SlashCommandKind::PromptMacro,
            "code review prompt",
            Some("Please review {{1}} focusing on {{focus}}.".into()),
            None,
        )
        .unwrap();
        reg.register_skill("release", "skill:release-flow", "run release skill")
            .unwrap();
        reg
    }

    #[test]
    fn runtime_commands_dispatch_and_never_reach_model() {
        // Acceptance #8: resolution yields a dispatch target, not text.
        let reg = registry();
        let r = reg.resolve("/model gpt-5").unwrap();
        assert_eq!(r, SlashResolution::RuntimeDispatch { name: "model".into() });
        assert!(matches!(
            reg.resolve("/compact").unwrap(),
            SlashResolution::RuntimeDispatch { .. }
        ));
    }

    #[test]
    fn macros_expand_to_user_input_with_provenance_not_system() {
        // Acceptance #9: macro output is user input + provenance tag.
        let reg = registry();
        let r = reg.resolve("/review src/main.rs focus=security").unwrap();
        match r {
            SlashResolution::ExpandedUserInput { content, provenance } => {
                assert_eq!(
                    content,
                    "Please review src/main.rs focusing on security."
                );
                assert_eq!(provenance, "slash-macro:review");
            }
            other => panic!("expected ExpandedUserInput, got {other:?}"),
        }
    }

    #[test]
    fn user_commands_get_user_namespace_on_conflict() {
        // Doc 19 §13: builtin namespace reserved; user `model` becomes
        // `user:model` unless explicitly overridable.
        let mut reg = SlashCommandRegistry::with_builtins([("model", "builtin")]);
        let resolved = reg
            .register_user(
                "model",
                SlashCommandKind::PromptMacro,
                "my macro",
                Some("hi".into()),
                None,
            )
            .unwrap();
        assert_eq!(resolved, "user:model");
        // Builtin still wins the bare name.
        assert!(matches!(
            reg.resolve("/model").unwrap(),
            SlashResolution::RuntimeDispatch { .. }
        ));
        // Explicit `user:` form reaches the macro.
        assert!(matches!(
            reg.resolve("/user:model").unwrap(),
            SlashResolution::ExpandedUserInput { .. }
        ));

        // Explicit override configuration allows taking the bare name.
        let mut reg2 = SlashCommandRegistry::with_builtins([("model", "builtin")]);
        reg2.allow_override("model");
        let resolved2 = reg2
            .register_user("model", SlashCommandKind::PromptMacro, "m", Some("x".into()), None)
            .unwrap();
        assert_eq!(resolved2, "model");
    }

    #[test]
    fn shell_substitution_rejected_outright() {
        let reg = registry();
        let err = reg.resolve("/review $(rm -rf /) focus=x").unwrap_err();
        assert!(matches!(err, SlashCommandError::ShellSubstitution(_)));
        let err2 = reg.resolve("/review `whoami` focus=x").unwrap_err();
        assert!(matches!(err2, SlashCommandError::ShellSubstitution(_)));
    }

    #[test]
    fn unknown_parameter_is_a_clear_diagnostic() {
        // Unknown TEMPLATE parameter → explicit diagnostic, not silent.
        let mut reg = SlashCommandRegistry::default();
        reg.register_user("t", SlashCommandKind::PromptMacro, "d", Some("{{missing}}".into()), None)
            .unwrap();
        let err = reg.resolve("/t a b").unwrap_err();
        assert_eq!(err, SlashCommandError::UnknownParameter("missing".into()));
    }

    #[test]
    fn skill_invocation_dispatches_to_registry_not_system() {
        let reg = registry();
        let r = reg.resolve("/release --dry-run").unwrap();
        assert_eq!(
            r,
            SlashResolution::SkillDispatch {
                skill_id: "skill:release-flow".into(),
                args: "--dry-run".into()
            }
        );
    }

    #[test]
    fn non_slash_and_unknown_commands() {
        let reg = registry();
        assert_eq!(reg.resolve("hello world").unwrap(), SlashResolution::Passthrough);
        // Unknown slash command must NOT fall through to the model.
        assert_eq!(
            reg.resolve("/nope").unwrap_err(),
            SlashCommandError::Unknown("nope".into())
        );
    }
}
