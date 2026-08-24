//! ReadArtifactTool — retrieves offloaded tool results (blobs).
//!
//! When a tool result exceeds `max_tool_result_bytes`, the TurnCoordinator
//! offloads the full content to the blob store and replaces the inline
//! content with a preview + file path. The model can then use
//! `read_artifact` to retrieve the full content (or a bounded head+tail
//! view) without re-executing the original tool.
//!
//! This is the "artifact get" half of Design Doc 15 §12: "bounded view + blob ref".

use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, Tool, ToolMetadata, ToolRuntime};
use serde::{Deserialize, Serialize};

/// Arguments for the ReadArtifactTool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadArtifactArgs {
    /// Path to the offloaded artifact file (provided in the offload
    /// notice left by the TurnCoordinator).
    pub path: String,
    /// Maximum bytes to return from the head of the file.
    /// Default: 8192.
    #[serde(default)]
    pub head_bytes: Option<usize>,
    /// Maximum bytes to return from the tail of the file.
    /// Default: 4096.
    #[serde(default)]
    pub tail_bytes: Option<usize>,
}

/// Output of the ReadArtifactTool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadArtifactOutput {
    pub path: String,
    pub content: String,
    pub total_bytes: u64,
    pub returned_bytes: u64,
    pub omitted_bytes: u64,
    pub truncated: bool,
}

pub struct ReadArtifactTool;

impl Default for ReadArtifactTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadArtifactTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for ReadArtifactTool {
    type Args = ReadArtifactArgs;
    type Output = ReadArtifactOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "read_artifact".into(),
            display_name: "Read Artifact".into(),
            description: "Read the full content of an offloaded tool result (artifact). \
                Use this when a previous tool result was too large and was saved to a file. \
                Returns a head+tail view with an omission marker for the middle."
                .into(),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::ReadOnly,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the offloaded artifact file (from the tool result offload notice)"
                },
                "head_bytes": {
                    "type": "integer",
                    "description": "Maximum bytes from the head (default 8192)"
                },
                "tail_bytes": {
                    "type": "integer",
                    "description": "Maximum bytes from the tail (default 4096)"
                }
            },
            "required": ["path"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "Head+tail view of the artifact"},
                "total_bytes": {"type": "integer"},
                "returned_bytes": {"type": "integer"},
                "omitted_bytes": {"type": "integer"},
                "truncated": {"type": "boolean"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for ReadArtifactTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: ReadArtifactArgs = serde_json::from_value(args)
            .map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let data = std::fs::read(&args.path).map_err(|e| {
            GrodexError::ToolExecution(format!("cannot read artifact {}: {e}", args.path))
        })?;

        let total_bytes = data.len() as u64;
        let head_cap = args.head_bytes.unwrap_or(8192);
        let tail_cap = args.tail_bytes.unwrap_or(4096);

        let (content, returned_bytes, omitted_bytes) = if total_bytes as usize <= head_cap + tail_cap
        {
            // Small enough to return in full.
            let text = String::from_utf8_lossy(&data).to_string();
            let len = text.len() as u64;
            (text, len, 0)
        } else {
            // Head + tail with elision marker.
            let head = String::from_utf8_lossy(&data[..head_cap]).to_string();
            let tail_start = data.len().saturating_sub(tail_cap);
            let tail = String::from_utf8_lossy(&data[tail_start..]).to_string();
            let omitted = (tail_start - head_cap) as u64;
            let content = format!(
                "{head}\n... [{omitted} bytes omitted] ...\n{tail}"
            );
            let returned = (head_cap + tail_cap) as u64;
            (content, returned, omitted)
        };

        let output = ReadArtifactOutput {
            path: args.path,
            content,
            total_bytes,
            returned_bytes,
            omitted_bytes,
            truncated: omitted_bytes > 0,
        };

        serde_json::to_value(output)
            .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn read_artifact_small_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hello world").unwrap();

        let tool = ReadArtifactTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": tmp.path().to_str().unwrap()}),
            OperationId::new(),
        )
        .await
        .unwrap();

        let output: ReadArtifactOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.total_bytes, 11);
        assert!(!output.truncated);
        assert_eq!(output.omitted_bytes, 0);
        assert!(output.content.contains("hello world"));
    }

    #[tokio::test]
    async fn read_artifact_large_file_head_tail() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Write 20KB of data.
        let data: Vec<u8> = (0..20_000).map(|i| (i % 256) as u8).collect();
        tmp.write_all(&data).unwrap();

        let tool = ReadArtifactTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                "path": tmp.path().to_str().unwrap(),
                "head_bytes": 100,
                "tail_bytes": 100
            }),
            OperationId::new(),
        )
        .await
        .unwrap();

        let output: ReadArtifactOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.total_bytes, 20_000);
        assert!(output.truncated);
        assert!(output.omitted_bytes > 0);
        assert!(output.content.contains("bytes omitted"));
    }

    #[tokio::test]
    async fn read_artifact_missing_file() {
        let tool = ReadArtifactTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": "/nonexistent/path/file.blob"}),
            OperationId::new(),
        )
        .await;

        assert!(result.is_err());
    }
}
