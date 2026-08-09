//! Layer merging — combine config layers in precedence order.
//!
//! Layers are merged low-to-high: lower-precedence values provide defaults,
//! higher-precedence values override. Each key records which layer provided
//! the winning value via `MergeTrace`.

use crate::error::ConfigError;
use crate::layer::{ConfigLayer, ConfigLayerSource, MergeTrace};
use crate::values::{ConfigDiagnostic, DiagnosticLevel, EffectiveConfig};
use std::collections::HashMap;

/// Merge the given layers into an EffectiveConfig.
///
/// `layers` must be in order from **lowest** to **highest** precedence
/// (i.e. system → enterprise → user → workspace → session).
pub fn merge_layers(layers: &[ConfigLayer]) -> Result<EffectiveConfig, ConfigError> {
    if layers.is_empty() {
        return Ok(EffectiveConfig::empty());
    }

    let mut merged = toml::Value::Table(toml::value::Table::new());
    let mut traces: HashMap<String, MergeTrace> = HashMap::new();
    let mut diagnostics = Vec::new();

    for layer in layers {
        if let Some(reason) = &layer.disabled_reason {
            diagnostics.push(ConfigDiagnostic {
                level: DiagnosticLevel::Warning,
                key_path: String::new(),
                message: format!("layer {:?} is disabled: {reason}", layer.source),
            });
            continue;
        }

        let trace = MergeTrace::new(layer.source.clone(), layer.fingerprint.clone());
        merge_table(&mut merged, &layer.values, &trace, "", &mut traces);
    }

    Ok(EffectiveConfig {
        values: merged,
        merge_traces: traces,
        generation: 1,
        diagnostics,
    })
}

/// Recursively merge an overlay table into a base table.
fn merge_table(
    base: &mut toml::Value,
    overlay: &toml::Value,
    trace: &MergeTrace,
    key_prefix: &str,
    traces: &mut HashMap<String, MergeTrace>,
) {
    let toml::Value::Table(base_table) = base else {
        // Base is not a table — overlay replaces entirely.
        *base = overlay.clone();
        if !key_prefix.is_empty() {
            traces.insert(key_prefix.to_string(), trace.clone());
        }
        return;
    };

    let toml::Value::Table(overlay_table) = overlay else {
        // Overlay is not a table — it replaces the entire key.
        *base = overlay.clone();
        if !key_prefix.is_empty() {
            traces.insert(key_prefix.to_string(), trace.clone());
        }
        return;
    };

    for (key, overlay_value) in overlay_table {
        let full_key = if key_prefix.is_empty() {
            key.clone()
        } else {
            format!("{key_prefix}.{key}")
        };

        match base_table.get_mut(key) {
            Some(base_value) => {
                // Both are tables — recurse.
                if base_value.is_table() && overlay_value.is_table() {
                    merge_table(base_value, overlay_value, trace, &full_key, traces);
                } else {
                    // Scalar or type mismatch — overlay wins.
                    *base_value = overlay_value.clone();
                    traces.insert(full_key, trace.clone());
                }
            }
            None => {
                // New key — insert.
                base_table.insert(key.clone(), overlay_value.clone());
                traces.insert(full_key, trace.clone());
            }
        }
    }
}

/// Standard precedence order for config layer sources.
///
/// Returns the ordering weight (lower = lower precedence).
pub fn layer_precedence(source: &ConfigLayerSource) -> u8 {
    match source {
        ConfigLayerSource::Builtin => 0,
        ConfigLayerSource::System => 10,
        ConfigLayerSource::EnterpriseManaged => 20,
        ConfigLayerSource::User => 30,
        ConfigLayerSource::Profile(_) => 35,
        ConfigLayerSource::Workspace { .. } => 40,
        ConfigLayerSource::SessionFlag => 50,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_layer(source: ConfigLayerSource, toml_str: &str) -> ConfigLayer {
        ConfigLayer {
            source,
            values: toml::from_str(toml_str).unwrap(),
            fingerprint: "test".into(),
            disabled_reason: None,
        }
    }

    #[test]
    fn merge_simple_override() {
        let base = make_layer(ConfigLayerSource::System, r#"key = "base""#);
        let overlay = make_layer(ConfigLayerSource::User, r#"key = "user""#);

        let result = merge_layers(&[base, overlay]).unwrap();
        assert_eq!(result.values["key"].as_str(), Some("user"));
        assert_eq!(result.generation, 1);
    }

    #[test]
    fn merge_nested_tables() {
        let base = make_layer(ConfigLayerSource::System, "[server]\nhost = \"localhost\"\nport = 8080");
        let overlay = make_layer(ConfigLayerSource::User, "[server]\nport = 9090");

        let result = merge_layers(&[base, overlay]).unwrap();
        assert_eq!(result.values["server"]["host"].as_str(), Some("localhost"));
        assert_eq!(result.values["server"]["port"].as_integer(), Some(9090));
    }

    #[test]
    fn merge_new_key_from_overlay() {
        let base = make_layer(ConfigLayerSource::System, r#"existing = 1"#);
        let overlay = make_layer(ConfigLayerSource::User, r#"new_key = "hello""#);

        let result = merge_layers(&[base, overlay]).unwrap();
        assert_eq!(result.values["existing"].as_integer(), Some(1));
        assert_eq!(result.values["new_key"].as_str(), Some("hello"));
    }

    #[test]
    fn empty_layers_returns_empty_config() {
        let result = merge_layers(&[]).unwrap();
        assert_eq!(result.generation, 0);
    }

    #[test]
    fn merge_traces_record_origin() {
        let base = make_layer(ConfigLayerSource::System, r#"key = "sys""#);
        let overlay = make_layer(ConfigLayerSource::User, r#"key2 = "usr""#);

        let result = merge_layers(&[base, overlay]).unwrap();
        assert_eq!(result.merge_traces["key"].origin, ConfigLayerSource::System);
        assert_eq!(result.merge_traces["key2"].origin, ConfigLayerSource::User);
    }
}
