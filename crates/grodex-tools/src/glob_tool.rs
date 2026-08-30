//! GlobTool — find files by glob pattern, like fd / find.
//!
//! Walks a directory tree and returns file paths matching a glob pattern
//! (e.g. `**/*.rs`, `src/**/*.ts`). Results are sorted by modification time
//! (newest first) to surface recently changed files.

use crate::blocking::run_blocking_io;
use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 返回文件路径数上限。
const DEFAULT_MAX_RESULTS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobArgs {
    /// Glob pattern (e.g. "**/*.rs", "src/**/*.ts", "*.toml").
    pub pattern: String,
    /// Root directory to search. Default: current directory.
    #[serde(default)]
    pub path: Option<String>,
    /// Max number of results. Default: 200.
    #[serde(default)]
    pub max_results: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobOutput {
    pub pattern: String,
    pub matches: Vec<String>,
    pub total: usize,
    pub truncated: bool,
}

pub struct GlobTool;

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for GlobTool {
    type Args = GlobArgs;
    type Output = GlobOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "glob".into(),
            display_name: "Glob".into(),
            description: "Find files matching a glob pattern. Walks a directory tree and returns file paths matching patterns like '**/*.rs', 'src/**/*.ts', '*.toml'. Results are sorted by modification time (newest first). Use this to locate files by name pattern instead of reading files to search.".into(),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::ReadOnly,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern: '**' matches any path depth, '*' matches within a directory, '?' matches single char, '{a,b}' alternation. E.g. '**/*.rs', 'src/**/*.ts'"},
                "path": {"type": "string", "description": "Root directory to search. Default: current directory."},
                "max_results": {"type": "integer", "description": "Max number of file paths to return. Default: 200."}
            },
            "required": ["pattern"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "matches": {"type": "array", "items": {"type": "string"}, "description": "Matching file paths, sorted by modification time (newest first)"},
                "total": {"type": "integer", "description": "Total matches found (may exceed matches.length if truncated)"},
                "truncated": {"type": "boolean"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for GlobTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: GlobArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let result = run_blocking_io(move || -> Result<GlobOutput, GrodexError> {
            run_glob(args)
        })
        .await?;

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

fn run_glob(args: GlobArgs) -> Result<GlobOutput, GrodexError> {
    let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
    let root = args.path.as_deref().unwrap_or(".");

    // 把 **/*.rs 格式的 glob 转成正则,匹配相对路径。
    let re_str = glob_pattern_to_regex(&args.pattern);
    let re = Regex::new(&re_str)
        .map_err(|e| GrodexError::ToolExecution(format!("invalid glob pattern: {e}")))?;

    let root_path = Path::new(root);
    let mut matches: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();

    collect_matching_files(root_path, root_path, &re, &mut matches)
        .map_err(|e| GrodexError::ToolExecution(format!("walk {}: {e}", root)))?;

    // 按修改时间倒序 (最新在前)。
    matches.sort_by(|a, b| b.1.cmp(&a.1));

    let total = matches.len();
    let truncated = total > max_results;
    let paths: Vec<String> = matches
        .into_iter()
        .take(max_results)
        .map(|(p, _)| p.to_string_lossy().to_string())
        .collect();

    Ok(GlobOutput {
        pattern: args.pattern,
        matches: paths,
        total,
        truncated,
    })
}

/// 递归收集匹配正则的文件路径。
fn collect_matching_files(
    root: &Path,
    current: &Path,
    re: &Regex,
    matches: &mut Vec<(PathBuf, std::time::SystemTime)>,
) -> Result<(), String> {
    if current.is_file() {
        // 对单个文件直接匹配路径。
        let rel = current
            .strip_prefix(root)
            .unwrap_or(current)
            .to_string_lossy()
            .to_string();
        if re.is_match(&rel) {
            let mtime = fs::metadata(current)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            matches.push((current.to_path_buf(), mtime));
        }
        return Ok(());
    }

    let mut stack = vec![current.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if path.is_dir() {
                // 跳过隐藏目录和常见忽略目录。
                if name_str.starts_with('.') || name_str == "node_modules" || name_str == "target" {
                    continue;
                }
                stack.push(path);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if re.is_match(&rel) {
                    let mtime = fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    matches.push((path, mtime));
                }
            }
        }
    }
    Ok(())
}

/// 把 glob 模式转换为正则表达式,匹配相对于根的路径。
///
/// - `**` → `.*` (跨目录)
/// - `*` → `[^/]*` (单层)
/// - `?` → `.` (单字符)
/// - `{a,b}` → `(a|b)`
/// - `. ` → `\.`
fn glob_pattern_to_regex(glob: &str) -> String {
    let mut out = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // 吃掉可能紧跟的 /
                    if chars.peek() == Some(&'/') {
                        chars.next();
                    }
                    out.push_str(".*");
                } else {
                    out.push_str("[^/]*");
                }
            }
            '?' => out.push('.'),
            '.' | '(' | ')' | '+' | '|' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '/' => out.push('/'),
            '{' => {
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
    fn test_glob_pattern_to_regex() {
        assert_eq!(glob_pattern_to_regex("**/*.rs"), "^.*[^/]*\\.rs$");
        assert_eq!(glob_pattern_to_regex("*.toml"), "^[^/]*\\.toml$");
        assert_eq!(glob_pattern_to_regex("src/**/*.ts"), "^src/.*[^/]*\\.ts$");
    }

    #[test]
    fn test_glob_basic() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a.rs"), "fn a()").unwrap();
        fs::write(sub.join("b.ts"), "const b").unwrap();
        fs::write(dir.path().join("c.rs"), "fn c()").unwrap();
        fs::write(dir.path().join("d.txt"), "hello").unwrap();

        let args = GlobArgs {
            pattern: "**/*.rs".into(),
            path: Some(dir.path().to_string_lossy().to_string()),
            max_results: None,
        };
        let result = run_glob(args).unwrap();
        assert_eq!(result.total, 2);
        assert!(result.matches.iter().all(|p| p.ends_with(".rs")));
    }

    #[test]
    fn test_glob_single_level() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.toml"), "").unwrap();
        fs::write(dir.path().join("b.toml"), "").unwrap();
        fs::write(dir.path().join("c.txt"), "").unwrap();

        let args = GlobArgs {
            pattern: "*.toml".into(),
            path: Some(dir.path().to_string_lossy().to_string()),
            max_results: None,
        };
        let result = run_glob(args).unwrap();
        assert_eq!(result.total, 2);
    }

    #[test]
    fn test_glob_max_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            fs::write(dir.path().join(format!("f{}.rs", i)), "").unwrap();
        }
        let args = GlobArgs {
            pattern: "*.rs".into(),
            path: Some(dir.path().to_string_lossy().to_string()),
            max_results: Some(3),
        };
        let result = run_glob(args).unwrap();
        assert_eq!(result.matches.len(), 3);
        assert_eq!(result.total, 10);
        assert!(result.truncated);
    }
}
