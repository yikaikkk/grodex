//! Requirements plane — non-overridable enterprise constraints.
//!
//! Unlike the values plane, requirements use a ceiling/floor model: user
//! values that conflict with requirements are constrained with diagnostics,
//! never silently clipped. The [`RequirementBinding::enforce`] method applies
//! requirements to a merged TOML config, producing diagnostics for every
//! override applied.

use serde::{Deserialize, Serialize};
use crate::values::{ConfigDiagnostic, DiagnosticLevel};

/// Enterprise-managed constraints that user config cannot override.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RequirementBinding {
    /// Patterns or paths that are always denied regardless of user config.
    pub managed_deny: Vec<String>,
    /// If set, the only allowed model provider.
    pub forced_provider: Option<String>,
    /// If set, the required sandbox type.
    pub required_sandbox: Option<String>,
    /// Features that are forcibly disabled.
    pub disabled_features: Vec<String>,
    /// Allowed MCP server sources (e.g. `["github.com/my-org/*"]`).
    pub allowed_mcp_sources: Vec<String>,
    /// If true, credentials must be stored in the OS keychain.
    pub require_keychain_storage: bool,
}

impl RequirementBinding {
    /// Returns `true` if the given feature is disabled by requirements.
    pub fn is_feature_disabled(&self, feature: &str) -> bool {
        self.disabled_features.iter().any(|f| f == feature)
    }

    /// Parse a `RequirementBinding` from the `[requirements]` table of a
    /// merged TOML config. Only enterprise-managed layers should populate
    /// this — user/workspace `[requirements]` tables are ignored by the
    /// resolver (requirements are non-overridable ceiling constraints, not
    /// user preferences).
    ///
    /// Missing fields default to their `Default` values.
    pub fn from_toml(values: &toml::Value) -> Self {
        let Some(table) = values.as_table() else {
            return Self::default();
        };
        let Some(req_table) = table.get("requirements").and_then(|v| v.as_table()) else {
            return Self::default();
        };

        Self {
            managed_deny: req_table
                .get("managed_deny")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            forced_provider: req_table
                .get("forced_provider")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            required_sandbox: req_table
                .get("required_sandbox")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            disabled_features: req_table
                .get("disabled_features")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            allowed_mcp_sources: req_table
                .get("allowed_mcp_sources")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            require_keychain_storage: req_table
                .get("require_keychain_storage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }
    }

    /// Returns `true` if this binding has no constraints (the empty/default state).
    pub fn is_empty(&self) -> bool {
        self.managed_deny.is_empty()
            && self.forced_provider.is_none()
            && self.required_sandbox.is_none()
            && self.disabled_features.is_empty()
            && self.allowed_mcp_sources.is_empty()
            && !self.require_keychain_storage
    }

    /// Apply non-overridable requirements to the merged values TOML.
    ///
    /// Returns the (possibly modified) TOML value and a list of diagnostics
    /// describing every override applied. User-preference values that
    /// conflict with requirements are replaced with the required value and
    /// marked with an `Error`-level diagnostic (fail-closed semantics for
    /// security-sensitive requirements).
    ///
    /// # Enforcement order (Design Doc 18 §5)
    ///   1. `forced_provider` → override `provider` at top-level + `model_routes.*` candidates
    ///   2. `required_sandbox` → override `sandbox.type`
    ///   3. `disabled_features` → set `features.<name> = false` (and strip `true`)
    ///   4. `require_keychain_storage` → set `credential.storage = "keychain"`
    ///   5. `allowed_mcp_sources` → filter `mcp.servers` entries by glob
    ///   6. `managed_deny` → informational diagnostic (enforced by permission layer)
    pub fn enforce(&self, mut values: toml::Value) -> (toml::Value, Vec<ConfigDiagnostic>) {
        let mut diags = Vec::new();

        if self.is_empty() {
            return (values, diags);
        }

        let table = match values.as_table_mut() {
            Some(t) => t,
            None => return (values, diags),
        };

        // 1. forced_provider — override `provider` top-level, and also the
        //    `model_routes.*.candidates[*].provider_id` so failover never
        //    escapes the mandated provider.
        if let Some(ref forced) = self.forced_provider {
            let existing_provider = table.get("provider").and_then(|v| v.as_str());
            if existing_provider.map_or(true, |p| p != forced) {
                if let Some(prev) = existing_provider {
                    diags.push(ConfigDiagnostic {
                        level: DiagnosticLevel::Error,
                        key_path: "provider".into(),
                        message: format!(
                            "requirement override: provider '{prev}' → '{forced}' (enterprise forced_provider, cannot be overridden)"
                        ),
                    });
                }
                table.insert("provider".into(), toml::Value::String(forced.clone()));
            }
            // Also constrain model_routes candidates.
            if let Some(routes) = table.get_mut("model_routes").and_then(|v| v.as_table_mut()) {
                for (_name, route_val) in routes.iter_mut() {
                    if let Some(route_table) = route_val.as_table_mut() {
                        if let Some(candidates) = route_table.get_mut("candidates").and_then(|v| v.as_array_mut()) {
                            for cand in candidates.iter_mut() {
                                if let Some(cand_table) = cand.as_table_mut() {
                                    if let Some(pid_val) = cand_table.get("provider_id") {
                                        if pid_val.as_str().map_or(true, |p| p != forced) {
                                            cand_table.insert(
                                                "provider_id".into(),
                                                toml::Value::String(forced.clone()),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. required_sandbox — override sandbox.type.
        if let Some(ref required) = self.required_sandbox {
            let sb = table
                .entry::<String>("sandbox".into())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(sandbox_table) = sb.as_table_mut() {
                let existing = sandbox_table.get("type").and_then(|v| v.as_str());
                if existing.map_or(true, |t| t != required) {
                    if let Some(prev) = existing {
                        diags.push(ConfigDiagnostic {
                            level: DiagnosticLevel::Error,
                            key_path: "sandbox.type".into(),
                            message: format!(
                                "requirement override: sandbox.type '{prev}' → '{required}' (enterprise required_sandbox, cannot be overridden)"
                            ),
                        });
                    }
                    sandbox_table.insert("type".into(), toml::Value::String(required.clone()));
                }
            }
        }

        // 3. disabled_features — each disabled feature is forced to false in
        //    the [features] table. If the user explicitly set it to true, log
        //    an override diagnostic.
        if !self.disabled_features.is_empty() {
            let features = table
                .entry::<String>("features".into())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(feat_table) = features.as_table_mut() {
                for feat in &self.disabled_features {
                    let prev = feat_table.get(feat).and_then(|v| v.as_bool());
                    let was_true = prev.unwrap_or(false);
                    if was_true || prev.is_none() {
                        if was_true {
                            diags.push(ConfigDiagnostic {
                                level: DiagnosticLevel::Error,
                                key_path: format!("features.{feat}"),
                                message: format!(
                                    "requirement override: features.{feat} = true → false (enterprise disabled_features, cannot be re-enabled)"
                                ),
                            });
                        }
                        feat_table.insert(feat.clone(), toml::Value::Boolean(false));
                    }
                }
            }
        }

        // 4. require_keychain_storage — credential.storage forced to "keychain".
        if self.require_keychain_storage {
            let cred = table
                .entry::<String>("credential".into())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(cred_table) = cred.as_table_mut() {
                let existing = cred_table.get("storage").and_then(|v| v.as_str());
                if existing.map_or(true, |s| s != "keychain") {
                    if let Some(prev) = existing {
                        diags.push(ConfigDiagnostic {
                            level: DiagnosticLevel::Error,
                            key_path: "credential.storage".into(),
                            message: format!(
                                "requirement override: credential.storage '{prev}' → 'keychain' (enterprise require_keychain_storage, cannot be overridden)"
                            ),
                        });
                    }
                    cred_table.insert("storage".into(), toml::Value::String("keychain".into()));
                }
            }
        }

        // 5. allowed_mcp_sources — filter mcp.servers by glob match against
        //    each server's `source` (or `command`, if source is absent).
        if !self.allowed_mcp_sources.is_empty() {
            if let Some(mcp) = table.get_mut("mcp").and_then(|v| v.as_table_mut()) {
                if let Some(servers) = mcp.get_mut("servers").and_then(|v| v.as_array_mut()) {
                    let mut removed = 0usize;
                    let allowed_globs: Vec<&str> = self.allowed_mcp_sources.iter().map(|s| s.as_str()).collect();
                    servers.retain(|server_val| {
                        let src = server_val
                            .as_table()
                            .and_then(|t| t.get("source").and_then(|v| v.as_str()))
                            .or_else(|| server_val.as_table().and_then(|t| t.get("command").and_then(|v| v.as_str())))
                            .unwrap_or("");
                        let allowed = allowed_globs.iter().any(|glob| glob_match(glob, src));
                        if !allowed {
                            removed += 1;
                        }
                        allowed
                    });
                    if removed > 0 {
                        diags.push(ConfigDiagnostic {
                            level: DiagnosticLevel::Warning,
                            key_path: "mcp.servers".into(),
                            message: format!(
                                "requirement filter: removed {removed} MCP server(s) not matching allowed_mcp_sources {:?}",
                                self.allowed_mcp_sources
                            ),
                        });
                    }
                }
            }
        }

        // 6. managed_deny — informational; the permission broker enforces at
        //    call time. Just produce a single Info diagnostic listing the
        //    patterns so admins can audit via diagnostics.
        if !self.managed_deny.is_empty() {
            diags.push(ConfigDiagnostic {
                level: DiagnosticLevel::Info,
                key_path: "requirements.managed_deny".into(),
                message: format!(
                    "managed_deny patterns loaded ({} path(s), enforced at permission layer): {:?}",
                    self.managed_deny.len(),
                    self.managed_deny
                ),
            });
        }

        (values, diags)
    }
}

/// Simple glob matcher supporting `*` (any chars except `/`) and `**` (any chars including `/`).
///
/// Used by `allowed_mcp_sources` filtering. Pattern syntax:
///   - `*` matches any sequence of non-`/` characters
///   - `**` matches any sequence of characters including `/`
///   - all other characters match literally
fn glob_match(pattern: &str, text: &str) -> bool {
    // Convert the glob to a simple state machine over (pi, ti).
    fn helper(p: &[char], t: &[char], mut pi: usize, mut ti: usize) -> bool {
        while pi < p.len() {
            // Handle ** (match any chars including separator).
            if pi + 1 < p.len() && p[pi] == '*' && p[pi + 1] == '*' {
                pi += 2;
                // Skip a single optional leading `/` after ** to avoid "**/x" requiring "//x".
                if pi < p.len() && p[pi] == '/' {
                    pi += 1;
                }
                if pi >= p.len() {
                    return true; // trailing ** matches everything.
                }
                // Try every suffix of t.
                for start in ti..=t.len() {
                    if helper(p, t, pi, start) {
                        return true;
                    }
                }
                return false;
            }
            // Handle * (match any chars except separator).
            if p[pi] == '*' {
                pi += 1;
                for start in ti..=t.len() {
                    // Must not cross a `/` boundary.
                    if start > ti && t[start - 1] == '/' {
                        break;
                    }
                    if helper(p, t, pi, start) {
                        return true;
                    }
                }
                return false;
            }
            // Literal match.
            if ti >= t.len() {
                return false;
            }
            if p[pi] != t[ti] {
                return false;
            }
            pi += 1;
            ti += 1;
        }
        ti == t.len()
    }

    let p_chars: Vec<char> = pattern.chars().collect();
    let t_chars: Vec<char> = text.chars().collect();
    helper(&p_chars, &t_chars, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_requirements() {
        let toml: toml::Value = toml::from_str(
            r#"
[requirements]
managed_deny = ["/etc/**", "/var/**"]
forced_provider = "anthropic"
required_sandbox = "landlock"
disabled_features = ["shell_exec"]
allowed_mcp_sources = ["github.com/my-org/*"]
require_keychain_storage = true
"#,
        )
        .unwrap();

        let req = RequirementBinding::from_toml(&toml);
        assert_eq!(req.managed_deny, vec!["/etc/**", "/var/**"]);
        assert_eq!(req.forced_provider.as_deref(), Some("anthropic"));
        assert_eq!(req.required_sandbox.as_deref(), Some("landlock"));
        assert!(req.is_feature_disabled("shell_exec"));
        assert!(!req.is_feature_disabled("read_file"));
        assert_eq!(req.allowed_mcp_sources, vec!["github.com/my-org/*"]);
        assert!(req.require_keychain_storage);
        assert!(!req.is_empty());
    }

    #[test]
    fn parse_empty_requirements_defaults() {
        let toml: toml::Value = toml::from_str(r#"model = "gpt-4""#).unwrap();
        let req = RequirementBinding::from_toml(&toml);
        assert!(req.is_empty());
        assert!(!req.require_keychain_storage);
    }

    #[test]
    fn parse_partial_requirements() {
        let toml: toml::Value = toml::from_str(
            r#"
[requirements]
forced_provider = "openai"
"#,
        )
        .unwrap();
        let req = RequirementBinding::from_toml(&toml);
        assert_eq!(req.forced_provider.as_deref(), Some("openai"));
        assert!(req.managed_deny.is_empty());
        assert!(!req.is_empty());
    }

    // ── enforce() tests ─────────────────────────────────────────────

    #[test]
    fn enforce_empty_requirements_noop() {
        let req = RequirementBinding::default();
        let values: toml::Value = toml::from_str(r#"provider = "anthropic""#).unwrap();
        let (enforced, diags) = req.enforce(values.clone());
        assert_eq!(enforced, values);
        assert!(diags.is_empty());
    }

    #[test]
    fn enforce_forced_provider_overrides_user_value() {
        let req = RequirementBinding {
            forced_provider: Some("anthropic".into()),
            ..Default::default()
        };
        let values: toml::Value = toml::from_str(r#"provider = "openai""#).unwrap();
        let (enforced, diags) = req.enforce(values);
        assert_eq!(enforced["provider"].as_str(), Some("anthropic"));
        assert!(diags.iter().any(|d|
            d.level == DiagnosticLevel::Error
                && d.key_path == "provider"
                && d.message.contains("→ 'anthropic'")
        ));
    }

    #[test]
    fn enforce_forced_provider_inserts_when_absent() {
        let req = RequirementBinding {
            forced_provider: Some("ollama".into()),
            ..Default::default()
        };
        let values: toml::Value = toml::from_str(r#"model_id = "qwen2.5""#).unwrap();
        let (enforced, _diags) = req.enforce(values);
        assert_eq!(enforced["provider"].as_str(), Some("ollama"));
    }

    #[test]
    fn enforce_required_sandbox_overrides() {
        let req = RequirementBinding {
            required_sandbox: Some("landlock".into()),
            ..Default::default()
        };
        let values: toml::Value = toml::from_str(
            r#"
[sandbox]
type = "none"
"#,
        )
        .unwrap();
        let (enforced, diags) = req.enforce(values);
        assert_eq!(enforced["sandbox"]["type"].as_str(), Some("landlock"));
        assert!(diags.iter().any(|d| d.key_path == "sandbox.type" && d.level == DiagnosticLevel::Error));
    }

    #[test]
    fn enforce_disabled_features_forces_false() {
        let req = RequirementBinding {
            disabled_features: vec!["shell_exec".into(), "code_interpreter".into()],
            ..Default::default()
        };
        let values: toml::Value = toml::from_str(
            r#"
[features]
shell_exec = true
read_file = true
"#,
        )
        .unwrap();
        let (enforced, diags) = req.enforce(values);
        assert_eq!(enforced["features"]["shell_exec"].as_bool(), Some(false));
        assert_eq!(enforced["features"]["code_interpreter"].as_bool(), Some(false));
        // read_file is not in disabled_features → should remain true.
        assert_eq!(enforced["features"]["read_file"].as_bool(), Some(true));
        assert!(diags.iter().any(|d| d.key_path == "features.shell_exec"));
    }

    #[test]
    fn enforce_require_keychain_storage() {
        let req = RequirementBinding {
            require_keychain_storage: true,
            ..Default::default()
        };
        let values: toml::Value = toml::from_str(
            r#"
[credential]
storage = "plaintext"
"#,
        )
        .unwrap();
        let (enforced, diags) = req.enforce(values);
        assert_eq!(enforced["credential"]["storage"].as_str(), Some("keychain"));
        assert!(diags.iter().any(|d| d.key_path == "credential.storage"));
    }

    #[test]
    fn enforce_allowed_mcp_sources_filters_servers() {
        let req = RequirementBinding {
            allowed_mcp_sources: vec!["github.com/my-org/*".into()],
            ..Default::default()
        };
        let values: toml::Value = toml::from_str(
            r#"
[[mcp.servers]]
name = "allowed-1"
source = "github.com/my-org/mcp-filesystem"
command = "npx"

[[mcp.servers]]
name = "disallowed"
source = "github.com/someone-else/evil-mcp"
command = "npx"

[[mcp.servers]]
name = "allowed-2"
source = "github.com/my-org/mcp-git"
command = "npx"
"#,
        )
        .unwrap();
        let (enforced, diags) = req.enforce(values);
        let servers = enforced["mcp"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2, "should keep 2 matching servers, removed 1 disallowed");
        assert!(diags.iter().any(|d| d.key_path == "mcp.servers" && d.message.contains("removed 1")));
    }

    #[test]
    fn enforce_managed_deny_emits_info_diagnostic() {
        let req = RequirementBinding {
            managed_deny: vec!["/etc/**".into()],
            ..Default::default()
        };
        let values: toml::Value = toml::from_str(r#"model_id = "gpt-4""#).unwrap();
        let (_enforced, diags) = req.enforce(values);
        assert!(diags.iter().any(|d|
            d.key_path == "requirements.managed_deny"
                && d.level == DiagnosticLevel::Info
                && d.message.contains("/etc/**")
        ));
    }

    #[test]
    fn enforce_combined_all_constraints() {
        let toml_cfg: toml::Value = toml::from_str(
            r#"
[requirements]
forced_provider = "anthropic"
required_sandbox = "landlock"
disabled_features = ["shell_exec"]
allowed_mcp_sources = ["internal.corp/**"]
require_keychain_storage = true
managed_deny = ["/proc/**"]
"#,
        )
        .unwrap();
        let req = RequirementBinding::from_toml(&toml_cfg);

        let user_values: toml::Value = toml::from_str(
            r#"
provider = "openai"
[features]
shell_exec = true
[credential]
storage = "plaintext"
[[mcp.servers]]
name = "bad"
source = "github.com/evil/mcp"
[[mcp.servers]]
name = "good"
source = "internal.corp/mcp/internal-logs"
"#,
        )
        .unwrap();

        let (enforced, _diags) = req.enforce(user_values);
        assert_eq!(enforced["provider"].as_str(), Some("anthropic"));
        assert_eq!(enforced["sandbox"]["type"].as_str(), Some("landlock"));
        assert_eq!(enforced["features"]["shell_exec"].as_bool(), Some(false));
        assert_eq!(enforced["credential"]["storage"].as_str(), Some("keychain"));
        assert_eq!(enforced["mcp"]["servers"].as_array().unwrap().len(), 1);
    }

    // ── glob_match tests ────────────────────────────────────────────

    #[test]
    fn glob_match_literal() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn glob_match_single_star_no_slash() {
        assert!(glob_match("foo/*/baz", "foo/bar/baz"));
        assert!(glob_match("*.md", "README.md"));
        assert!(!glob_match("*.md", "docs/README.md")); // * doesn't cross /
    }

    #[test]
    fn glob_match_double_star_crosses_slash() {
        assert!(glob_match("**/README.md", "README.md"));
        assert!(glob_match("**/README.md", "a/README.md"));
        assert!(glob_match("**/README.md", "a/b/c/README.md"));
    }

    #[test]
    fn glob_match_double_star_trailing() {
        assert!(glob_match("github.com/my-org/**", "github.com/my-org/mcp-foo"));
        assert!(glob_match("github.com/my-org/**", "github.com/my-org/nested/mcp-bar"));
        assert!(!glob_match("github.com/my-org/**", "github.com/other-org/mcp"));
    }

    #[test]
    fn glob_match_empty_pattern_and_text() {
        assert!(glob_match("", ""));
        assert!(!glob_match("x", ""));
        assert!(!glob_match("", "y"));
    }
}
