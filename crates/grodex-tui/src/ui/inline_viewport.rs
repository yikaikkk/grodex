//! Word-wrap helpers for scrollback history lines.
//!
//! 本模块原本承载完整的 inline viewport 实现（InlineBackend + 手写
//! DECSTBM scroll region 插入,commit 5350e09）。现该职责已由对齐 codex
//! 的两个模块接管：
//!
//! * `crate::custom_terminal` —— inline viewport Terminal（手动管理
//!   viewport_area）
//! * `crate::insert_history` —— scroll region 历史推入（codex Standard 模式）
//!
//! 这里只保留 scrollback 折行辅助函数,供 `insert_history` 预折行使用。

/// Simple word-wrap for a single line. Returns wrapped sub-lines.
pub fn wrap_line_public(text: &str, width: usize) -> Vec<String> {
    wrap_line(text, width)
}

/// Simple word-wrap for a single line. Returns wrapped sub-lines.
fn wrap_line(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            if current.is_empty() {
                current = word.to_string();
            } else if current.len() + 1 + word.len() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                result.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            result.push(current);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}
