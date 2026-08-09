//! ConfigResolver — unified entry point for loading and reloading configuration.
//!
//! The resolver discovers config files, loads TOML, merges layers,
//! validates against requirements, and provides a `LoadedConfig`.

use crate::error::ConfigError;
use crate::generation::{ConfigGeneration, LoadedConfig};
use crate::loader::{ConfigPaths, load_all_layers};
use crate::merge::merge_layers;
use crate::requirements::RequirementBinding;
use std::path::Path;

/// The central config resolver.
///
/// Created once at process start. Handles both initial load and
/// subsequent reloads (when file watchers detect changes).
pub struct ConfigResolver {
    paths: ConfigPaths,
    last_config: Option<LoadedConfig>,
}

impl ConfigResolver {
    /// Discover config paths and perform the initial load.
    ///
    /// If no config files exist, returns an empty `LoadedConfig` with
    /// sensible defaults — the agent will run with built-in defaults.
    pub fn load(cwd: &Path) -> Result<LoadedConfig, ConfigError> {
        let paths = ConfigPaths::discover(cwd);
        let resolver = Self {
            paths: paths.clone(),
            last_config: None,
        };
        resolver.load_internal(paths, ConfigGeneration::initial())
    }

    /// Load from explicitly-provided paths instead of auto-discovering.
    ///
    /// Production code calls [`load`]; this entry point exists so tests can
    /// exercise the merge/load pipeline against a known, host-independent set
    /// of paths (no `~/.grodex` / system-config leakage from the dev host).
    #[cfg(test)]
    pub(crate) fn load_paths(paths: ConfigPaths) -> Result<LoadedConfig, ConfigError> {
        let resolver = Self {
            paths: paths.clone(),
            last_config: None,
        };
        resolver.load_internal(paths, ConfigGeneration::initial())
    }

    /// Reload configuration (called by the file watcher).
    ///
    /// Attempts to load and merge all layers. If loading fails, the
    /// previous valid config is preserved (last-known-good pattern).
    #[allow(dead_code)]
    pub fn reload(&mut self) -> Result<&LoadedConfig, ConfigError> {
        let paths = self.paths.clone();
        let prev_gen = self
            .last_config
            .as_ref()
            .map(|c| c.generation)
            .unwrap_or_else(ConfigGeneration::initial);

        let mut next_gen = prev_gen;
        next_gen.bump_all();

        let new_config = self.load_internal(paths, next_gen);

        match new_config {
            Ok(config) => {
                self.last_config = Some(config);
                Ok(self.last_config.as_ref().unwrap())
            }
            Err(e) => {
                // Preserve last known good config, report the error.
                if let Some(last) = &self.last_config {
                    let _ = e;
                    Ok(last)
                } else {
                    Err(e)
                }
            }
        }
    }

    fn load_internal(&self, paths: ConfigPaths, mut generation: ConfigGeneration) -> Result<LoadedConfig, ConfigError> {
        let (raw_layers, extra_loader_diags) = load_all_layers(&paths)?;
        let effective = merge_layers(&raw_layers)?;

        // Step 1. Migrate the merged TOML to the current schema version.
        // This normalizes aliases (model→model_id, provider.endpoint→base_url,
        // mcp_servers→mcp.servers, max_turns→budget.max_turns) and stamps
        // schema_version into the result.
        let migration_result = crate::migration::migrate(effective.values);

        // Step 2. Parse the Requirements plane from the migrated TOML.
        // The [requirements] table only has effect when it comes from the
        // enterprise-managed layer (or system) in production; here we parse
        // whatever the merged TOML carries, and the enforce() step below
        // overrides user values unconditionally (fail-closed by design).
        let requirements = RequirementBinding::from_toml(&migration_result.values);

        // Step 3. ENFORCE requirements against the values plane (Design Doc 18 §5).
        // This is the dual-plane "ceiling constraint" — every override produces
        // an Error/Warning diagnostic so users and admins can audit why a
        // preference was discarded.
        let (enforced_values, enforce_diagnostics) = requirements.enforce(migration_result.values);

        // Step 4. Bump relevant domain generations if requirements changed
        // the effective configuration (provider/policy/sandbox/credential are
        // all security-sensitive — the cache must be invalidated).
        if !enforce_diagnostics.is_empty() {
            // Any enforcement change affects root generation.
            generation.root = generation.root.max(effective.generation) + 1;
            // Determine which specific domain generations need bumps based on
            // which keys were overridden.
            for d in &enforce_diagnostics {
                if d.key_path.starts_with("provider")
                    || d.key_path.starts_with("model_routes") {
                    generation.provider += 1;
                }
                if d.key_path.starts_with("sandbox") {
                    generation.sandbox += 1;
                }
                if d.key_path.starts_with("features")
                    || d.key_path.starts_with("requirements.managed_deny") {
                    generation.policy += 1;
                }
                if d.key_path.starts_with("credential") {
                    generation.policy += 1; // credential = security → policy bump
                }
                if d.key_path.starts_with("mcp") {
                    generation.capability += 1; // MCP servers → capability gen
                }
            }
        } else {
            generation.root = effective.generation;
        }

        // Reconstruct EffectiveConfig with migrated+enforced values,
        // carrying merge diagnostics, migration diagnostics, AND enforcement
        // diagnostics so callers (CLI, UI) can display a full audit trail.
        let effective = crate::values::EffectiveConfig {
            values: enforced_values,
            merge_traces: effective.merge_traces,
            generation: generation.root,
            diagnostics: {
                let mut diags = effective.diagnostics;
                diags.extend(migration_result.diagnostics);
                diags.extend(enforce_diagnostics);
                diags.extend(extra_loader_diags);
                diags
            },
        };

        Ok(LoadedConfig {
            effective,
            requirements,
            generation,
            raw_layers,
            paths,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::ConfigPaths;
    use std::io::Write;
    use tempfile::TempDir;

    /// Loading with NO config files yields an empty config and zero layers.
    ///
    /// We use the host-independent `load_paths` entry point with an all-`None`
    /// `ConfigPaths` so the test is deterministic regardless of whether the
    /// host has a `~/.grodex/config.toml` or a system config. The production
    /// `load()` discovers from `~` and `cwd`, which can't be safely isolated
    /// from the dev machine's real config (edition-2024 makes `set_var`
    /// `unsafe`, and the workspace denies `unsafe`).
    #[test]
    fn load_empty_when_no_config_files() {
        let paths = ConfigPaths {
            system: None,
            enterprise: None,
            user: None,
            profile: None,
            workspace: None,
        };
        let config = ConfigResolver::load_paths(paths).unwrap();
        assert_eq!(config.effective.generation, 0);
        assert!(
            config.raw_layers.is_empty(),
            "expected zero layers, got: {:?}",
            config.raw_layers.iter().map(|l| &l.source).collect::<Vec<_>>()
        );
    }

    /// A workspace layer discovered under cwd is loaded and merged. Uses
    /// `load_paths` with an explicit workspace path (not auto-discovery) to
    /// stay host-independent.
    #[test]
    fn load_explicit_workspace_layer() {
        let dir = TempDir::new().unwrap();
        let grodex_dir = dir.path().join(".grodex");
        std::fs::create_dir_all(&grodex_dir).unwrap();
        let config_path = grodex_dir.join("config.toml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        // NOTE: workspace layer requires `[workspace] trusted = true` to be
        // merged; otherwise it is quarantined (fail-closed default).
        writeln!(f, r#"model = "claude-opus""#).unwrap();
        writeln!(f, r#""#).unwrap();
        writeln!(f, r#"[workspace]"#).unwrap();
        writeln!(f, r#"trusted = true"#).unwrap();
        drop(f);

        let paths = ConfigPaths {
            system: None,
            enterprise: None,
            user: None,
            profile: None,
            workspace: Some(config_path),
        };
        let config = ConfigResolver::load_paths(paths).unwrap();
        assert_eq!(config.raw_layers.len(), 1);
        // 'model' is migrated to 'model_id' by the v1→v2 migration pipeline.
        let table = config.effective.values.as_table().expect("effective values must be a table");
        let model_id = table
            .get("model_id")
            .unwrap_or_else(|| panic!("model_id key missing. Available keys: {:?}", table.keys().collect::<Vec<_>>()));
        assert_eq!(model_id.as_str(), Some("claude-opus"));
    }

    /// End-to-end: enterprise [requirements] table in a system layer enforces
    /// `forced_provider`, overriding the user workspace's explicit `provider`.
    /// Also verifies: domain generations bump, diagnostics are populated,
    /// and the resulting TOML carries the mandated value.
    #[test]
    fn enterprise_requirements_override_workspace_in_load_pipeline() {
        let dir = TempDir::new().unwrap();

        // 1. System layer with enterprise requirements.
        let system_dir = dir.path().join("system");
        std::fs::create_dir_all(&system_dir).unwrap();
        let system_cfg = system_dir.join("config.toml");
        {
            let mut f = std::fs::File::create(&system_cfg).unwrap();
            writeln!(
                f,
                r#"
[requirements]
forced_provider = "anthropic"
required_sandbox = "landlock"
disabled_features = ["shell_exec"]
"#
            )
            .unwrap();
        }

        // 2. Workspace layer with user preferences that conflict.
        let ws_dir = dir.path().join("project");
        let ws_grodex = ws_dir.join(".grodex");
        std::fs::create_dir_all(&ws_grodex).unwrap();
        let ws_cfg = ws_grodex.join("config.toml");
        {
            let mut f = std::fs::File::create(&ws_cfg).unwrap();
            writeln!(
                f,
                r#"
provider = "openai"
model_id = "gpt-4o"

[sandbox]
type = "none"

[features]
shell_exec = true
read_file = true

[workspace]
trusted = true
"#
            )
            .unwrap();
        }

        let paths = ConfigPaths {
            system: Some(system_cfg),
            enterprise: None,
            user: None,
            profile: None,
            workspace: Some(ws_cfg),
        };
        let loaded = ConfigResolver::load_paths(paths).unwrap();

        // 1. Forced values must win.
        assert_eq!(loaded.effective.values["provider"].as_str(), Some("anthropic"));
        assert_eq!(loaded.effective.values["sandbox"]["type"].as_str(), Some("landlock"));
        assert_eq!(
            loaded.effective.values["features"]["shell_exec"].as_bool(),
            Some(false)
        );

        // 2. Non-overridden user values must be preserved.
        assert_eq!(loaded.effective.values["model_id"].as_str(), Some("gpt-4o"));
        assert_eq!(
            loaded.effective.values["features"]["read_file"].as_bool(),
            Some(true)
        );

        // 3. Diagnostics should contain the override records.
        let override_diags: Vec<_> = loaded
            .effective
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("requirement override"))
            .collect();
        assert!(
            override_diags.len() >= 3,
            "expected 3+ requirement override diagnostics, got: {:?}",
            loaded.effective.diagnostics
        );

        // 4. Domain generations must have been bumped (root > initial,
        // provider/policy/sandbox bumped due to enforcement).
        assert!(
            loaded.generation.root > 1,
            "root generation should be bumped after enforcement override"
        );
        assert!(
            loaded.generation.provider > 1,
            "provider generation should be bumped: forced_provider override"
        );
        assert!(
            loaded.generation.sandbox > 1,
            "sandbox generation should be bumped: required_sandbox override"
        );
        assert!(
            loaded.generation.policy > 1,
            "policy generation should be bumped: disabled_features override"
        );

        // 5. RequirementBinding is populated (not empty default).
        assert!(
            !loaded.requirements.is_empty(),
            "requirements should be populated from merged config"
        );
        assert_eq!(
            loaded.requirements.forced_provider.as_deref(),
            Some("anthropic")
        );
    }
}
