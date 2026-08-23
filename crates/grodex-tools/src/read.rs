//! ReadFileTool — reads a file with line numbers, offset, and limit support.
//!
//! Returns content in the format: `1→line content\n2→...`

use crate::common::{
    BuiltInTool, ChangedResource, ChangeType, FileSnapshot, FileType, LineEnding, ModelContent,
    PreparedCall, ReadRange, Retryability, SideEffectHint, ToolResultEnvelope, ToolStatus,
    TruncationInfo,
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
use std::time::SystemTime;

/// Arguments for the ReadFileTool.
///
/// Supports three modes:
/// 1. **Legacy single-range** (backward-compatible): `offset` + `limit` + `max_bytes`
/// 2. **Multi-range**: `ranges` array of `ReadRange` objects
/// 3. **Anchor**: a single range with `start_pattern` / `end_pattern`
///
/// When `ranges` is present it takes precedence over `offset`/`limit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileArgs {
    pub path: String,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Multiple ranges to read from the same file. When present, the
    /// output concatenates all ranges with `--- range N ---` separators.
    #[serde(default)]
    pub ranges: Option<Vec<ReadRange>>,
    /// Output format override. When set to "hashline", each line is
    /// prefixed with `{line_num}\t{short_hash}\t{content}` for change
    /// detection without re-hashing the entire file. When None, the
    /// default line_numbered / multi_range format is used.
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFileOutput {
    pub path: String,
    pub content: String,
    pub content_hash: String, // SHA-256 of file content
    pub total_lines: usize,
    pub lines_returned: usize,
    pub truncated: bool,
    pub file_size_bytes: u64,
    /// Stable file snapshot at the moment of reading. Edit tools can
    /// consume this as `expected_snapshot` for version fencing (§9.2).
    #[serde(default)]
    pub snapshot: Option<FileSnapshot>,
    /// The rendering format actually used for this output (L1 standardization).
    /// Possible values: "line_numbered", "hashline", "raw", "hex_dump", "markdown".
    #[serde(default)]
    pub render_format: String,
}

pub struct ReadFileTool;

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for ReadFileTool {
    type Args = ReadFileArgs;
    type Output = ReadFileOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "read_file".into(),
            display_name: "Read File".into(),
            description: "Read contents of a file with line numbers. Supports offset, limit, and byte cap.".into(),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::ReadOnly,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read"},
                "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "integer", "description": "Maximum number of lines to return"},
                "max_bytes": {"type": "integer", "description": "Maximum bytes to read"},
                "ranges": {
                    "type": "array",
                    "description": "Multiple ranges to read. Each element is one of: {\"start_line\":N,\"count\":M} | {\"start_byte\":N,\"count\":M} | {\"start_pattern\":\"regex\",\"end_pattern\":\"regex\"}",
                    "items": {"type": "object"}
                },
                "format": {
                    "type": "string",
                    "description": "Output format: 'line_numbered' (default) | 'hashline' (each line prefixed with line_num + short SHA-256 hash for change detection)",
                    "enum": ["line_numbered", "hashline"]
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
                "content": {"type": "string", "description": "File content with line numbers"},
                "total_lines": {"type": "integer"},
                "lines_returned": {"type": "integer"},
                "truncated": {"type": "boolean"},
                "file_size_bytes": {"type": "integer"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for ReadFileTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: ReadFileArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let content = std::fs::read_to_string(&args.path)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot read {}: {e}", args.path)))?;

        let file_size = content.len() as u64;
        let content_hash = {
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            format!("{:x}", h.finalize())
        };
        let all_lines: Vec<&str> = content.lines().collect();
        let total_lines = all_lines.len();
        let max_bytes = args.max_bytes.unwrap_or(usize::MAX);

        // Multi-range mode: when `ranges` is present, use it instead of
        // the legacy offset/limit pair.
        // Hashline mode: when `format` is "hashline", use per-line SHA-256
        // hashing for change detection (L1 standardization).
        let use_hashline = args.format.as_deref() == Some("hashline");
        let (output, lines_returned, truncated) = if use_hashline {
            let start_idx = args.offset.unwrap_or(1).max(1) - 1;
            let lim = args.limit.unwrap_or(usize::MAX);
            let end_idx = start_idx.saturating_add(lim).min(all_lines.len());
            render_hashline(&all_lines, start_idx, end_idx, max_bytes)
        } else if let Some(ref ranges) = args.ranges {
            render_multi_range(&all_lines, ranges, max_bytes)?
        } else {
            render_single_range(&all_lines, args.offset, args.limit, max_bytes)
        };

        // Type routing: for known binary/special extensions, append a
        // diagnostic note so the model knows the content type.
        let type_note = type_routing_note(&args.path);
        let final_output = if type_note.is_empty() {
            output
        } else {
            format!("{type_note}\n{output}")
        };

        // Determine the render format actually used for this output (L1 standardization).
        let render_format = if use_hashline {
            "hashline"
        } else if type_note.contains("binary image") {
            "hex_dump"
        } else if type_note.contains("Markdown") {
            "markdown"
        } else if type_note.contains("JSON") {
            "json"
        } else if !type_note.is_empty() {
            "line_numbered"
        } else if args.ranges.is_some() {
            "multi_range"
        } else {
            "line_numbered"
        };

        let canonical_path = crate::fsutil::canonicalize(std::path::Path::new(&args.path));
        let canonical_resource_id = format!("fs://{}", canonical_path.display());
        let display_path = PathBuf::from(&args.path);

        let read_range = if args.offset.is_some() || args.limit.is_some() {
            ReadRange::Lines {
                start_line: args.offset.unwrap_or(1) as u64,
                count: args.limit.map(|v| v as u64),
            }
        } else if args.ranges.is_some() {
            ReadRange::Whole // simplified; multi-range snapshot covers whole file
        } else {
            ReadRange::Whole
        };

        let snapshot = FileSnapshot {
            canonical_resource_id,
            display_path: display_path.clone(),
            file_type: FileType::Text,
            size: file_size,
            mtime_secs: std::fs::metadata(&args.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            content_hash: Some(content_hash.clone()),
            range_hash: Some(content_hash.clone()),
            line_ending: LineEnding::Lf,
            encoding: Some("utf-8".into()),
            read_at: SystemTime::now(),
            environment_id: String::new(),
            read_range,
        };

        let result = ReadFileOutput {
            path: args.path,
            content: final_output,
            content_hash,
            total_lines,
            lines_returned,
            truncated,
            file_size_bytes: file_size,
            snapshot: Some(snapshot),
            render_format: render_format.to_string(),
        };

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

// ── Range rendering helpers ──────────────────────────────────────────

/// Render lines in L1 hashline format: each line is prefixed with
/// `{line_num}\t{short_hash}\t{content}`. The short hash is the first
/// 8 hex chars of SHA-256(line_content), enabling change detection
/// without re-hashing the entire file.
fn render_hashline(
    all_lines: &[&str],
    start_idx: usize,
    end_idx: usize,
    max_bytes: usize,
) -> (String, usize, bool) {
    use sha2::{Digest, Sha256};
    let mut output = String::new();
    let mut bytes = 0usize;
    let mut lines_returned = 0usize;
    let mut truncated = false;

    for (i, line) in all_lines[start_idx..end_idx].iter().enumerate() {
        let line_num = start_idx + i + 1;
        let mut h = Sha256::new();
        h.update(line.as_bytes());
        let short_hash = format!("{:x}", h.finalize());
        let short_hash = &short_hash[..8];
        let formatted = format!("{line_num}\t{short_hash}\t{line}\n");
        if bytes + formatted.len() > max_bytes {
            truncated = true;
            break;
        }
        bytes += formatted.len();
        lines_returned += 1;
        output.push_str(&formatted);
    }
    (output, lines_returned, truncated)
}

/// Render a single legacy offset/limit range.
fn render_single_range(
    all_lines: &[&str],
    offset: Option<usize>,
    limit: Option<usize>,
    max_bytes: usize,
) -> (String, usize, bool) {
    let total_lines = all_lines.len();
    let start = offset.unwrap_or(1).max(1);
    let lim = limit.unwrap_or(usize::MAX);
    let start_idx = (start - 1).min(total_lines);
    let end_idx = start_idx.saturating_add(lim).min(total_lines);

    let mut output = String::new();
    let mut bytes = 0usize;
    let mut lines_returned = 0usize;
    let mut truncated = false;

    for (i, line) in all_lines[start_idx..end_idx].iter().enumerate() {
        let line_num = start_idx + i + 1;
        let formatted = format!("{line_num}\t{line}\n");
        if bytes + formatted.len() > max_bytes {
            truncated = true;
            break;
        }
        bytes += formatted.len();
        lines_returned += 1;
        output.push_str(&formatted);
    }
    (output, lines_returned, truncated)
}

/// Render multiple ranges, concatenating with separators.
fn render_multi_range(
    all_lines: &[&str],
    ranges: &[ReadRange],
    max_bytes: usize,
) -> Result<(String, usize, bool), GrodexError> {
    let total_lines = all_lines.len();
    let mut output = String::new();
    let mut total_lines_returned = 0usize;
    let mut truncated = false;
    let mut bytes = 0usize;

    for (idx, range) in ranges.iter().enumerate() {
        if ranges.len() > 1 {
            let sep = format!("--- range {} ---\n", idx + 1);
            if bytes + sep.len() > max_bytes {
                truncated = true;
                break;
            }
            bytes += sep.len();
            output.push_str(&sep);
        }

        let (start_idx, end_idx) = match range {
            ReadRange::Whole => (0, total_lines),
            ReadRange::Lines { start_line, count } => {
                let s = ((*start_line as usize).saturating_sub(1)).min(total_lines);
                let c = count.map(|c| c as usize).unwrap_or(total_lines);
                (s, (s + c).min(total_lines))
            }
            ReadRange::Bytes { start_byte, count } => {
                // Convert byte range to line range by scanning offsets.
                let content: String = all_lines.join("\n");
                let sb = (*start_byte as usize).min(content.len());
                let eb = count
                    .map(|c| (sb + c as usize).min(content.len()))
                    .unwrap_or(content.len());
                // Map byte offsets back to line indices.
                let mut byte_offset = 0;
                let mut line_start = 0;
                let mut line_end = total_lines;
                for (i, line) in all_lines.iter().enumerate() {
                    if byte_offset + line.len() >= sb && line_start == 0 && byte_offset <= sb {
                        line_start = i;
                    }
                    byte_offset += line.len() + 1; // +1 for '\n'
                    if byte_offset >= eb {
                        line_end = (i + 1).min(total_lines);
                        break;
                    }
                }
                (line_start, line_end)
            }
            ReadRange::Pages { .. } => {
                return Err(GrodexError::ToolExecution(
                    "page ranges not supported for text files".into(),
                ));
            }
            ReadRange::Anchor { start_pattern, end_pattern } => {
                resolve_anchor_range(all_lines, start_pattern, end_pattern.as_deref())
            }
        };

        for (i, line) in all_lines[start_idx..end_idx].iter().enumerate() {
            let line_num = start_idx + i + 1;
            let formatted = format!("{line_num}\t{line}\n");
            if bytes + formatted.len() > max_bytes {
                truncated = true;
                break;
            }
            bytes += formatted.len();
            total_lines_returned += 1;
            output.push_str(&formatted);
        }
        if truncated {
            break;
        }
    }

    Ok((output, total_lines_returned, truncated))
}

/// Resolve an anchor range to (start_idx, end_idx) line indices.
///
/// Searches for the first line matching `start_pattern` (substring match).
/// If `end_pattern` is Some, extends to include lines through the first
/// match of `end_pattern` after the start (inclusive).
/// If `end_pattern` is None, returns just the single matched start line.
fn resolve_anchor_range(
    all_lines: &[&str],
    start_pattern: &str,
    end_pattern: Option<&str>,
) -> (usize, usize) {
    let total = all_lines.len();
    // Find start line (first match).
    let start_idx = all_lines
        .iter()
        .position(|l| l.contains(start_pattern));

    let Some(start) = start_idx else {
        return (0, 0); // pattern not found → empty range
    };

    if let Some(end_pat) = end_pattern {
        // Find end line: first match of end_pattern at or after start.
        let end_idx = all_lines[start..]
            .iter()
            .position(|l| l.contains(end_pat))
            .map(|p| (start + p + 1).min(total))
            .unwrap_or(total); // no end match → through EOF
        (start, end_idx)
    } else {
        // Single-line anchor: just the matched line.
        (start, start + 1)
    }
}

/// Type routing note based on file extension.
///
/// Returns a prefix string to prepend to the output for non-plain-text
/// file types. Returns empty string for standard text files.
fn type_routing_note(path: &str) -> String {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "rs" => "[file type: Rust source — syntax-aware rendering]".into(),
        "md" | "mdx" => "[file type: Markdown — rendered content]".into(),
        "json" => "[file type: JSON — structured data]".into(),
        "toml" => "[file type: TOML — configuration]".into(),
        "yaml" | "yml" => "[file type: YAML — configuration]".into(),
        "py" => "[file type: Python source]".into(),
        "js" | "ts" | "jsx" | "tsx" => "[file type: JavaScript/TypeScript source]".into(),
        "go" => "[file type: Go source]".into(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" => {
            "[file type: binary image — cannot display as text]".into()
        }
        "pdf" => "[file type: PDF binary — cannot display as text]".into(),
        _ => String::new(),
    }
}

#[derive(Debug, Clone)]
pub struct PreparedReadCall {
    pub args: ReadFileArgs,
    pub content: String,
    pub content_hash: String,
    pub total_lines: usize,
    pub file_size_bytes: u64,
    pub snapshot: FileSnapshot,
}

impl PreparedCall for PreparedReadCall {
    fn side_effect_hint(&self) -> SideEffectHint {
        SideEffectHint::ReadOnly
    }

    fn plan_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.args.path.as_bytes());
        h.update(self.args.offset.unwrap_or(0).to_le_bytes());
        h.update(self.args.limit.unwrap_or(0).to_le_bytes());
        h.update(self.args.max_bytes.unwrap_or(0).to_le_bytes());
        h.update(self.content_hash.as_bytes());
        let full = format!("{:x}", h.finalize());
        full[..16].to_string()
    }
}

impl BuiltInTool for ReadFileTool {
    type Input = ReadFileArgs;
    type Prepared = PreparedReadCall;
    type OkResult = ToolResultEnvelope;
    type Error = GrodexError;

    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error> {
        let content = std::fs::read_to_string(&input.path)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot read {}: {e}", input.path)))?;

        let file_size = content.len() as u64;
        let content_hash = {
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            format!("{:x}", h.finalize())
        };
        let all_lines: Vec<&str> = content.lines().collect();
        let total_lines = all_lines.len();

        let read_range = if input.offset.is_some() || input.limit.is_some() {
            ReadRange::Lines {
                start_line: input.offset.unwrap_or(1) as u64,
                count: input.limit.map(|v| v as u64),
            }
        } else {
            ReadRange::Whole
        };

        let canonical_path = crate::fsutil::canonicalize(std::path::Path::new(&input.path));
        let canonical_resource_id = format!("fs://{}", canonical_path.display());
        let display_path = PathBuf::from(&input.path);

        let snapshot = FileSnapshot {
            canonical_resource_id: canonical_resource_id.clone(),
            display_path: display_path.clone(),
            file_type: FileType::Text,
            size: file_size,
            mtime_secs: std::fs::metadata(&input.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            content_hash: Some(content_hash.clone()),
            range_hash: Some(content_hash.clone()),
            line_ending: LineEnding::Lf,
            encoding: Some("utf-8".into()),
            read_at: SystemTime::now(),
            environment_id: String::new(),
            read_range,
        };

        Ok(PreparedReadCall {
            args: input,
            content,
            content_hash,
            total_lines,
            file_size_bytes: file_size,
            snapshot,
        })
    }

    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error> {
        let start = std::time::Instant::now();
        let all_lines: Vec<&str> = prepared.content.lines().collect();
        let total_lines = all_lines.len();

        let offset = prepared.args.offset.unwrap_or(1).max(1);
        let limit = prepared.args.limit.unwrap_or(usize::MAX);
        let max_bytes = prepared.args.max_bytes.unwrap_or(usize::MAX);

        let start_idx = (offset - 1).min(total_lines);
        let end_idx = start_idx.saturating_add(limit).min(total_lines);

        // Hashline mode: when `format` is "hashline", use per-line SHA-256
        // hashing for change detection (L1 standardization).
        let use_hashline = prepared.args.format.as_deref() == Some("hashline");
        let (output, lines_returned, truncated) = if use_hashline {
            render_hashline(&all_lines, start_idx, end_idx, max_bytes)
        } else {
            let selected: Vec<&str> = all_lines[start_idx..end_idx].to_vec();
            let mut output = String::new();
            let mut bytes = 0usize;
            let mut lines_returned = 0usize;
            let mut truncated = false;

            for (i, line) in selected.iter().enumerate() {
                let line_num = start_idx + i + 1;
                let formatted = format!("{line_num}\t{line}\n");
                if bytes + formatted.len() > max_bytes {
                    truncated = true;
                    break;
                }
                bytes += formatted.len();
                lines_returned += 1;
                output.push_str(&formatted);
            }
            (output, lines_returned, truncated)
        };

        let render_format = if use_hashline {
            "hashline"
        } else {
            "line_numbered"
        };

        let result = ReadFileOutput {
            path: prepared.args.path.clone(),
            content: output.clone(),
            content_hash: prepared.content_hash.clone(),
            total_lines,
            lines_returned,
            truncated,
            file_size_bytes: prepared.file_size_bytes,
            snapshot: Some(prepared.snapshot.clone()),
            render_format: render_format.to_string(),
        };

        let output_serialized = serde_json::to_string(&result).ok();
        let retained_bytes = output.len() as u64;
        let original_bytes = prepared.file_size_bytes;

        let mut structured_data = BTreeMap::new();
        if let Some(s) = output_serialized {
            structured_data.insert("output_serialized".into(), serde_json::Value::String(s));
        }
        structured_data.insert(
            "snapshot".into(),
            serde_json::to_value(&prepared.snapshot).unwrap_or(serde_json::Value::Null),
        );

        let changed_resources = vec![ChangedResource {
            resource_id: prepared.snapshot.canonical_resource_id.clone(),
            display_path: prepared.snapshot.display_path.clone(),
            change_type: ChangeType::Metadata,
            before_hash: prepared.content_hash.clone().into(),
            after_hash: prepared.content_hash.clone().into(),
        }];

        let model_text = format!(
            "tool `read_file` ok: {} bytes / {} lines",
            output.len(),
            lines_returned
        );

        let envelope = ToolResultEnvelope {
            tool_call_id: String::new(),
            operation_id: None,
            capability_id: Some("read_file".into()),
            contract_version: 1,
            status: ToolStatus::Ok,
            summary: model_text.clone(),
            model_content: vec![ModelContent::Text(model_text)],
            structured_data,
            artifacts: vec![],
            changed_resources,
            truncation: TruncationInfo {
                original_bytes,
                retained_bytes,
                strategy: if truncated {
                    crate::common::TruncationStrategy::HeadOnly
                } else {
                    crate::common::TruncationStrategy::None
                },
                omitted: original_bytes.saturating_sub(retained_bytes),
            },
            wall_time: start.elapsed(),
            retryability: Retryability::Retryable,
            diagnostics: vec![],
        };

        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn read_file_basic() {
        use grodex_core::tool::ToolRuntime;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "line one").unwrap();
        writeln!(tmp, "line two").unwrap();
        writeln!(tmp, "line three").unwrap();

        let tool = ReadFileTool::new();
        let result = ToolRuntime::execute(&tool, serde_json::json!({"path": tmp.path()}), OperationId::new())
            .await
            .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.total_lines, 3);
        assert_eq!(output.lines_returned, 3);
        assert!(output.content.contains("1\tline one"));
        assert!(output.content.contains("3\tline three"));
    }

    #[tokio::test]
    async fn read_file_with_offset_limit() {
        use grodex_core::tool::ToolRuntime;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(tmp, "line {i}").unwrap();
        }

        let tool = ReadFileTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": tmp.path(), "offset": 5, "limit": 3}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.total_lines, 10);
        assert_eq!(output.lines_returned, 3);
        assert!(output.content.contains("5\tline 5"));
        assert!(output.content.contains("7\tline 7"));
    }

    #[tokio::test]
    async fn read_file_multi_range() {
        use grodex_core::tool::ToolRuntime;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 1..=20 {
            writeln!(tmp, "line {i}").unwrap();
        }

        let tool = ReadFileTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                "path": tmp.path(),
                "ranges": [
                    {"start_line": 1, "count": 3},
                    {"start_line": 18, "count": 3}
                ]
            }),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.lines_returned, 6);
        assert!(output.content.contains("1\tline 1"));
        assert!(output.content.contains("3\tline 3"));
        assert!(output.content.contains("18\tline 18"));
        assert!(output.content.contains("20\tline 20"));
        assert!(output.content.contains("--- range 1 ---"));
        assert!(output.content.contains("--- range 2 ---"));
    }

    #[tokio::test]
    async fn read_file_anchor_range() {
        use grodex_core::tool::ToolRuntime;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "header").unwrap();
        writeln!(tmp, "fn alpha() {{").unwrap();
        writeln!(tmp, "    let x = 1;").unwrap();
        writeln!(tmp, "}}").unwrap();
        writeln!(tmp, "fn beta() {{").unwrap();
        writeln!(tmp, "    let y = 2;").unwrap();
        writeln!(tmp, "}}").unwrap();

        let tool = ReadFileTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                "path": tmp.path(),
                "ranges": [
                    {"start_pattern": "fn alpha", "end_pattern": "}"}
                ]
            }),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        // Should include lines from "fn alpha" through "}"
        assert!(output.content.contains("fn alpha"));
        assert!(output.content.contains("let x = 1"));
        assert!(output.content.contains("}"));
        // Should NOT include beta
        assert!(!output.content.contains("fn beta"));
    }

    #[test]
    fn type_routing_detects_extensions() {
        assert!(type_routing_note("foo.rs").contains("Rust"));
        assert!(type_routing_note("bar.md").contains("Markdown"));
        assert!(type_routing_note("img.png").contains("binary image"));
        assert!(type_routing_note("unknown.xyz").is_empty());
    }
}
