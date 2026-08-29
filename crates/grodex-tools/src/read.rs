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

/// 默认返回行数上限（当模型未指定 limit 时）。防止模型无意中请求
/// 整个超大文件，导致巨大内存对象与 Prompt 占用。
const DEFAULT_MAX_LINES: usize = 2000;
/// 默认返回字节数上限（当模型未指定 max_bytes 时）。
const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// 不可由模型突破的硬上限。即使模型显式传入更大的 max_bytes，
/// 实际返回也不会超过此字节数，防止一次性读入超大文件导致 OOM。
const HARD_CAP_BYTES: usize = 8 * 1024 * 1024;

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
    /// 当 `truncated=true` 时，下一页的起始行号（1-indexed），
    /// 引导模型分页读取。`None` 表示无分页或已是最后一页。
    /// 仅 single-range / hashline 模式有意义；multi-range 模式始终为 `None`。
    #[serde(default)]
    pub next_offset: Option<usize>,
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
                "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed). Default: 1."},
                "limit": {"type": "integer", "description": "Maximum number of lines to return. Default: 2000 if omitted."},
                "max_bytes": {"type": "integer", "description": "Maximum bytes to return. Default: 262144 (256KB); hard-capped at 8MB even if larger."},
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
                "file_size_bytes": {"type": "integer"},
                "next_offset": {
                    "type": "integer",
                    "description": "When truncated=true, the next page's start line (1-indexed). Use as offset in the next read_file call."
                }
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

        // 把整文件读取、SHA-256、行索引、渲染等阻塞操作移到
        // spawn_blocking 线程池，并经全局 Semaphore 限流（T3），
        // 避免大文件读取占用 tokio runtime worker。
        // 安全默认值 + 不可突破硬上限（T2）：
        //   未指定 max_bytes → 用保守默认（256KB）；
        //   模型显式传入更大的值 → 仍被 HARD_CAP_BYTES 夹断，防 OOM。
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .min(HARD_CAP_BYTES);
        // Hashline mode: when `format` is "hashline", use per-line SHA-256
        // hashing for change detection (L1 standardization).
        let use_hashline = args.format.as_deref() == Some("hashline");

        let result = crate::blocking::run_blocking_io(move || -> Result<ReadFileOutput, GrodexError> {
        // T1 流式范围 I/O：
        //   - single-range / hashline：单遍流式扫描，只把请求窗口内的行
        //     保留进输出，整文件哈希与 total_lines 在同一遍中算出，
        //     内存 O(输出+缓冲) 而非 O(文件大小)。
        //   - multi-range / anchor：可能请求多个不连续窗口与字节范围，
        //     仍走整文件读取路径（高级特性，按需保留）。
        let (output, lines_returned, truncated, total_lines, content_hash, file_size) =
            if let Some(ref ranges) = args.ranges {
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
                let (output, lines_returned, truncated) =
                    render_multi_range(&all_lines, ranges, max_bytes)?;
                (output, lines_returned, truncated, total_lines, content_hash, file_size)
            } else {
                let start_line = args.offset.unwrap_or(1).max(1);
                let line_limit = args.limit.unwrap_or(DEFAULT_MAX_LINES);
                let sr = stream_read_lines(&args.path, start_line, line_limit, max_bytes, use_hashline)?;
                (sr.output, sr.lines_returned, sr.truncated, sr.total_lines, sr.content_hash, sr.file_size)
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

        // 分页引导（T2）：仅 single-range / hashline 模式下被截断时
        // 给出下一页起点，引导模型用 offset=next_offset 继续读取。
        let next_offset = if truncated && args.ranges.is_none() {
            let start_line = args.offset.unwrap_or(1);
            Some(start_line + lines_returned)
        } else {
            None
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
                next_offset,
            };

            Ok(result)
        })
        .await?;

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

// ── Range rendering helpers ──────────────────────────────────────────

/// `stream_read_lines` 的产出。
struct StreamReadResult {
    output: String,
    lines_returned: usize,
    truncated: bool,
    total_lines: usize,
    content_hash: String,
    file_size: u64,
}

/// 单遍流式读取（T1：I/O 边界匹配请求范围）。
///
/// 旧实现 `std::fs::read_to_string` + `content.lines().collect()` 会把整
/// 个文件装进一个 `String` 并生成 `Vec<&str>`，再从中切出请求窗口——
/// 对超大文件，内存为 O(文件大小)。
///
/// 本函数改为通过 `BufReader` 单遍扫描：
///   - 把每个读到的字节喂给 SHA-256 哈希器 → 得到整文件 `content_hash`；
///   - 统计换行 → 得到 `total_lines`；
///   - 仅当当前行号落在请求窗口 `[start_line, start_line+line_limit)` 内
///     时，才把它（带行号前缀）追加进输出，并受 `max_bytes` 夹断。
///
/// 文件仍会被读到末尾（为得到整文件哈希与总行数所必需），但只有请求
/// 窗口被保留在内存中，内存为 O(输出+读缓冲) 而非 O(文件大小)。
fn stream_read_lines(
    path: &str,
    start_line: usize,
    line_limit: usize,
    max_bytes: usize,
    use_hashline: bool,
) -> Result<StreamReadResult, GrodexError> {
    use std::io::BufRead;

    let file = std::fs::File::open(path)
        .map_err(|e| GrodexError::ToolExecution(format!("cannot read {}: {e}", path)))?;
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut total_lines = 0usize;
    let mut lines_returned = 0usize;
    let mut bytes = 0usize;
    let mut output = String::new();
    let mut byte_capped = false;
    let mut line_buf: Vec<u8> = Vec::with_capacity(8192);

    loop {
        line_buf.clear();
        let n = reader
            .read_until(b'\n', &mut line_buf)
            .map_err(|e| GrodexError::ToolExecution(format!("read {}: {e}", path)))?;
        if n == 0 {
            break;
        }
        // 整文件 SHA-256：喂入原始字节（含尾随换行）。
        hasher.update(&line_buf[..n]);
        total_lines += 1;

        // 仅采集请求窗口内的行。
        let in_window = total_lines >= start_line && lines_returned < line_limit;
        if in_window && !byte_capped {
            // 去掉尾随换行 / 回车用于展示。
            let mut end = n;
            if end > 0 && line_buf[end - 1] == b'\n' {
                end -= 1;
                if end > 0 && line_buf[end - 1] == b'\r' {
                    end -= 1;
                }
            }
            let line_str = String::from_utf8_lossy(&line_buf[..end]);
            let formatted = if use_hashline {
                let mut h = Sha256::new();
                h.update(line_str.as_bytes());
                let short = format!("{:x}", h.finalize());
                format!("{}\t{}\t{}\n", total_lines, &short[..8], line_str)
            } else {
                format!("{}\t{}\n", total_lines, line_str)
            };
            if bytes + formatted.len() > max_bytes {
                byte_capped = true;
                // 继续读到末尾以完成哈希与总行数统计。
            } else {
                bytes += formatted.len();
                lines_returned += 1;
                output.push_str(&formatted);
            }
        }
    }

    // 截断：请求行窗口未到文件末尾，或字节上限在窗口中途被触发。
    let start_idx = start_line.saturating_sub(1);
    let end_idx = (start_idx + line_limit).min(total_lines);
    let truncated = byte_capped || end_idx < total_lines;

    let content_hash = format!("{:x}", hasher.finalize());
    Ok(StreamReadResult {
        output,
        lines_returned,
        truncated,
        total_lines,
        content_hash,
        file_size,
    })
}

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
    // 行范围未覆盖到文件末尾 → 分页截断，引导模型用 next_offset 续读。
    let mut truncated = end_idx < all_lines.len();

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
                // T1b/T4：字节范围 → 行范围。旧实现 `all_lines.join("\n")`
                // 会为取总长度而分配整文件副本；这里用前缀和公式零分配得到
                // 等价总长（join 长度 = Σ line.len() + (行数-1) 个 '\n'），
                // 再单遍扫描把字节偏移映射到行索引。
                let total_bytes: usize = all_lines
                    .iter()
                    .map(|l| l.len())
                    .sum::<usize>()
                    .saturating_add(total_lines.saturating_sub(1));
                let sb = (*start_byte as usize).min(total_bytes);
                let eb = count
                    .map(|c| (sb + c as usize).min(total_bytes))
                    .unwrap_or(total_bytes);
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

        let next_offset = if truncated && prepared.args.ranges.is_none() {
            let start_line = prepared.args.offset.unwrap_or(1);
            Some(start_line + lines_returned)
        } else {
            None
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
            next_offset,
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
    async fn read_file_default_limit_truncates_and_returns_next_offset() {
        use grodex_core::tool::ToolRuntime;
        // 不传 limit/max_bytes 时，默认上限应生效：截断 + 给出 next_offset
        // 引导分页（T2 安全默认值）。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        let content: String = (1..=DEFAULT_MAX_LINES + 5)
            .map(|i| format!("line {i}\n"))
            .collect();
        std::fs::write(&path, content).unwrap();

        let tool = ReadFileTool::new();
        let result =
            ToolRuntime::execute(&tool, serde_json::json!({"path": path}), OperationId::new())
                .await
                .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.total_lines, DEFAULT_MAX_LINES + 5);
        assert!(output.truncated, "未传 limit 时应被默认上限截断");
        assert_eq!(output.lines_returned, DEFAULT_MAX_LINES);
        assert_eq!(output.next_offset, Some(DEFAULT_MAX_LINES + 1));
    }

    #[tokio::test]
    async fn read_file_hard_cap_clamps_oversized_max_bytes() {
        use grodex_core::tool::ToolRuntime;
        // 模型显式传入远超 HARD_CAP 的 max_bytes，实际返回仍被夹断（T2）。
        // 造一个 > HARD_CAP 的文件，传 max_bytes=usize::MAX，验证返回被
        // 限制在 HARD_CAP 以内。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.bin");
        // 每行 64 字节，共 (HARD_CAP/64)+100 行，总量略超 HARD_CAP。
        let line: String = "a".repeat(63) + "\n";
        let n_lines = HARD_CAP_BYTES / 64 + 100;
        let mut content = String::with_capacity(n_lines * 64);
        for _ in 0..n_lines {
            content.push_str(&line);
        }
        std::fs::write(&path, content).unwrap();

        let tool = ReadFileTool::new();
        // 传 limit=MAX 禁用行截断，让全文件进入渲染，从而真正触发
        // byte 维度的 HARD_CAP 夹断（T2）。
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "limit": usize::MAX, "max_bytes": usize::MAX}),
            OperationId::new(),
        )
        .await
        .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        assert!(output.truncated, "max_bytes=MAX 应被 HARD_CAP 夹断为 truncated");
        assert!(
            output.content.len() <= HARD_CAP_BYTES,
            "返回内容不应超过 HARD_CAP ({}), got {}",
            HARD_CAP_BYTES,
            output.content.len()
        );
    }

    #[tokio::test]
    async fn read_file_streaming_window_in_middle_of_large_file() {
        use grodex_core::tool::ToolRuntime;
        // T1：流式范围 I/O。构造一个远大于默认上限的文件，请求中间一段
        // 窗口，验证：只返回窗口内行、total_lines 为整文件行数、truncated
        // 为真、next_offset 指向窗口末尾的下一行。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.log");
        let total = 10_000;
        let content: String = (1..=total).map(|i| format!("entry {i}\n")).collect();
        std::fs::write(&path, content).unwrap();

        let offset = 5_000;
        let limit = 10;
        let tool = ReadFileTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "offset": offset, "limit": limit}),
            OperationId::new(),
        )
        .await
        .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.total_lines, total, "流式扫描应统计整文件行数");
        assert_eq!(output.lines_returned, limit);
        assert!(output.truncated, "窗口未覆盖整文件应 truncated");
        assert_eq!(output.next_offset, Some(offset + limit));
        // 窗口首尾行应在，窗口外的不应在。
        assert!(output.content.contains(&format!("{offset}\tentry {offset}")));
        assert!(output
            .content
            .contains(&format!("{}\tentry {}", offset + limit - 1, offset + limit - 1)));
        assert!(!output.content.contains("entry 4999"));
        assert!(!output.content.contains("entry 5010"));
    }

    #[tokio::test]
    async fn read_file_byte_range_maps_to_lines() {
        use grodex_core::tool::ToolRuntime;
        // T1b/T4：字节范围映射到行，且不再做整文件 join 复制。
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // "aaaa\nbbbb\ncccc\n" → 字节 5..10 落在第二行 "bbbb"。
        writeln!(tmp, "aaaa").unwrap();
        writeln!(tmp, "bbbb").unwrap();
        writeln!(tmp, "cccc").unwrap();

        let tool = ReadFileTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                "path": tmp.path(),
                "ranges": [{"start_byte": 5, "count": 5}]
            }),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: ReadFileOutput = serde_json::from_value(result).unwrap();
        assert!(output.content.contains("2\tbbbb"), "byte range 5..10 应映射到第 2 行");
        assert!(!output.content.contains("aaaa"));
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
