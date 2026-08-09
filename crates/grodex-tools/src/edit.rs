//! EditTool — exact string replacement in an existing file.
//!
//! Matches `old_string` in the file content and replaces it with `new_string`.
//! Returns an error if `old_string` is not found or appears multiple times
//! (to avoid ambiguous edits).

use crate::common::{
    BuiltInTool, ChangedResource, ChangeType, ModelContent, PreparedCall, Retryability,
    SideEffectHint, StaleFile, StaleSuggestion, ToolResultEnvelope, ToolStatus, TruncationInfo,
};
use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    /// If true, replace all occurrences. Default: false (error on multiple matches).
    #[serde(default)]
    pub replace_all: bool,
    /// Lost-update fence: the SHA-256 of the file the caller last read.
    /// If supplied and the on-disk file no longer matches, the edit is
    /// refused rather than silently applied over a newer version. Get this
    /// value from `ReadFileOutput::content_hash`. (Design Doc 15 §10.1
    /// expected_file_version.)
    #[serde(default)]
    pub expected_content_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditOutput {
    pub path: String,
    pub replacements: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

pub struct EditTool;

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for EditTool {
    type Args = EditArgs;
    type Output = EditOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "edit_file".into(),
            display_name: "Edit File".into(),
            description: "Replace exact string matches in a file. Requires exact match (including whitespace). Use replace_all=true to replace all occurrences.".into(),
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Ask,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit"},
                "old_string": {"type": "string", "description": "Exact string to find and replace"},
                "new_string": {"type": "string", "description": "Replacement string"},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)"},
                "expected_content_hash": {"type": "string", "description": "SHA-256 from the last read; refuse the edit if the file changed since then (lost-update fence)"}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "replacements": {"type": "integer"},
                "bytes_before": {"type": "integer"},
                "bytes_after": {"type": "integer"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for EditTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: EditArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let content = std::fs::read_to_string(&args.path)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot read {}: {e}", args.path)))?;

        // Lost-update fence: if the caller pinned the file's hash at read
        // time, refuse the edit when the file has changed under us.
        if let Some(expected) = &args.expected_content_hash {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            let actual = format!("{:x}", h.finalize());
            if &actual != expected {
                return Err(GrodexError::ToolExecution(format!(
                    "lost-update fence: {} changed since last read (expected hash {}, got {}). Re-read before editing.",
                    args.path, expected, actual
                )));
            }
        }

        let bytes_before = content.len() as u64;

        let count = content.matches(&args.old_string).count();

        if count == 0 {
            return Err(GrodexError::ToolExecution(format!(
                "string not found in {}: {:?}",
                args.path, args.old_string
            )));
        }

        if count > 1 && !args.replace_all {
            return Err(GrodexError::ToolExecution(format!(
                "string appears {count} times in {}. Use replace_all=true to replace all, or use a more specific old_string.",
                args.path
            )));
        }

        let new_content = if args.replace_all {
            content.replace(&args.old_string, &args.new_string)
        } else {
            content.replacen(&args.old_string, &args.new_string, 1)
        };

        // Atomic write (temp + fsync + rename): a crash leaves the file
        // either fully old or fully new, never half-edited.
        crate::fsutil::atomic_write(std::path::Path::new(&args.path), new_content.as_bytes())
            .map_err(|e| GrodexError::ToolExecution(format!("cannot write {}: {e}", args.path)))?;

        let result = EditOutput {
            path: args.path,
            replacements: if args.replace_all { count } else { 1 },
            bytes_before,
            bytes_after: new_content.len() as u64,
        };

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

#[derive(Debug, Clone)]
pub struct PreparedEditCall {
    pub args: EditArgs,
    pub original_content: String,
    pub new_content: String,
    pub replacements: usize,
    pub before_hash: String,
    pub after_hash: String,
    pub canonical_resource_id: String,
    pub display_path: PathBuf,
    pub stale_candidate: Option<StaleFile>,
    pub prepare_error: Option<String>,
}

impl PreparedCall for PreparedEditCall {
    fn side_effect_hint(&self) -> SideEffectHint {
        SideEffectHint::LocalFsWrite
    }

    fn plan_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.args.path.as_bytes());
        h.update(self.args.old_string.as_bytes());
        h.update(self.args.new_string.as_bytes());
        h.update(self.before_hash.as_bytes());
        h.update(self.after_hash.as_bytes());
        h.update(self.replacements.to_le_bytes());
        let full = format!("{:x}", h.finalize());
        full[..16].to_string()
    }
}

impl BuiltInTool for EditTool {
    type Input = EditArgs;
    type Prepared = PreparedEditCall;
    type OkResult = ToolResultEnvelope;
    type Error = GrodexError;

    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error> {
        let content = std::fs::read_to_string(&input.path)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot read {}: {e}", input.path)))?;

        let _bytes_before = content.len() as u64;

        let mut h = Sha256::new();
        h.update(content.as_bytes());
        let before_hash = format!("{:x}", h.finalize());

        let canonical_path = crate::fsutil::canonicalize(std::path::Path::new(&input.path));
        let canonical_resource_id = format!("fs://{}", canonical_path.display());
        let display_path = PathBuf::from(&input.path);

        let mut stale_candidate = None;
        if let Some(expected) = &input.expected_content_hash {
            if &before_hash != expected {
                stale_candidate = Some(StaleFile {
                    resource_id: canonical_resource_id.clone(),
                    expected_hash: Some(expected.clone()),
                    actual_hash: Some(before_hash.clone()),
                    changed_since_secs: None,
                    suggested_action: StaleSuggestion::RereadAndResample,
                });
            }
        }

        let count = content.matches(&input.old_string).count();
        let mut prepare_error = None;

        if count == 0 {
            prepare_error = Some(format!(
                "string not found in {}: {:?}",
                input.path, input.old_string
            ));
        }

        if count > 1 && !input.replace_all {
            prepare_error = Some(format!(
                "string appears {count} times in {}. Use replace_all=true to replace all, or use a more specific old_string.",
                input.path
            ));
        }

        let new_content = if prepare_error.is_none() {
            if input.replace_all {
                content.replace(&input.old_string, &input.new_string)
            } else {
                content.replacen(&input.old_string, &input.new_string, 1)
            }
        } else {
            content.clone()
        };

        let mut ah = Sha256::new();
        ah.update(new_content.as_bytes());
        let after_hash = format!("{:x}", ah.finalize());

        let replacements = if prepare_error.is_some() {
            0
        } else if input.replace_all {
            count
        } else {
            1
        };

        Ok(PreparedEditCall {
            args: input,
            original_content: content,
            new_content,
            replacements,
            before_hash,
            after_hash,
            canonical_resource_id,
            display_path,
            stale_candidate,
            prepare_error,
        })
    }

    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error> {
        let start = std::time::Instant::now();

        if let Some(sc) = prepared.stale_candidate {
            let mut structured_data = BTreeMap::new();
            structured_data.insert(
                "stale_file".into(),
                serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
            );
            let envelope = ToolResultEnvelope {
                tool_call_id: String::new(),
                operation_id: None,
                capability_id: Some("edit_file".into()),
                contract_version: 1,
                status: ToolStatus::RuntimeError,
                summary: format!(
                    "stale file fence: expected {:?}, actual {:?}",
                    sc.expected_hash, sc.actual_hash
                ),
                model_content: vec![ModelContent::Text(format!(
                    "tool `edit_file` stale: file {} modified since last read, re-read before editing",
                    prepared.display_path.display()
                ))],
                structured_data,
                artifacts: vec![],
                changed_resources: vec![],
                truncation: TruncationInfo::default(),
                wall_time: start.elapsed(),
                retryability: Retryability::StaleResource,
                diagnostics: vec![],
            };
            return Ok(envelope);
        }

        if let Some(err) = prepared.prepare_error {
            let envelope = ToolResultEnvelope {
                tool_call_id: String::new(),
                operation_id: None,
                capability_id: Some("edit_file".into()),
                contract_version: 1,
                status: ToolStatus::ToolError,
                summary: err.clone(),
                model_content: vec![ModelContent::Text(format!(
                    "tool `edit_file` error: {}",
                    err
                ))],
                structured_data: BTreeMap::new(),
                artifacts: vec![],
                changed_resources: vec![],
                truncation: TruncationInfo::default(),
                wall_time: start.elapsed(),
                retryability: Retryability::NotRetryable,
                diagnostics: vec![],
            };
            return Ok(envelope);
        }

        crate::fsutil::atomic_write(
            std::path::Path::new(&prepared.args.path),
            prepared.new_content.as_bytes(),
        )
        .map_err(|e| {
            GrodexError::ToolExecution(format!(
                "cannot write {}: {e}",
                prepared.args.path
            ))
        })?;

        let result = EditOutput {
            path: prepared.args.path.clone(),
            replacements: prepared.replacements,
            bytes_before: prepared.original_content.len() as u64,
            bytes_after: prepared.new_content.len() as u64,
        };

        let output_serialized = serde_json::to_string(&result).ok();
        let mut structured_data = BTreeMap::new();
        if let Some(s) = output_serialized {
            structured_data.insert("output_serialized".into(), serde_json::Value::String(s));
        }

        let changed_resources = vec![ChangedResource {
            resource_id: prepared.canonical_resource_id.clone(),
            display_path: prepared.display_path.clone(),
            change_type: ChangeType::Updated,
            before_hash: Some(prepared.before_hash.clone()),
            after_hash: Some(prepared.after_hash.clone()),
        }];

        let model_text = format!(
            "tool `edit_file` ok: {} replacements, {} bytes → {} bytes",
            result.replacements, result.bytes_before, result.bytes_after
        );

        let envelope = ToolResultEnvelope {
            tool_call_id: String::new(),
            operation_id: None,
            capability_id: Some("edit_file".into()),
            contract_version: 1,
            status: ToolStatus::Ok,
            summary: model_text.clone(),
            model_content: vec![ModelContent::Text(model_text)],
            structured_data,
            artifacts: vec![],
            changed_resources,
            truncation: TruncationInfo {
                original_bytes: result.bytes_before,
                retained_bytes: result.bytes_after,
                strategy: crate::common::TruncationStrategy::None,
                omitted: 0,
            },
            wall_time: start.elapsed(),
            retryability: Retryability::NotRetryable,
            diagnostics: vec![],
        };

        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::tool::ToolRuntime;

    #[tokio::test]
    async fn edit_single_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edit.txt");
        std::fs::write(&path, "Hello, world!").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "world", "new_string": "Rust"}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: EditOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.replacements, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, Rust!");
    }

    #[tokio::test]
    async fn edit_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nf.txt");
        std::fs::write(&path, "hello").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "xyz", "new_string": "abc"}),
            OperationId::new(),
        )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        std::fs::write(&path, "foo bar foo baz foo").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "foo", "new_string": "qux", "replace_all": true}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: EditOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.replacements, 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "qux bar qux baz qux");
    }

    #[tokio::test]
    async fn edit_multiple_without_replace_all_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambig.txt");
        std::fs::write(&path, "foo bar foo").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "foo", "new_string": "bar"}),
            OperationId::new(),
        )
            .await;

        assert!(result.is_err());
    }

    /// Lost-update fence: supplying the wrong expected_content_hash must
    /// refuse the edit rather than silently overwrite a changed file.
    #[tokio::test]
    async fn edit_refused_when_expected_hash_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fence.txt");
        std::fs::write(&path, "Hello, world!").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                    "path": path,
                    "old_string": "world",
                    "new_string": "Rust",
                    "expected_content_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }),
            OperationId::new(),
        )
            .await;
        assert!(result.is_err(), "mismatched expected_content_hash must refuse the edit");
        // File untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, world!");
    }

    /// The correct hash (from a prior read) allows the edit.
    #[tokio::test]
    async fn edit_allowed_when_expected_hash_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.txt");
        std::fs::write(&path, "Hello, world!").unwrap();

        // "Read" first to obtain the hash.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"Hello, world!");
        let hash = format!("{:x}", h.finalize());

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                    "path": path,
                    "old_string": "world",
                    "new_string": "Rust",
                    "expected_content_hash": hash
                }),
            OperationId::new(),
        )
            .await
            .unwrap();
        let output: EditOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.replacements, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, Rust!");
    }
}
