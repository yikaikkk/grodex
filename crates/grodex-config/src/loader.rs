//! Config file discovery and TOML loading.
//!
//! Discovers config files from standard locations (system, user, workspace)
//! and loads/parses each into a ConfigLayer with a content fingerprint.

use crate::error::ConfigError;
use crate::layer::{ConfigLayer, ConfigLayerSource};
use crate::trust::WorkspaceTrustBinding;
use crate::values::{ConfigDiagnostic, DiagnosticLevel};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Expand a leading `~` / `~/...` to the user's home directory.
///
/// Config-supplied paths (e.g. `path = "~/.grodex/memory.db"`) must never
/// reach the filesystem with a literal `~` — the shell does no expansion
/// inside config files. Paths without a leading `~` pass through as-is;
/// if the home directory cannot be resolved the literal path is returned
/// so the caller's IO error surfaces the real problem.
pub fn expand_user_path(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(trimmed));
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

/// All config file paths for a session.
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    /// System-level config (e.g. `/etc/grodex/config.toml`).
    pub system: Option<PathBuf>,
    /// Enterprise-managed config (MDM, managed service).
    pub enterprise: Option<PathBuf>,
    /// User-level config (e.g. `~/.grodex/config.toml`).
    pub user: Option<PathBuf>,
    /// User profile-specific config.
    pub profile: Option<PathBuf>,
    /// Workspace/project config (e.g. `<project>/.grodex/config.toml`).
    pub workspace: Option<PathBuf>,
}

impl ConfigPaths {
    /// Discover config paths from standard locations.
    ///
    /// Walks up from `cwd` to find the nearest `.grodex/config.toml` workspace
    /// file. System and user paths use standard platform directories.
    pub fn discover(cwd: &Path) -> Self {
        let system = Self::system_path();
        let user = Self::user_path();
        let workspace = Self::workspace_path(cwd);

        Self {
            system,
            enterprise: None,
            user,
            profile: None,
            workspace,
        }
    }

    /// System config path: `/etc/grodex/config.toml` (Linux) or equivalent.
    fn system_path() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            Some(PathBuf::from("/Library/Application Support/grodex/config.toml"))
        }
        #[cfg(target_os = "linux")]
        {
            Some(PathBuf::from("/etc/grodex/config.toml"))
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }

    /// User config path: `~/.grodex/config.toml`.
    fn user_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".grodex").join("config.toml"))
    }

    /// Walk up from `cwd` to find the nearest `.grodex/config.toml`.
    fn workspace_path(cwd: &Path) -> Option<PathBuf> {
        let mut current = cwd.to_path_buf();
        loop {
            let candidate = current.join(".grodex").join("config.toml");
            if candidate.exists() {
                return Some(candidate);
            }
            if !current.pop() {
                break;
            }
        }
        None
    }

    /// Iterator over all present config paths in load order (low→high precedence).
    pub fn ordered_paths(&self) -> Vec<(&'static str, &Path)> {
        let mut paths = Vec::new();
        if let Some(ref p) = self.system {
            paths.push(("system", p.as_path()));
        }
        if let Some(ref p) = self.enterprise {
            paths.push(("enterprise", p.as_path()));
        }
        if let Some(ref p) = self.user {
            paths.push(("user", p.as_path()));
        }
        if let Some(ref p) = self.profile {
            paths.push(("profile", p.as_path()));
        }
        if let Some(ref p) = self.workspace {
            paths.push(("workspace", p.as_path()));
        }
        paths
    }
}

/// Load a TOML file and return its parsed content + SHA-256 fingerprint.
pub fn load_toml(path: &Path) -> Result<(toml::Value, String), ConfigError> {
    let content = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
    let fingerprint = fingerprint_bytes(content.as_bytes());
    let value: toml::Value = toml::from_str(&content).map_err(|e| ConfigError::Parse {
        path: path.to_path_buf(),
        error: e.to_string(),
    })?;
    Ok((value, fingerprint))
}

/// Load a single config file into a ConfigLayer.
pub fn load_layer(source: ConfigLayerSource, path: &Path) -> Result<ConfigLayer, ConfigError> {
    let (values, fingerprint) = load_toml(path)?;
    Ok(ConfigLayer {
        source,
        values,
        fingerprint,
        disabled_reason: None,
    })
}

/// Load all layers from discovered paths in precedence order.
///
/// Missing files are silently skipped. Parse errors are reported as
/// diagnostics but the remaining layers are still loaded.
///
/// Returns `(layers, extra_diagnostics)` where `extra_diagnostics` contains
/// workspace trust quarantine / review warnings.
pub fn load_all_layers(
    paths: &ConfigPaths,
) -> Result<(Vec<ConfigLayer>, Vec<ConfigDiagnostic>), ConfigError> {
    let mut layers = Vec::new();
    let mut extra_diagnostics = Vec::new();

    let ordered = paths.ordered_paths();
    for (label, path) in ordered {
        match label {
            "workspace" => {
                let trusted = std::fs::read_to_string(path)
                    .ok()
                    .and_then(|content| toml::from_str::<toml::Value>(&content).ok())
                    .and_then(|v| {
                        v.as_table()?
                            .get("workspace")?
                            .as_table()?
                            .get("trusted")?
                            .as_bool()
                    })
                    .unwrap_or(false);

                let (toml_value, fingerprint) = load_toml(path)?;
                let high_risk_keys = high_risk_keys(&toml_value);

                let workspace_root = path.parent().unwrap_or(path);
                let binding = WorkspaceTrustBinding::compute(
                    workspace_root,
                    fingerprint.clone(),
                    high_risk_keys.clone(),
                );

                let mut layer = ConfigLayer {
                    source: ConfigLayerSource::Workspace { trusted },
                    values: toml_value,
                    fingerprint,
                    disabled_reason: None,
                };

                if !trusted {
                    layer.disabled_reason = Some(format!(
                        "workspace untrusted; quarantined (workspace binding hash: {}, high-risk keys detected: {:?})",
                        binding.binding_hash, binding.high_risk_keys
                    ));
                    extra_diagnostics.push(ConfigDiagnostic {
                        level: DiagnosticLevel::Warning,
                        key_path: "workspace".to_string(),
                        message: format!(
                            "workspace layer quarantined: not trusted; values not merged into effective config. binding = {}; high-risk = {:?}",
                            binding.binding_hash, binding.high_risk_keys
                        ),
                    });
                } else if !high_risk_keys.is_empty() {
                    extra_diagnostics.push(ConfigDiagnostic {
                        level: DiagnosticLevel::Warning,
                        key_path: "workspace".to_string(),
                        message: format!(
                            "workspace trusted but review_required: high-risk keys present; re-validate trust. binding = {}; keys = {:?}",
                            binding.binding_hash, binding.high_risk_keys
                        ),
                    });
                }

                layers.push(layer);
            }
            _ => {
                let source = match label {
                    "system" => ConfigLayerSource::System,
                    "enterprise" => ConfigLayerSource::EnterpriseManaged,
                    "user" => ConfigLayerSource::User,
                    "profile" => ConfigLayerSource::Profile("default".into()),
                    _ => continue,
                };

                match load_layer(source, path) {
                    Ok(layer) => layers.push(layer),
                    Err(ConfigError::Io(ref e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    Ok((layers, extra_diagnostics))
}

pub fn high_risk_keys(values: &toml::Value) -> Vec<String> {
    let mut keys = Vec::new();

    if let Some(table) = values.as_table() {
        if let Some(hooks) = table.get("hooks").and_then(|v| v.as_table()) {
            for subkey in hooks.keys() {
                keys.push(format!("hooks.{}", subkey));
            }
        }

        if let Some(mcp) = table.get("mcp").and_then(|v| v.as_table()) {
            if mcp.contains_key("servers") {
                keys.push("mcp.servers".to_string());
            }
        }

        if table.contains_key("skills") {
            keys.push("skills".to_string());
        }
        if table.contains_key("plugins") {
            keys.push("plugins".to_string());
        }

        if let Some(network) = table.get("network").and_then(|v| v.as_table()) {
            if network.contains_key("domains") {
                keys.push("network.domains".to_string());
            }
            if network.contains_key("allow_list") {
                keys.push("network.allow_list".to_string());
            }
            if network.contains_key("deny_list") {
                keys.push("network.deny_list".to_string());
            }
            if network.contains_key("proxy") {
                keys.push("network.proxy".to_string());
            }
        }

        if let Some(security) = table.get("security").and_then(|v| v.as_table()) {
            if security.contains_key("external_paths") {
                keys.push("security.external_paths".to_string());
            }
        }

        if let Some(sandbox) = table.get("sandbox").and_then(|v| v.as_table()) {
            if sandbox.contains_key("allow_paths_outside_workspace") {
                keys.push("sandbox.allow_paths_outside_workspace".to_string());
            }
        }

        if let Some(credential) = table.get("credential").and_then(|v| v.as_table()) {
            if credential.contains_key("insecure_storage") {
                keys.push("credential.insecure_storage".to_string());
            }
            if credential.contains_key("keychain_sharing") {
                keys.push("credential.keychain_sharing".to_string());
            }
        }

        if let Some(paths) = table.get("paths").and_then(|v| v.as_table()) {
            for (child, val) in paths {
                if let Some(s) = val.as_str() {
                    if s.starts_with("/") {
                        keys.push(format!("paths.{}", child));
                    }
                }
            }
        }
    }

    keys.sort();
    keys.dedup();
    keys
}

/// Compute a SHA-256 fingerprint for change detection.
fn fingerprint_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_user_path_leaves_absolute_and_relative_untouched() {
        assert_eq!(expand_user_path("/tmp/x.db"), PathBuf::from("/tmp/x.db"));
        assert_eq!(expand_user_path("relative/x.db"), PathBuf::from("relative/x.db"));
    }

    #[test]
    fn expand_user_path_expands_tilde_prefix() {
        let Some(home) = dirs::home_dir() else { return };
        assert_eq!(
            expand_user_path("~/.grodex/memory.db"),
            home.join(".grodex/memory.db")
        );
        assert_eq!(expand_user_path("~"), home);
    }

    #[test]
    fn expand_user_path_trims_whitespace() {
        assert_eq!(expand_user_path("  /tmp/x.db  "), PathBuf::from("/tmp/x.db"));
    }
}
