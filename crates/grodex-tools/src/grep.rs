//! GrepTool — search file contents with regex, like ripgrep/grep -rn.
//!
//! Walks a directory tree (or reads a single file), applies a regex pattern
//! to each line, and returns matches with file path + line number + content.
//! Supports file-pattern glob filtering, case-insensitive mode, context lines,
//! and three output modes: content (default), files_with_matches, count.

use crate::blocking::run_blocking_io;
use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

/// 返回结果条数上限,防止海量匹配撑爆 prompt。
const DEFAULT_MAX_MATCHES: usize = 200;
/// 单个文件读取上限,防止读取超大文件导致 OOM。
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Output mode for grep results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    /// 返回匹配行内容 + 文件路径 + 行号 (default).
    #[default]
    Content,
    /// 只返回含匹配的文件路径列表.
    FilesWithMatches,
    /// 每个文件的匹配行计数.
    Count,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepArgs {
    /// Regex pattern to search for.
    pub pattern: String,
    /// Directory or file to search. Default: current directory.
    #[serde(default)]
    pub path: Option<String>,
    /// Glob pattern to filter files (e.g. "*.rs", "*.{ts,tsx}").
    /// When omitted, all files are searched.
    #[serde(default)]
    pub glob: Option<String>,
    /// Case-insensitive matching. Default: false.
    #[serde(default)]
    pub case_insensitive: Option<bool>,
    /// Output mode: content | files_with_matches | count. Default: content.
    #[serde(default)]
    pub output_mode: Option<GrepOutputMode>,
    /// Lines of context after each match (like grep -A).
    #[serde(default)]
    pub after_context: Option<usize>,
    /// Lines of context before each match (like grep -B).
    #[serde(default)]
    pub before_context: Option<usize>,
    /// Max number of result entries (files or matches). Default: 200.
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
    /// Matches within the line: (start_byte, end_byte).
    pub match_positions: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepOutput {
    pub pattern: String,
    pub mode: String,
    /// In content mode: list of matches. Empty in other modes.
    pub matches: Vec<GrepMatch>,
    /// In files_with_matches mode: list of file paths.
    pub files_with_matches: Vec<String>,
    /// In count mode: per-file { path, count }.
    pub counts: Vec<FileCount>,
    pub total_matches: usize,
    pub files_searched: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCount {
    pub path: String,
    pub count: usize,
}

pub struct GrepTool;

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GrepTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for GrepTool {
    type Args = GrepArgs;
    type Output = GrepOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "grep".into(),
            display_name: "Grep".into(),
            description: "Search file contents with regex. Like grep -rn: walks a directory tree, applies a regex pattern to each line, returns matches with file path + line number + content. Supports glob file filtering, case-insensitive mode, context lines, and output modes (content/files_with_matches/count).".into(),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::ReadOnly,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for"},
                "path": {"type": "string", "description": "Directory or file to search. Default: current directory."},
                "glob": {"type": "string", "description": "Glob pattern to filter files (e.g. '*.rs', '*.{ts,tsx}')"},
                "case_insensitive": {"type": "boolean", "description": "Case-insensitive matching. Default: false."},
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode. Default: content. 'files_with_matches' returns only file paths. 'count' returns per-file match counts."
                },
                "after_context": {"type": "integer", "description": "Lines of context to show after each match (grep -A). Default: 0."},
                "before_context": {"type": "integer", "description": "Lines of context to show before each match (grep -B). Default: 0."},
                "max_results": {"type": "integer", "description": "Max result entries. Default: 200."}
            },
            "required": ["pattern"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "mode": {"type": "string"},
                "matches": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "line_number": {"type": "integer"},
                            "line": {"type": "string"},
                            "match_positions": {"type": "array", "items": {"type": "array", "items": {"type": "integer"}, "maxItems": 2}}
                        }
                    }
                },
                "files_with_matches": {"type": "array", "items": {"type": "string"}},
                "counts": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "count": {"type": "integer"}}}},
                "total_matches": {"type": "integer"},
                "files_searched": {"type": "integer"},
                "truncated": {"type": "boolean"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for GrepTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: GrepArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let result = run_blocking_io(move || -> Result<GrepOutput, GrodexError> {
            run_grep(args)
        })
        .await?;

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

fn run_grep(args: GrepArgs) -> Result<GrepOutput, GrodexError> {
    let mode = args.output_mode.unwrap_or_default();
    let max_results = args.max_results.unwrap_or(DEFAULT_MAX_MATCHES);
    let case_insensitive = args.case_insensitive.unwrap_or(false);
    let before_ctx = args.before_context.unwrap_or(0);
    let after_ctx = args.after_context.unwrap_or(0);
    let root = args.path.as_deref().unwrap_or(".");

    // Build regex.
    let re = {
        let pattern = if case_insensitive {
            format!("(?i){}", args.pattern)
        } else {
            args.pattern.clone()
        };
        Regex::new(&pattern).map_err(|e| GrodexError::ToolExecution(format!("invalid regex pattern: {e}")))?
    };

    // Build glob filter (convert simple glob to regex).
    let glob_re = match &args.glob {
        Some(g) => Some(glob_to_regex(g))
            .and_then(|p| Regex::new(&p).ok())
            .map(|re| (re, g.clone())),
        None => None,
    };

    let mut output = GrepOutput {
        pattern: args.pattern,
        mode: format!("{:?}", mode).to_lowercase(),
        matches: Vec::new(),
        files_with_matches: Vec::new(),
        counts: Vec::new(),
        total_matches: 0,
        files_searched: 0,
        truncated: false,
    };

    let files = collect_files(Path::new(root), &glob_re).map_err(|e| {
        GrodexError::ToolExecution(format!("cannot walk {}: {e}", root))
    })?;

    output.files_searched = files.len();

    for file in &files {
        if output.total_matches >= max_results {
            output.truncated = true;
            break;
        }
        match search_file(file, &re, mode, before_ctx, after_ctx, max_results - output.total_matches, &mut output) {
            Ok(_) => {}
            Err(_) => continue, // 跳过不可读/二进制文件
        }
    }

    Ok(output)
}

/// Recursively collect files under `root` that match the optional glob filter.
fn collect_files(root: &Path, glob_re: &Option<(Regex, String)>) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
        return Ok(files);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // 跳过隐藏目录和常见忽略目录。
            if path.is_dir() {
                if name_str.starts_with('.') || name_str == "node_modules" || name_str == "target" {
                    continue;
                }
                stack.push(path);
            } else if path.is_file() {
                if let Some((re, _)) = glob_re {
                    if !re.is_match(&name_str) {
                        continue;
                    }
                }
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Search a single file for regex matches and update the output.
fn search_file(
    path: &Path,
    re: &Regex,
    mode: GrepOutputMode,
    before_ctx: usize,
    after_ctx: usize,
    remaining: usize,
    output: &mut GrepOutput,
) -> Result<(), std::io::Error> {
    // 跳过超大文件。
    let meta = fs::metadata(path)?;
    if meta.len() as usize > MAX_FILE_BYTES {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let display_path = path.to_string_lossy().to_string();
    let lines: Vec<&str> = content.lines().collect();
    let mut file_count = 0usize;
    let mut file_has_match = false;

    let mut match_lines: VecDeque<usize> = VecDeque::new(); // for context

    for (idx, line) in lines.iter().enumerate() {
        if re.is_match(line) {
            output.total_matches += 1;
            file_count += 1;
            file_has_match = true;
            match_lines.push_back(idx);

            if mode == GrepOutputMode::Content && output.matches.len() < remaining {
                // 计算所有匹配位置
                let positions: Vec<(usize, usize)> = re
                    .find_iter(line)
                    .map(|m| (m.start(), m.end()))
                    .collect();

                // 附加上下文行
                let ctx_start = idx.saturating_sub(before_ctx);
                let ctx_end = (idx + after_ctx).min(lines.len() - 1);
                if before_ctx > 0 || after_ctx > 0 {
                    // 把上下文也带上,用 "..." 分隔
                    let mut full_line = String::new();
                    for i in ctx_start..=ctx_end {
                        let prefix = if i == idx { "" } else { "  " };
                        full_line.push_str(&format!("{}{}→ {}\n", prefix, i + 1, lines[i]));
                    }
                    output.matches.push(GrepMatch {
                        path: display_path.clone(),
                        line_number: idx + 1,
                        line: full_line.trim_end().to_string(),
                        match_positions: positions,
                    });
                } else {
                    output.matches.push(GrepMatch {
                        path: display_path.clone(),
                        line_number: idx + 1,
                        line: line.to_string(),
                        match_positions: positions,
                    });
                }
            }

            if mode == GrepOutputMode::FilesWithMatches && file_has_match {
                // 只记一次
            }

            if output.total_matches >= remaining + output.matches.len() && mode != GrepOutputMode::Content {
                break;
            }
        }
    }

    match mode {
        GrepOutputMode::FilesWithMatches => {
            if file_has_match {
                output.files_with_matches.push(display_path);
            }
        }
        GrepOutputMode::Count => {
            if file_count > 0 {
                output.counts.push(FileCount {
                    path: display_path,
                    count: file_count,
                });
            }
        }
        GrepOutputMode::Content => {}
    }

    Ok(())
}

/// Convert a simple glob pattern to a regex string.
/// Supports: * (any chars except /), ? (single char), {a,b} alternation.
fn glob_to_regex(glob: &str) -> String {
    let mut out = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '.' | '(' | ')' | '+' | '|' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '{' => {
                // {a,b,c} -> (a|b|c)
                let mut alts = String::new();
                while let Some(&nc) = chars.peek() {
                    if nc == '}' {
                        chars.next();
                        break;
                    }
                    if nc == ',' {
                        chars.next();
                        alts.push('|');
                    } else {
                        alts.push(nc);
                        chars.next();
                    }
                }
                out.push('(');
                out.push_str(&alts);
                out.push(')');
            }
            _ => out.push(c),
        }
    }
    out.push('$');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_to_regex() {
        assert_eq!(glob_to_regex("*.rs"), "^.*\\.rs$");
        assert_eq!(glob_to_regex("*.{ts,tsx}"), "^.*\\.(ts|tsx)$");
        assert_eq!(glob_to_regex("test_?"), "^test_.$");
    }

    #[test]
    fn test_grep_basic() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("a.rs");
        fs::write(&f1, "fn hello() {}\nfn world() {}\n").unwrap();
        let f2 = dir.path().join("b.txt");
        fs::write(&f2, "hello world\n").unwrap();

        let args = GrepArgs {
            pattern: "fn (\\w+)".into(),
            path: Some(dir.path().to_string_lossy().to_string()),
            glob: Some("*.rs".into()),
            case_insensitive: Some(false),
            output_mode: Some(GrepOutputMode::Content),
            after_context: None,
            before_context: None,
            max_results: None,
        };
        let result = run_grep(args).unwrap();
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.files_searched, 1); // only .rs files
        assert!(result.matches[0].line.contains("fn hello"));
    }

    #[test]
    fn test_grep_files_with_matches() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "hello\n").unwrap();
        fs::write(dir.path().join("b.rs"), "world\n").unwrap();

        let args = GrepArgs {
            pattern: "hello".into(),
            path: Some(dir.path().to_string_lossy().to_string()),
            glob: None,
            case_insensitive: None,
            output_mode: Some(GrepOutputMode::FilesWithMatches),
            after_context: None,
            before_context: None,
            max_results: None,
        };
        let result = run_grep(args).unwrap();
        assert_eq!(result.files_with_matches.len(), 1);
        assert!(result.files_with_matches[0].ends_with("a.rs"));
    }

    #[test]
    fn test_grep_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "Hello\nHELLO\nworld\n").unwrap();

        let args = GrepArgs {
            pattern: "hello".into(),
            path: Some(dir.path().to_string_lossy().to_string()),
            glob: None,
            case_insensitive: Some(true),
            output_mode: Some(GrepOutputMode::Count),
            after_context: None,
            before_context: None,
            max_results: None,
        };
        let result = run_grep(args).unwrap();
        assert_eq!(result.counts.len(), 1);
        assert_eq!(result.counts[0].count, 2);
    }
}
