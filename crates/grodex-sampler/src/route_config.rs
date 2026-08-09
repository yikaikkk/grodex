//! Route TOML loader — multi-candidate routing table with weights,
//! capacities, and fallback chains.

use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RouteConfigError {
    #[error("failed to read TOML file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse TOML: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("empty routes list")]
    EmptyRoutes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub name: String,
    pub provider: String,
    pub canonical_model_id: String,
    #[serde(default)]
    pub compatible_aliases: Vec<String>,
    pub endpoint: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default)]
    pub max_rpm: Option<u32>,
    #[serde(default)]
    pub max_tpm: Option<u64>,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub auth_env_var: String,
}

fn default_weight() -> u32 {
    100
}

fn default_priority() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRouteConfig {
    #[serde(rename = "routes")]
    pub entries: Vec<RouteEntry>,
}

impl ModelRouteConfig {
    pub fn load_from_toml(path: &Path) -> Result<Self, RouteConfigError> {
        let content = std::fs::read_to_string(path)?;
        Self::parse_from_toml_str(&content)
    }

    pub fn parse_from_toml_str(s: &str) -> Result<Self, RouteConfigError> {
        let cfg: ModelRouteConfig = toml::from_str(s)?;
        if cfg.entries.is_empty() {
            return Err(RouteConfigError::EmptyRoutes);
        }
        Ok(cfg)
    }

    pub fn entries(&self) -> &[RouteEntry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
[[routes]]
name = "primary-openai"
provider = "openai"
canonical_model_id = "gpt-4o"
compatible_aliases = ["gpt-4", "gpt-4-turbo"]
endpoint = "https://api.openai.com/v1"
weight = 80
max_rpm = 500
max_tpm = 100000
priority = 1
capabilities = ["streaming", "tool_calls", "json_mode", "vision", "parallel_tools", "reasoning"]
auth_env_var = "OPENAI_API_KEY"

[[routes]]
name = "fallback-anthropic"
provider = "anthropic"
canonical_model_id = "claude-3-opus"
endpoint = "https://api.anthropic.com/v1"
weight = 20
priority = 2
capabilities = ["streaming", "tool_calls"]
auth_env_var = "ANTHROPIC_API_KEY"
"#;

    #[test]
    fn parse_sample_toml() {
        let cfg = ModelRouteConfig::parse_from_toml_str(SAMPLE_TOML).unwrap();
        assert_eq!(cfg.entries().len(), 2);
        assert_eq!(cfg.entries()[0].name, "primary-openai");
        assert_eq!(cfg.entries()[0].weight, 80);
        assert_eq!(cfg.entries()[0].priority, 1);
        assert_eq!(cfg.entries()[0].max_rpm, Some(500));
        assert_eq!(cfg.entries()[0].auth_env_var, "OPENAI_API_KEY");
        assert!(cfg.entries()[0].capabilities.contains(&"streaming".to_string()));
        assert_eq!(cfg.entries()[1].name, "fallback-anthropic");
        assert_eq!(cfg.entries()[1].compatible_aliases.len(), 0);
    }

    #[test]
    fn empty_routes_error() {
        let err = ModelRouteConfig::parse_from_toml_str("routes = []").unwrap_err();
        assert!(matches!(err, RouteConfigError::EmptyRoutes));
    }

    #[test]
    fn defaults_applied() {
        let toml_str = r#"
[[routes]]
name = "minimal"
provider = "test"
canonical_model_id = "test-model"
endpoint = "https://test"
auth_env_var = "TEST_KEY"
"#;
        let cfg = ModelRouteConfig::parse_from_toml_str(toml_str).unwrap();
        let e = &cfg.entries()[0];
        assert_eq!(e.weight, 100);
        assert_eq!(e.priority, 1);
        assert_eq!(e.max_rpm, None);
        assert_eq!(e.max_tpm, None);
        assert!(e.capabilities.is_empty());
        assert!(e.compatible_aliases.is_empty());
    }
}
