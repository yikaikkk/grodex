//! WriteFileTool — creates or overwrites a file.

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
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
    /// Optional: SHA-256 hash of the file content at read time.
    /// If provided and the file has changed since, the write is rejected.
    #[serde(default)]
    pub expected_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileOutput {
    pub path: String,
    pub bytes_written: u64,
    pub file_existed: bool,
    pub content_hash: String,
}

pub struct WriteFileTool;

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for WriteFileTool {
    type Args = WriteFileArgs;
    type Output = WriteFileOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "write_file".into(),
            display_name: "Write File".into(),
            description: "Create or overwrite a file with the given content.".into(),
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Ask,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write"},
                "content": {"type": "string", "description": "Content to write to the file"}
            },
            "required": ["path", "content"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "bytes_written": {"type": "integer"},
                "file_existed": {"type": "boolean"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for WriteFileTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: WriteFileArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let file_existed = std::path::Path::new(&args.path).exists();

        // File version fence.
        if let Some(ref expected) = args.expected_hash {
            if file_existed {
                let current = std::fs::read_to_string(&args.path)
                    .map_err(|e| GrodexError::ToolExecution(format!("hash check: {e}")))?;
                let mut h = Sha256::new();
                h.update(current.as_bytes());
                if format!("{:x}", h.finalize()) != *expected {
                    return Err(GrodexError::ToolExecution(
                        "file modified since last read — re-read before writing".into(),
                    ));
                }
            }
        }

        std::fs::write(&args.path, &args.content)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot write {}: {e}", args.path)))?;

        let content_hash = {
            let mut h = Sha256::new();
            h.update(args.content.as_bytes());
            format!("{:x}", h.finalize())
        };
        let result = WriteFileOutput {
            path: args.path,
            bytes_written: args.content.len() as u64,
            file_existed,
            content_hash,
        };

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

#[derive(Debug, Clone)]
pub struct PreparedWriteCall {
    pub args: WriteFileArgs,
    pub file_existed: bool,
    pub before_hash: Option<String>,
    pub after_hash: String,
    pub canonical_resource_id: String,
    pub display_path: PathBuf,
    pub stale_candidate: Option<StaleFile>,
}

impl PreparedCall for PreparedWriteCall {
    fn side_effect_hint(&self) -> SideEffectHint {
        SideEffectHint::LocalFsWrite
    }

    fn plan_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.args.path.as_bytes());
        h.update(self.args.content.as_bytes());
        h.update(self.after_hash.as_bytes());
        if let Some(ref e) = self.args.expected_hash {
            h.update(e.as_bytes());
        }
        let full = format!("{:x}", h.finalize());
        full[..16].to_string()
    }
}

impl BuiltInTool for WriteFileTool {
    type Input = WriteFileArgs;
    type Prepared = PreparedWriteCall;
    type OkResult = ToolResultEnvelope;
    type Error = GrodexError;

    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error> {
        let file_existed = std::path::Path::new(&input.path).exists();

        let before_hash = if file_existed {
            let current = std::fs::read_to_string(&input.path)
                .map_err(|e| GrodexError::ToolExecution(format!("hash check read: {e}")))?;
            let mut h = Sha256::new();
            h.update(current.as_bytes());
            Some(format!("{:x}", h.finalize()))
        } else {
            None
        };

        let mut stale_candidate = None;
        if let Some(ref expected) = input.expected_hash {
            if !file_existed {
                return Err(GrodexError::ToolExecution(format!(
                    "expected_hash provided but file does not exist: {}",
                    input.path
                )));
            }
            if before_hash.as_ref() != Some(expected) {
                stale_candidate = Some(StaleFile {
                    resource_id: format!(
                        "fs://{}",
                        crate::fsutil::canonicalize(std::path::Path::new(&input.path)).display()
                    ),
                    expected_hash: Some(expected.clone()),
                    actual_hash: before_hash.clone(),
                    changed_since_secs: None,
                    suggested_action: StaleSuggestion::RereadAndResample,
                });
            }
        }

        let after_hash = {
            let mut h = Sha256::new();
            h.update(input.content.as_bytes());
            format!("{:x}", h.finalize())
        };

        let canonical_path = crate::fsutil::canonicalize(std::path::Path::new(&input.path));
        let canonical_resource_id = format!("fs://{}", canonical_path.display());
        let display_path = PathBuf::from(&input.path);

        Ok(PreparedWriteCall {
            args: input,
            file_existed,
            before_hash,
            after_hash,
            canonical_resource_id,
            display_path,
            stale_candidate,
        })
    }

    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error> {
        let start = std::time::Instant::now();

        if prepared.stale_candidate.is_some() {
            let sc = prepared.stale_candidate.unwrap();
            let mut structured_data = BTreeMap::new();
            structured_data.insert(
                "stale_file".into(),
                serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
            );
            let envelope = ToolResultEnvelope {
                tool_call_id: String::new(),
                operation_id: None,
                capability_id: Some("write_file".into()),
                contract_version: 1,
                status: ToolStatus::RuntimeError,
                summary: format!(
                    "stale file fence: expected {:?}, actual {:?}",
                    sc.expected_hash, sc.actual_hash
                ),
                model_content: vec![ModelContent::Text(format!(
                    "tool `write_file` stale: file {} modified since last read, re-read before writing",
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

        let before_hash_for_changed = prepared.before_hash.clone();

        std::fs::write(&prepared.args.path, &prepared.args.content)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot write {}: {e}", prepared.args.path)))?;

        let result = WriteFileOutput {
            path: prepared.args.path.clone(),
            bytes_written: prepared.args.content.len() as u64,
            file_existed: prepared.file_existed,
            content_hash: prepared.after_hash.clone(),
        };

        let output_serialized = serde_json::to_string(&result).ok();
        let mut structured_data = BTreeMap::new();
        if let Some(s) = output_serialized {
            structured_data.insert("output_serialized".into(), serde_json::Value::String(s));
        }

        let change_type = if prepared.file_existed {
            ChangeType::Updated
        } else {
            ChangeType::Created
        };

        let changed_resources = vec![ChangedResource {
            resource_id: prepared.canonical_resource_id.clone(),
            display_path: prepared.display_path.clone(),
            change_type,
            before_hash: before_hash_for_changed,
            after_hash: Some(prepared.after_hash.clone()),
        }];

        let model_text = format!(
            "tool `write_file` ok: {} bytes written to {}",
            result.bytes_written,
            prepared.display_path.display()
        );

        let envelope = ToolResultEnvelope {
            tool_call_id: String::new(),
            operation_id: None,
            capability_id: Some("write_file".into()),
            contract_version: 1,
            status: ToolStatus::Ok,
            summary: model_text.clone(),
            model_content: vec![ModelContent::Text(model_text)],
            structured_data,
            artifacts: vec![],
            changed_resources,
            truncation: TruncationInfo {
                original_bytes: result.bytes_written,
                retained_bytes: result.bytes_written,
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
    async fn write_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");

        let tool = WriteFileTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "content": "hello world"}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: WriteFileOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.bytes_written, 11);
        assert!(!output.file_existed);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn overwrite_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overwrite.txt");
        std::fs::write(&path, "old").unwrap();

        let tool = WriteFileTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "content": "new content"}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: WriteFileOutput = serde_json::from_value(result).unwrap();
        assert!(output.file_existed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    }
}
