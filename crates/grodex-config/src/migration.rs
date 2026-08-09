//! Schema version migration — transform old config schemas to current (Design Doc 18 §7).
//!
//! The loading pipeline is:
//!   read bytes → parse TOML → **normalize aliases** → **migrate old schema**
//!   → typed deserialize → per-field validation → cross-ref validation
//!   → requirements constraint → compile derived artifacts
//!
//! This module handles the "normalize aliases" and "migrate old schema" steps.
//! Each migration is a pure function `toml::Value → toml::Value` that also
//! records diagnostics for any transforms applied.

use crate::values::{ConfigDiagnostic, DiagnosticLevel};

/// Current config schema version.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Result of a migration pass.
#[derive(Debug, Clone)]
pub struct MigrationResult {
    /// The migrated TOML value.
    pub values: toml::Value,
    /// Diagnostics produced during migration.
    pub diagnostics: Vec<ConfigDiagnostic>,
    /// The schema version detected in the source (before migration).
    pub source_schema_version: u32,
    /// The schema version after migration (always CURRENT_SCHEMA_VERSION).
    pub target_schema_version: u32,
}

impl Default for MigrationResult {
    fn default() -> Self {
        Self {
            values: toml::Value::Table(toml::value::Table::new()),
            diagnostics: Vec::new(),
            source_schema_version: CURRENT_SCHEMA_VERSION,
            target_schema_version: CURRENT_SCHEMA_VERSION,
        }
    }
}

/// Migrate a parsed TOML config to the current schema version.
///
/// If no `schema_version` key is present, it defaults to 1 (the original
/// unversioned format). Migrations are applied sequentially:
///   v1 → v2 (alias normalization + key renames)
pub fn migrate(values: toml::Value) -> MigrationResult {
    let mut result = MigrationResult {
        target_schema_version: CURRENT_SCHEMA_VERSION,
        ..Default::default()
    };

    let source_version = extract_schema_version(&values);
    result.source_schema_version = source_version;

    let mut current = values;
    let mut current_version = source_version;

    while current_version < CURRENT_SCHEMA_VERSION {
        let (next, diags) = match current_version {
            1 => migrate_v1_to_v2(current),
            _ => break,
        };
        current = next;
        result.diagnostics.extend(diags);
        current_version += 1;
    }

    // Stamp the final schema_version into the values.
    if let toml::Value::Table(ref mut table) = current {
        table.insert(
            "schema_version".to_string(),
            toml::Value::Integer(CURRENT_SCHEMA_VERSION as i64),
        );
    }

    result.values = current;
    result
}

/// Extract the schema_version from a TOML value. Defaults to 1 if missing.
fn extract_schema_version(values: &toml::Value) -> u32 {
    values
        .as_table()
        .and_then(|t| t.get("schema_version"))
        .and_then(|v| v.as_integer())
        .map(|v| v as u32)
        .unwrap_or(1)
}

/// Migration v1 → v2: normalize aliases and rename deprecated keys.
///
/// Changes:
///   - `model` → `model_id` (top-level)
///   - `provider.endpoint` → `provider.base_url`
///   - `mcp_servers` → `mcp.servers` (move to MCP subtable)
///   - `max_turns` → `budget.max_turns` (move to budget subtable)
///   - Add `schema_version = 2` if missing
fn migrate_v1_to_v2(mut values: toml::Value) -> (toml::Value, Vec<ConfigDiagnostic>) {
    let mut diagnostics = Vec::new();

    let table = match values.as_table_mut() {
        Some(t) => t,
        None => return (values, diagnostics),
    };

    // Rename `model` → `model_id`.
    if let Some(model_val) = table.remove("model") {
        let exists = table.insert("model_id".to_string(), model_val);
        if exists.is_none() {
            diagnostics.push(ConfigDiagnostic {
                level: DiagnosticLevel::Info,
                key_path: "model_id".to_string(),
                message: "migrated 'model' → 'model_id' (v1→v2)".to_string(),
            });
        }
    }

    // Rename `provider.endpoint` → `provider.base_url`.
    if let Some(provider) = table.get_mut("provider").and_then(|v| v.as_table_mut()) {
        if let Some(endpoint) = provider.remove("endpoint") {
            provider.insert("base_url".to_string(), endpoint);
            diagnostics.push(ConfigDiagnostic {
                level: DiagnosticLevel::Info,
                key_path: "provider.base_url".to_string(),
                message: "migrated 'provider.endpoint' → 'provider.base_url' (v1→v2)".to_string(),
            });
        }
    }

    // Move `mcp_servers` → `mcp.servers`.
    if let Some(mcp_servers) = table.remove("mcp_servers") {
        let mcp_table = table
            .entry("mcp".to_string())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        if let Some(mcp) = mcp_table.as_table_mut() {
            mcp.insert("servers".to_string(), mcp_servers);
            diagnostics.push(ConfigDiagnostic {
                level: DiagnosticLevel::Info,
                key_path: "mcp.servers".to_string(),
                message: "migrated 'mcp_servers' → 'mcp.servers' (v1→v2)".to_string(),
            });
        }
    }

    // Move `max_turns` → `budget.max_turns`.
    if let Some(max_turns) = table.remove("max_turns") {
        let budget_table = table
            .entry("budget".to_string())
            .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
        if let Some(budget) = budget_table.as_table_mut() {
            budget.insert("max_turns".to_string(), max_turns);
            diagnostics.push(ConfigDiagnostic {
                level: DiagnosticLevel::Info,
                key_path: "budget.max_turns".to_string(),
                message: "migrated 'max_turns' → 'budget.max_turns' (v1→v2)".to_string(),
            });
        }
    }

    // Warn about unknown keys (strict mode for managed/permission/sandbox/credential).
    let strict_keys = ["permissions", "sandbox", "credential", "managed"];
    for key in &strict_keys {
        if table.contains_key(*key) {
            diagnostics.push(ConfigDiagnostic {
                level: DiagnosticLevel::Warning,
                key_path: key.to_string(),
                message: format!(
                    "key '{key}' is in strict zone — unknown subkeys will error in strict mode"
                ),
            });
        }
    }

    (values, diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_schema_version_defaults_to_v1() {
        let values: toml::Value = toml::from_str(r#"model = "gpt-4""#).unwrap();
        let result = migrate(values);
        assert_eq!(result.source_schema_version, 1);
        assert_eq!(result.target_schema_version, 2);
        // Should have migrated model → model_id.
        assert!(result.values.as_table().unwrap().contains_key("model_id"));
        assert!(!result.values.as_table().unwrap().contains_key("model"));
        assert!(!result.diagnostics.is_empty(), "should record migration diagnostics");
    }

    #[test]
    fn already_v2_no_migration() {
        let values: toml::Value =
            toml::from_str("schema_version = 2\nmodel_id = \"gpt-4\"").unwrap();
        let result = migrate(values);
        assert_eq!(result.source_schema_version, 2);
        assert_eq!(result.target_schema_version, 2);
        assert!(result.diagnostics.is_empty(), "v2 should not produce migration diagnostics");
    }

    #[test]
    fn v1_to_v2_renames_model() {
        let values: toml::Value = toml::from_str(r#"model = "claude-opus""#).unwrap();
        let result = migrate(values);
        assert_eq!(result.values["model_id"].as_str(), Some("claude-opus"));
        assert!(result.values.as_table().unwrap().get("model").is_none());
    }

    #[test]
    fn v1_to_v2_renames_provider_endpoint() {
        let values: toml::Value =
            toml::from_str("[provider]\nendpoint = \"https://api.openai.com\"").unwrap();
        let result = migrate(values);
        assert_eq!(
            result.values["provider"]["base_url"].as_str(),
            Some("https://api.openai.com")
        );
    }

    #[test]
    fn v1_to_v2_moves_mcp_servers() {
        let values: toml::Value =
            toml::from_str(r#"mcp_servers = ["github", "filesystem"]"#).unwrap();
        let result = migrate(values);
        assert!(result.values["mcp"]["servers"].is_array());
    }

    #[test]
    fn v1_to_v2_moves_max_turns_to_budget() {
        let values: toml::Value = toml::from_str(r#"max_turns = 50"#).unwrap();
        let result = migrate(values);
        assert_eq!(result.values["budget"]["max_turns"].as_integer(), Some(50));
    }

    #[test]
    fn schema_version_stamped_in_result() {
        let values: toml::Value = toml::from_str(r#"model = "gpt-4""#).unwrap();
        let result = migrate(values);
        assert_eq!(
            result.values["schema_version"].as_integer(),
            Some(CURRENT_SCHEMA_VERSION as i64)
        );
    }

    #[test]
    fn strict_zone_keys_warn() {
        let values: toml::Value =
            toml::from_str("[sandbox]\ntype = \"seatbelt\"").unwrap();
        let result = migrate(values);
        assert!(result.diagnostics.iter().any(|d| d.key_path == "sandbox" && d.level == DiagnosticLevel::Warning));
    }

    #[test]
    fn idempotent_migration() {
        let values: toml::Value = toml::from_str(r#"model = "gpt-4""#).unwrap();
        let result1 = migrate(values);
        let result2 = migrate(result1.values.clone());
        // Second migration should be a no-op (already v2).
        assert_eq!(result2.source_schema_version, 2);
        assert!(result2.diagnostics.is_empty());
    }
}
