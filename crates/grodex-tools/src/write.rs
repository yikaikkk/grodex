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

/// 写入内容的硬上限（T6 大内容输入预算）。单次 `write_file` 调用的
/// `content` 不得超过此大小，防止模型/调用方一次性写入超大文件撑爆
/// 内存与磁盘。需要写更大文件时用 `append=true` 分块写入。
const WRITE_HARD_CAP_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
    /// Optional: SHA-256 hash of the file content at read time.
    /// If provided and the file has changed since, the write is rejected.
    #[serde(default)]
    pub expected_hash: Option<String>,
    /// T6 大内容写入模式：当为 true 时，以追加方式写入而非整文件覆盖。
    /// 用于分块构建大文件（每块 ≤ WRITE_HARD_CAP_BYTES），避免一次性
    /// 重新生成整文件内容。追加模式下 `expected_hash` 校验的是追加前
    /// 已有文件的哈希。
    #[serde(default)]
    pub append: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileOutput {
    pub path: String,
    pub bytes_written: u64,
    pub file_existed: bool,
    pub content_hash: String,
    /// T6：是否以追加模式写入。
    #[serde(default)]
    pub append: bool,
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
            description: "Create or overwrite a file with the given content. Supports append mode for chunked large-file writes.".into(),
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
                "content": {"type": "string", "description": "Content to write to the file (max 16MB per call; use append=true for larger files)"},
                "expected_hash": {"type": "string", "description": "SHA-256 from the last read; refuse the write if the file changed since then"},
                "append": {"type": "boolean", "description": "Append to the file instead of overwriting. Use for chunked large-file construction (each chunk ≤ 16MB)."}
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
                "file_existed": {"type": "boolean"},
                "append": {"type": "boolean"}
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

        // 把 exists / hash-check 读 / 原子写 / SHA-256 等阻塞操作移到
        // spawn_blocking 线程池，并经全局 Semaphore 限流（T3）。
        let result = crate::blocking::run_blocking_io(move || -> Result<WriteFileOutput, GrodexError> {
            // T6 大内容输入预算：单次写入不得超过硬上限，防止一次性
            // 写入超大文件撑爆内存与磁盘。需写更大文件请用 append=true
            // 分块写入。
            if args.content.len() > WRITE_HARD_CAP_BYTES {
                return Err(GrodexError::ToolExecution(format!(
                    "write_file content too large: {} bytes > hard cap {} bytes. \
                     Use append=true to write large files in chunks.",
                    args.content.len(),
                    WRITE_HARD_CAP_BYTES
                )));
            }

            let file_existed = std::path::Path::new(&args.path).exists();

            // File version fence (applies to both overwrite and append:
            // in append mode it guards the *existing* pre-append content).
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

            if args.append {
                // T6 追加模式：分块构建大文件。直接 append 新字节（不
                // 重写整文件），fsync 持久化，再流式哈希整文件得到
                // content_hash。
                use std::io::Write;
                {
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&args.path)
                        .map_err(|e| GrodexError::ToolExecution(format!("cannot open(append) {}: {e}", args.path)))?;
                    f.write_all(args.content.as_bytes())
                        .map_err(|e| GrodexError::ToolExecution(format!("append write {}: {e}", args.path)))?;
                    let _ = f.sync_all();
                }
                let content_hash = stream_hash_file(&args.path)?;
                let result = WriteFileOutput {
                    path: args.path,
                    bytes_written: args.content.len() as u64,
                    file_existed,
                    content_hash,
                    append: true,
                };
                return Ok(result);
            }

            // 原子写（T7）：tempfile + fsync + atomic rename，崩溃时
            // 目标文件要么完整旧、要么完整新，绝不半写。
            crate::fsutil::atomic_write(std::path::Path::new(&args.path), args.content.as_bytes())
                .map_err(|e| GrodexError::ToolExecution(format!("cannot write {}: {e}", args.path)))?;

            // T9：content_hash 只算一次（旧实现这里也是一次，但与
            // atomic_write 的临时文件写入不重复，保持单次）。
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
                append: false,
            };

            Ok(result)
        })
        .await?;

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

/// 流式哈希整个文件（T6 追加模式用）：用 BufReader 逐块喂 SHA-256，
/// 避免把整个（可能很大的）文件读进一个 String。
fn stream_hash_file(path: &str) -> Result<String, GrodexError> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)
        .map_err(|e| GrodexError::ToolExecution(format!("hash read {}: {e}", path)))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| GrodexError::ToolExecution(format!("hash read {}: {e}", path)))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
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

        // 原子写（T7）：tempfile + fsync + atomic rename。
        crate::fsutil::atomic_write(
            std::path::Path::new(&prepared.args.path),
            prepared.args.content.as_bytes(),
        )
        .map_err(|e| GrodexError::ToolExecution(format!("cannot write {}: {e}", prepared.args.path)))?;

        let result = WriteFileOutput {
            path: prepared.args.path.clone(),
            bytes_written: prepared.args.content.len() as u64,
            file_existed: prepared.file_existed,
            content_hash: prepared.after_hash.clone(),
            append: prepared.args.append,
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
