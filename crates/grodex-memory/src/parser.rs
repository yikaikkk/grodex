//! Markdown → MemoryUnit 生产解析器。
//!
//! 设计 §7: Markdown 文件始终是事实来源。此模块负责:
//! 1. 将 Markdown 按标题分块成 sections
//! 2. 提取 `<!-- memory-unit: {"id":"mem_xxx","kind":"decision"} -->` 注释
//!    作为稳定 ID 载体
//! 3. 缺少注释时自动生成稳定 ID (基于 path + section + content hash)
//! 4. 自动推断 MemoryKind (根据 section 标题前缀 / 内容关键字)
//! 5. 可写回带 ID 的 Markdown 内容,以便后续修改时保持稳定
//!
//! Section 划分规则:
//! - `#` 标题行是 section 边界
//! - 连续的非标题文本归入最近的标题
//! - 文档顶部的无标题内容归入 `__preamble__` section
//! - 单个 section 内容过长时按段落分 chunks,共享 section 名
//!
//! 稳定 ID 格式: `mem_{sha256(path + ":" + section_name)[:10]}`
//! 若用户手写了 `<!-- memory-unit: {"id":"mem_abc123"} -->`,优先使用

use crate::types::{MemoryKind, MemoryScope, MemoryUnit, UnitStatus};
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 单个 section 中提取的 memory 块。
#[derive(Debug, Clone)]
pub struct ParsedMemoryChunk {
    /// 来自 HTML 注释的显式 ID,或自动生成的。
    pub explicit_id: Option<String>,
    /// 解析到的 kind (来自注释或 section 标题启发式)。
    pub explicit_kind: Option<MemoryKind>,
    /// section 名称 (如 "## Hard Constraints")。
    pub section: String,
    /// 该 chunk 的正文文本 (不含标题和 ID 注释)。
    pub content: String,
    /// 该 chunk 在原文件中的起始行 (1-based, 用于回写)。
    pub start_line: usize,
    /// 该 chunk 在原文件中的结束行 (inclusive)。
    pub end_line: usize,
}

/// 单个文件的解析结果。
#[derive(Debug, Clone)]
pub struct ParsedMemoryFile {
    pub path: String,
    pub chunks: Vec<ParsedMemoryChunk>,
    /// 需要写回磁盘的新内容 (当有缺失 ID 被补写时为 Some)。
    pub rewritten_content: Option<String>,
}

/// 写在 Markdown 中的 HTML 注释元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryUnitMarker {
    pub id: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// 单 chunk 最大字符数。超过则按段落切。
const MAX_CHUNK_CHARS: usize = 1500;

lazy_static::lazy_static! {
    /// 匹配 `<!-- memory-unit: { ... } -->`
    static ref MEM_UNIT_RE: Regex = Regex::new(
        r#"<!--\s*memory-unit:\s*(\{[^}]*\})\s*-->"#
    ).unwrap();
    /// 匹配 `#` 标题行。
    static ref HEADING_RE: Regex = Regex::new(r"^(#+)\s+(.+?)\s*#*\s*$").unwrap();
}

impl ParsedMemoryFile {
    /// 解析单个 Markdown 文件为若干 memory chunks。
    pub fn parse(path: &str, content: &str) -> Self {
        let lines: Vec<&str> = content.lines().collect();

        // Step 1: 按标题划 sections (start_line, heading_text, start_line_after_heading)
        let mut sections: Vec<(usize, String, usize)> = Vec::new();
        // 处理 preamble (第一个标题之前的内容)
        let mut first_heading_idx = None;
        for (i, line) in lines.iter().enumerate() {
            if HEADING_RE.is_match(line) {
                first_heading_idx = Some(i);
                break;
            }
        }
        let cursor = match first_heading_idx {
            None => {
                // 全文件没有标题
                if !lines.is_empty() {
                    sections.push((0, "__preamble__".into(), 0));
                }
                lines.len()
            }
            Some(i) => {
                if i > 0 {
                    sections.push((0, "__preamble__".into(), 0));
                }
                // 从第一个标题行开始处理
                let mut idx = i;
                while idx < lines.len() {
                    let line = lines[idx];
                    if let Some(caps) = HEADING_RE.captures(line) {
                        let title = caps.get(2).unwrap().as_str().to_string();
                        sections.push((idx, title, idx + 1));
                        idx += 1;
                    } else {
                        idx += 1;
                    }
                }
                lines.len()
            }
        };
        // 防止未使用警告
        let _ = cursor;

        // Step 2: 对每个 section,收集正文行 + 查 memory-unit 注释
        let mut chunks = Vec::new();
        for sec_i in 0..sections.len() {
            let (sec_start_line, ref sec_title, _sec_body_start) = sections[sec_i];
            let sec_title = sec_title.clone();
            let sec_end_line = if sec_i + 1 < sections.len() {
                sections[sec_i + 1].0 - 1
            } else {
                lines.len() - 1
            };

            // 收集 section 正文 (含 inline ID 注释)
            let mut body_lines: Vec<(usize, &str)> = Vec::new(); // (line_idx, text)
            for (i, line) in lines.iter().enumerate() {
                if i > sec_end_line {
                    break;
                }
                // 跳过 preamble 的标题行,或跳过当前 section 的 标题行
                if i == sec_start_line && sec_title.as_str() != "__preamble__" {
                    continue;
                }
                if i < sec_start_line {
                    continue;
                }
                body_lines.push((i, line));
            }

            // 在 body_lines 中查找 memory-unit 注释的显式 chunk 边界。
            // 策略: 如果某行包含 `<!-- memory-unit: ... -->`,它标记下一个 chunk 的 ID。
            // 无显式标记时整个 section 作为单个 chunk (过长则分段)。
            let mut explicit_chunks: Vec<(Option<String>, Option<MemoryKind>, usize, usize)> =
                Vec::new();
            // (id, kind, start_line_idx_in_body, end_line_idx_in_body)
            // 先找所有 marker 位置
            let mut markers: Vec<(usize, String, Option<MemoryKind>)> = Vec::new();
            for (offset, (_orig_idx, line)) in body_lines.iter().enumerate() {
                if let Some(caps) = MEM_UNIT_RE.captures(line) {
                    let json_str = caps.get(1).unwrap().as_str();
                    if let Ok(marker) = serde_json::from_str::<MemoryUnitMarker>(json_str) {
                        let kind = marker
                            .kind
                            .as_deref()
                            .and_then(MemoryKind::from_str);
                        markers.push((offset, marker.id, kind));
                    }
                }
            }

            if markers.is_empty() {
                // 无显式标记: 整 section 作为一个 chunk,再按长度分段
                if !body_lines.is_empty() {
                    let start = 0;
                    let end = body_lines.len() - 1;
                    split_large_chunk(
                        &body_lines, &sec_title,
                        start, end, MAX_CHUNK_CHARS,
                        &mut explicit_chunks,
                    );
                }
            } else {
                // 以 marker 为边界切 chunk
                for mi in 0..markers.len() {
                    let (marker_offset, id, kind) = &markers[mi];
                    // marker 所在行之后的内容,直到下一个 marker 之前 (或 section 结束)
                    let start_body = *marker_offset + 1; // 跳过 marker 行
                    let end_body = if mi + 1 < markers.len() {
                        markers[mi + 1].0 - 1
                    } else {
                        body_lines.len() - 1
                    };
                    if start_body <= end_body && !body_lines[start_body..=end_body].is_empty() {
                        split_large_chunk_with_id(
                            &body_lines,
                            start_body, end_body,
                            MAX_CHUNK_CHARS,
                            Some(id.clone()),
                            Some((*kind).unwrap_or(MemoryKind::Fact)),
                            &mut explicit_chunks,
                        );
                    }
                }
            }

            // 现在把 explicit_chunks 映射到原始行号, 构造 ParsedMemoryChunk
            let section_name = if sec_title.as_str() == "__preamble__" {
                String::new()
            } else {
                sec_title.clone()
            };
            let derived_kind = infer_kind_from_section(&sec_title);

            for (id_opt, kind_opt, start_body_off, end_body_off) in explicit_chunks {
                let orig_start = body_lines[start_body_off].0 + 1; // 1-based
                let orig_end = body_lines[end_body_off].0 + 1;
                let text: String = body_lines[start_body_off..=end_body_off]
                    .iter()
                    .map(|(_, l)| *l)
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let kind = kind_opt.or(derived_kind).unwrap_or(MemoryKind::Fact);
                chunks.push(ParsedMemoryChunk {
                    explicit_id: id_opt,
                    explicit_kind: Some(kind),
                    section: section_name.clone(),
                    content: text,
                    start_line: orig_start,
                    end_line: orig_end,
                });
            }
        }

        Self {
            path: path.into(),
            chunks,
            rewritten_content: None,
        }
    }

    /// 为缺失显式 ID 的 chunks 生成稳定 ID,并返回带注释写回的新内容。
    ///
    /// 如果有任何 chunk 需要写 ID,返回 Some(rewritten_markdown)。
    /// 否则返回 None。
    pub fn with_stable_ids(mut self) -> Self {
        // 判断是否有 chunk 需要补 ID。
        let any_needs_id = self.chunks.iter().any(|c| c.explicit_id.is_none());
        if !any_needs_id {
            return self;
        }

        // 为了写回注释,我们需要读取原文件行,在 chunk 起始位置插入注释。
        let file_content = match std::fs::read_to_string(&self.path) {
            Ok(c) => c,
            Err(_) => return self, // 读不到就放弃回写,仅生成内存 ID
        };
        let mut lines: Vec<String> = file_content.lines().map(|l| l.to_string()).collect();

        // 从下往上插入,避免行号偏移
        let mut chunks_sorted = self.chunks.clone();
        chunks_sorted.sort_by_key(|c| std::cmp::Reverse(c.start_line));

        for chunk in &mut chunks_sorted {
            if chunk.explicit_id.is_none() {
                // 生成稳定 ID: path + section + content
                let to_hash = format!("{}:{}:{}", self.path, chunk.section, chunk.content);
                let mut hasher = Sha256::new();
                hasher.update(to_hash.as_bytes());
                let hash_full = format!("{:x}", hasher.finalize());
                let id = format!("mem_{}", &hash_full[..10]);
                chunk.explicit_id = Some(id.clone());

                // 在 start_line - 1 (0-based) 之前插入注释行
                let kind_str = chunk
                    .explicit_kind
                    .map(|k| format!(r#","kind":"{}""#, k.as_str()))
                    .unwrap_or_default();
                let comment = format!(
                    r#"<!-- memory-unit: {{"id":"{}"{}}} -->"#,
                    id, kind_str
                );
                let insert_at = chunk.start_line - 1; // 0-based
                if insert_at <= lines.len() {
                    lines.insert(insert_at, comment);
                }
            }
        }

        // 修正 self.chunks 中的 ID (注意 chunks_sorted 和 self.chunks 是副本,
        // 我们按原始顺序用 section+start_line 对齐匹配)
        // 更简单的办法: 重新 parse 一遍 (带注释后不会再有缺 ID 的 chunk)
        let rewritten = lines.join("\n");
        let reparsed = Self::parse(&self.path, &rewritten);
        self.chunks = reparsed.chunks;
        self.rewritten_content = Some(rewritten);
        self
    }

    /// 将 chunks 转换为 MemoryUnits 列表, 给缺少 ID 的分配 mem_xxx。
    pub fn into_memory_units(
        self,
        scope: MemoryScope,
    ) -> Vec<MemoryUnit> {
        let now = Utc::now();
        self.chunks
            .into_iter()
            .map(|chunk| {
                let id = chunk.explicit_id.unwrap_or_else(|| {
                    let to_hash = format!("{}:{}:{}", self.path, chunk.section, chunk.content);
                    let mut hasher = Sha256::new();
                    hasher.update(to_hash.as_bytes());
                    let hash_full = format!("{:x}", hasher.finalize());
                    format!("mem_{}", &hash_full[..10])
                });
                let content_hash = {
                    let mut hasher = Sha256::new();
                    hasher.update(chunk.content.as_bytes());
                    format!("{:x}", hasher.finalize())
                };
                let kind = chunk.explicit_kind.unwrap_or(MemoryKind::Fact);
                MemoryUnit {
                    id,
                    path: self.path.clone(),
                    section: chunk.section,
                    kind,
                    scope,
                    status: UnitStatus::Active,
                    content: chunk.content,
                    content_hash,
                    updated_at: now,
                    created_at: now,
                }
            })
            .collect()
    }
}

// ─────────────── 辅助函数 ───────────────

/// 把大 chunk 按段落切成若干小块, 每块 ≤ max_chars。
fn split_large_chunk(
    body_lines: &[(usize, &str)],
    _sec_title: &str,
    start: usize,
    end: usize,
    max_chars: usize,
    out: &mut Vec<(Option<String>, Option<MemoryKind>, usize, usize)>,
) {
    split_large_chunk_with_id(body_lines, start, end, max_chars, None, None, out);
}

fn split_large_chunk_with_id(
    body_lines: &[(usize, &str)],
    start: usize,
    end: usize,
    max_chars: usize,
    id: Option<String>,
    kind: Option<MemoryKind>,
    out: &mut Vec<(Option<String>, Option<MemoryKind>, usize, usize)>,
) {
    // 以空行为段落分隔,累积到接近 max_chars 就切片
    // 若没有空行或单行过长,按行硬切
    let mut seg_start = start;
    let mut seg_chars = 0usize;

    for i in start..=end {
        let line = body_lines[i].1;
        let is_blank = line.trim().is_empty();
        seg_chars += line.len() + 1;

        if is_blank && seg_chars > max_chars.saturating_sub(200) {
            // 空行 + 已接近阈值 → 切
            out.push((id.clone(), kind, seg_start, i.saturating_sub(1)));
            seg_start = i + 1;
            seg_chars = 0;
        } else if seg_chars >= max_chars && !is_blank {
            // 非空行但超过阈值 → 在当前行切
            out.push((id.clone(), kind, seg_start, i));
            seg_start = i + 1;
            seg_chars = 0;
        }
    }
    if seg_start <= end {
        out.push((id, kind, seg_start, end));
    }
}

/// 从 section 标题启发式推断 MemoryKind。
fn infer_kind_from_section(title: &str) -> Option<MemoryKind> {
    let low = title.to_ascii_lowercase();
    if low.contains("preference") || low.contains("偏好") || low.contains("workflow") {
        Some(MemoryKind::Preference)
    } else if low.contains("decision") || low.contains("决策")
        || low.contains("design") || low.contains("设计")
    {
        Some(MemoryKind::Decision)
    } else if low.contains("constraint") || low.contains("约束")
        || low.contains("hard constraint") || low.contains("限制")
    {
        Some(MemoryKind::Constraint)
    } else if low.contains("solution") || low.contains("解决方案")
        || low.contains("fix") || low.contains("修复") || low.contains("修复方案")
    {
        Some(MemoryKind::Solution)
    } else if low.contains("fact") || low.contains("事实") || low.contains("overview")
        || low.contains("架构") || low.contains("architecture") || low.contains("preamble")
    {
        Some(MemoryKind::Fact)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_simple_headings() {
        let content = r#"
# Project Overview

This project uses Rust and Tokio.

## Hard Constraints

- No unsafe code allowed
- Fail-closed for security
"#;
        let parsed = ParsedMemoryFile::parse("/tmp/test.md", content);
        assert_eq!(parsed.chunks.len(), 2);
        // section 1: Project Overview
        assert!(parsed.chunks[0].section.contains("Project Overview"));
        assert!(parsed.chunks[0].content.contains("Rust"));
        // section 2: Hard Constraints
        assert!(parsed.chunks[1].section.contains("Hard Constraints"));
        assert!(parsed.chunks[1].content.contains("No unsafe"));
        assert_eq!(parsed.chunks[1].explicit_kind, Some(MemoryKind::Constraint));
    }

    #[test]
    fn parse_explicit_memory_unit_marker() {
        let content = r#"
## Decisions

<!-- memory-unit: {"id":"mem_draft","kind":"decision"} -->

We decided to use SQLite for the memory index.
Reasons: WAL support, FTS5 built-in.
"#;
        let parsed = ParsedMemoryFile::parse("/tmp/test.md", content);
        assert_eq!(parsed.chunks.len(), 1);
        assert_eq!(parsed.chunks[0].explicit_id.as_deref(), Some("mem_draft"));
        assert_eq!(parsed.chunks[0].explicit_kind, Some(MemoryKind::Decision));
        assert!(parsed.chunks[0].content.contains("SQLite"));
    }

    #[test]
    fn generate_stable_ids_writes_back() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("notes.md");
        let orig = r#"# Overview
Rust project with grodex.
"#;
        std::fs::write(&path, orig).unwrap();
        let parsed = ParsedMemoryFile::parse(&path.to_string_lossy(), orig);
        assert!(parsed.chunks[0].explicit_id.is_none());

        let with_ids = parsed.with_stable_ids();
        assert!(with_ids.chunks[0].explicit_id.is_some());
        assert!(with_ids.rewritten_content.is_some());
        let rewritten = with_ids.rewritten_content.unwrap();
        assert!(rewritten.contains("memory-unit"));
        assert!(rewritten.contains("mem_"));
    }

    #[test]
    fn kind_inference() {
        assert_eq!(
            infer_kind_from_section("## Hard Constraints"),
            Some(MemoryKind::Constraint)
        );
        assert_eq!(
            infer_kind_from_section("### 架构决策"),
            Some(MemoryKind::Decision)
        );
        assert_eq!(
            infer_kind_from_section("# User Preferences"),
            Some(MemoryKind::Preference)
        );
        assert_eq!(
            infer_kind_from_section("## 修复方案: openssl missing"),
            Some(MemoryKind::Solution)
        );
        assert_eq!(
            infer_kind_from_section("## 随便一个标题"),
            None
        );
    }

    #[test]
    fn into_memory_units_hashes() {
        let content = "## Facts\n\nRust builds take time.";
        let parsed = ParsedMemoryFile::parse("/repo/MEMORY.md", content)
            .with_stable_ids();
        let units = parsed.into_memory_units(MemoryScope::Workspace);
        assert_eq!(units.len(), 1);
        assert!(units[0].id.starts_with("mem_"));
        assert!(!units[0].content_hash.is_empty());
        assert_eq!(units[0].kind, MemoryKind::Fact);
    }
}
