//! Render pipeline for the Grok-style single-column layout.
//!
//! Render order (top → bottom) deliberately mirrors
//! `grok/pager/src/views/agent.rs` so the eye reads in a natural order and
//! no "split screen" chrome ever creates the squished two-panel look the
//! user flagged as "all mashed together."
//!
//! ```text
//! ┌ status_bar ─────────────────────────────────────────────────┐
//! │ ● Grodex  provider/model …   G=n  session=xxxx              │
//! ├ approvals (hidden if empty) ────────────────────────────────┤
//! │ ⚠ #0 tool_name() [HIGH] ~30s                                │
//! │   summary line 1                                             │
//! ├ conversation (scrollable, MIN(5)) ──────────────────────────┤
//! │  You  first line of user prompt                              │
//! │       second line (wrapped)                                  │
//! │                                                              │
//! │  Grodex  streaming…                                          │
//! │       reply text…                                            │
//! ├ turn_status (1 row; hidden if idle) ────────────────────────┤
//! │  ⏳ streaming · 2 tools active …                             │
//! ├ prompt (chrome-bordered Grok PromptWidget) ─────────────────┤
//! │ ╭─ session ───────────────────────────────────────────────╮ │
//! │ │ ❯ user input here                                       │ │
//! │ │   wrapped continuation line                             │ │
//! │ ╰─ model · trusted · multiline ───────────────────────────╯ │
//! ├ shortcuts (1 row hint bar) ─────────────────────────────────┤
//! │ [PROMPT]  Enter send  Alt↵ newline  Esc cancel  ←→ move    │
//! └──────────────────────────────────────────────────────────────┘
//! ```

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders as B, Paragraph, Wrap, List, ListItem};
use ratatui::Frame;

use super::layout::AppLayout;
use super::state::{BUILTIN_SLASH_COMMANDS, ChatMessage, InputMode, TuiAppState};

// ── Palette ───────────────────────────────────────────────────────────
// 使用终端命名颜色而非硬编码 RGB，让终端自动适配深色/浅色主题。
// Color::Reset = 终端默认前景/背景色，Color::Gray/Cyan 等由终端配色方案
// 决定具体色调，在深色终端上偏亮、浅色终端上偏暗，两套主题都可读。

fn c_bg() -> Color { Color::Reset }
#[allow(dead_code)]
fn c_bg_elevated() -> Color { Color::Reset }
#[allow(dead_code)]
fn c_bg_panel() -> Color { Color::Reset }
/// User 消息的淡灰背景，用来与 LLM（Assistant）输出做视觉区分。
/// Color::DarkGray 由终端配色方案决定（256 色终端的 #8 色），在深色终端
/// 上偏亮灰、浅色终端上偏暗灰，两套主题都能形成柔和的对比（不刺眼、
/// 不强制具体色相）。与 "You / Grodex" 文字标签相比，纯背景色区分更接近
/// Claude Code / Codex 的无标签聊天风格。
fn c_bg_user() -> Color { Color::DarkGray }

fn c_fg()        -> Style { Style::default().fg(Color::Reset) }
fn c_dim()       -> Style { Style::default().fg(Color::DarkGray) }
fn c_muted()     -> Style { Style::default().fg(Color::Gray) }

fn c_accent()    -> Style { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD) }
#[allow(dead_code)]
fn c_user()      -> Style { Style::default().fg(Color::Green).add_modifier(Modifier::BOLD) }
#[allow(dead_code)]
fn c_assistant() -> Style { Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD) }
fn c_prefix()    -> Style { Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD) } // ❯
fn c_tool_ok()   -> Style { Style::default().fg(Color::Green) }
fn c_tool_err()  -> Style { Style::default().fg(Color::Red).add_modifier(Modifier::BOLD) }
fn c_tool_name() -> Style { Style::default().fg(Color::Yellow) }
fn c_tool_args() -> Style { Style::default().fg(Color::Cyan) }
fn c_tool_out()  -> Style { Style::default().fg(Color::Gray) }
fn c_tool_run()  -> Style { Style::default().fg(Color::Yellow) }
/// Rail color shared by Thinking rail and Tool rail.
fn c_tool_rail() -> Color { Color::Yellow }
/// Thinking / reasoning panel.
fn c_thinking_label() -> Style { Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD) }
fn c_thinking_text()  -> Style { Style::default().fg(Color::Gray) }
fn c_thinking_rail()  -> Color { Color::Magenta }
fn c_error()     -> Style { Style::default().fg(Color::Red) }
fn c_error_bg()  -> Style { Style::default().fg(Color::Red).bg(Color::Black).add_modifier(Modifier::BOLD) }
fn c_warn()      -> Style { Style::default().fg(Color::Yellow) }
fn c_approval_bg_sel() -> Style {
    Style::default().fg(Color::Reset).bg(Color::Blue).add_modifier(Modifier::BOLD)
}

fn c_kbd()       -> Style { Style::default()
    .fg(Color::Reset)
    .bg(Color::DarkGray)
    .add_modifier(Modifier::BOLD) }
fn c_border_active() -> Color { Color::Cyan }
fn c_border_idle()   -> Color { Color::DarkGray }
fn c_footer_top()    -> Color { Color::DarkGray }

fn style_s(style: Style, s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), style)
}

/// Drain at most `n` items from the front of a VecDeque into a Vec.
/// Used by render_conversation to spread pending tool badges evenly
/// across assistant paragraphs without borrow conflicts from a closure.
#[inline]
fn flush_n<T>(
    queue: &mut std::collections::VecDeque<T>,
    n: usize,
    out: &mut Vec<T>,
) -> usize {
    let n = n.min(queue.len());
    for _ in 0..n {
        if let Some(b) = queue.pop_front() {
            out.push(b);
        }
    }
    n
}

/// Word-wrap `text` to `width` columns, returning the wrapped lines.
/// Preserves real `\n` paragraphs and only breaks on whitespace/punctuation
/// when a single word would otherwise exceed the width.
fn wrap_str(text: &str, width: usize) -> Vec<String> {
    if width == 0 { return vec![text.to_string()]; }
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        let mut col = 0usize;
        for ch in paragraph.chars() {
            if col >= width && (ch.is_whitespace() || ch.is_ascii_punctuation()) {
                lines.push(std::mem::take(&mut current));
                col = 0;
                if ch.is_whitespace() { continue; }
            }
            current.push(ch);
            col += 1;
        }
        lines.push(current);
    }
    lines
}

// ────────────────────────────────────────────────────────────────────────
// Lightweight Markdown → Vec<Line> renderer (zero new dependencies).
//
// Handles the subset Grok-style agents actually emit:
//   * Fenced code blocks (```lang ... ```)  →  muted bg, indent, mono-ish
//   * ATX headings (# .. ######)            →  bold + larger-than-body hue
//   * Block quotes (>)                       →  quote rail + italic hue
//   * Bullet / numbered lists (- + *  / 1.)  →  indented with glyph col
//   * Thematic break (--- /***)              →  dashed line
//   * Inline emphasis: **bold** / *italic* / __bold__ / _italic_
//   * Inline code: `code`                    →  code color, distinct bg
//   * Bare links [text](url)                 →  blue-underlined text
//
// Anything not recognized falls through as plain body text. The renderer
// intentionally never panics — malformed MD just renders without styling.
// ────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
enum BlockKind {
    Paragraph,
    Heading(u8),        // 1..=6
    Quote,
    ListItem { ordered: bool, marker: &'static str, depth: u8 },
    Fence { lang: String },
    Rule,
}

/// Markdown block — name kept deliberately different from ratatui's
/// `Block` widget so imports never shadow each other.
#[derive(Clone)]
struct MdBlock {
    kind: BlockKind,
    text: String,
}

fn md_parse_blocks(raw: &str) -> Vec<MdBlock> {
    let mut blocks: Vec<MdBlock> = Vec::new();
    let mut lines: Vec<&str> = raw.lines().collect();
    if lines.last().map(|l| !l.is_empty()).unwrap_or(false) {
        lines.push("");
    }
    let mut i = 0;
    let mut para_lines: Vec<String> = Vec::new();

    fn list_prefix<'a>(line: &'a str) -> Option<(bool, &'static str, u8, &'a str)> {
        // The `'a` lifetime ties the returned slice to `line`, matching
        // what we actually return (`&trimmed[2..]` where `trimmed` is a
        // borrow of `line`). Closures can't declare explicit lifetimes,
        // so this is lifted to a free function that can.
        let trimmed = line.trim_start();
        let indent_chars = line.len() - trimmed.len();
        let depth = (indent_chars as u8) / 2;
        let bytes = trimmed.as_bytes();
        if bytes.is_empty() { return None; }
        match bytes[0] {
            b'-' | b'+' | b'*'
                if bytes.len() >= 2 && (bytes[1] == b' ' || bytes[1] == b'\t') =>
            {
                let marker = match bytes[0] {
                    b'-' => "• ",
                    b'+' => "• ",
                    _   => "• ",
                };
                Some((false, marker, depth, &trimmed[2..]))
            }
            b'0'..=b'9' => {
                let mut end = 0usize;
                while end < bytes.len() && bytes[end].is_ascii_digit() { end += 1; }
                if end + 1 < bytes.len() && bytes[end] == b'.'
                    && (bytes[end + 1] == b' ' || bytes[end + 1] == b'\t')
                {
                    Some((true, "1. ", depth, &trimmed[end + 2..]))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    let flush_para = |blocks: &mut Vec<MdBlock>, para: &mut Vec<String>| {
        if para.is_empty() { return; }
        let text = para.join("\n");
        blocks.push(MdBlock { kind: BlockKind::Paragraph, text });
        para.clear();
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with("```") {
            flush_para(&mut blocks, &mut para_lines);
            let lang: String = trimmed.trim_start_matches('`').trim().to_string();
            i += 1;
            let mut body = String::new();
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            if i < lines.len() { i += 1; }
            if body.ends_with('\n') { body.pop(); }
            blocks.push(MdBlock { kind: BlockKind::Fence { lang }, text: body });
            continue;
        }

        if trimmed.is_empty() {
            flush_para(&mut blocks, &mut para_lines);
            i += 1;
            continue;
        }

        if (trimmed.chars().all(|c| c == '-' || c == ' ')
            && trimmed.chars().filter(|c| *c == '-').count() >= 3)
            || (trimmed.chars().all(|c| c == '*' || c == ' ')
                && trimmed.chars().filter(|c| *c == '*').count() >= 3)
        {
            flush_para(&mut blocks, &mut para_lines);
            blocks.push(MdBlock { kind: BlockKind::Rule, text: String::new() });
            i += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix('#') {
            let mut level = 1u8;
            let mut t = rest;
            while let Some(r) = t.strip_prefix('#') {
                level += 1;
                t = r;
            }
            if level <= 6 && t.starts_with(' ') {
                flush_para(&mut blocks, &mut para_lines);
                let heading = t.trim().trim_end_matches('#').trim().to_string();
                blocks.push(MdBlock { kind: BlockKind::Heading(level), text: heading });
                i += 1;
                continue;
            }
        }

        if let Some(body) = trimmed.strip_prefix("> ") {
            flush_para(&mut blocks, &mut para_lines);
            let mut content = body.to_string();
            i += 1;
            while i < lines.len() {
                let n = lines[i];
                if let Some(b) = n.trim().strip_prefix("> ") {
                    content.push('\n');
                    content.push_str(b);
                    i += 1;
                } else if n.trim() == ">" {
                    content.push('\n');
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(MdBlock { kind: BlockKind::Quote, text: content });
            continue;
        }

        if let Some((ordered, marker, depth, content)) = list_prefix(line) {
            flush_para(&mut blocks, &mut para_lines);
            blocks.push(MdBlock {
                kind: BlockKind::ListItem { ordered, marker, depth },
                text: content.to_string(),
            });
            i += 1;
            continue;
        }

        para_lines.push(line.to_string());
        i += 1;
    }

    flush_para(&mut blocks, &mut para_lines);
    blocks
}

/// Split line-width for a paragraph, taking a block-level prefix into
/// account. E.g. headings / quotes / list markers own a left column — the
/// text wraps into the remaining width and the continuation column is
/// padded to match the marker width.
struct WrappedPara {
    first_prefix: String,
    cont_prefix: String,
    chunks: Vec<String>,
}

fn wrap_paragraph(text: &str, block_prefix_w: usize, width: usize) -> WrappedPara {
    let total_w = width.max(block_prefix_w + 4);
    let wrap_w = total_w.saturating_sub(block_prefix_w).max(1);
    let chunks = wrap_str_display_width_inner(text, wrap_w);
    let cont_prefix = " ".repeat(block_prefix_w);
    WrappedPara { first_prefix: cont_prefix.clone(), cont_prefix, chunks }
}

fn wrap_str_display_width_inner(text: &str, width: usize) -> Vec<String> {
    // Copy of wrap_str_display_width but operates on paragraphs that may
    // still contain internal line breaks. We first split on hard newlines
    // then word-wrap each piece.
    let mut out = Vec::new();
    for para in text.split('\n') {
        let w = width.max(1);
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for c in para.chars() {
            let cw = display_width(c);
            if cur_w + cw > w && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(c);
            cur_w += cw;
            if cur_w > w {
                out.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
        }
        out.push(cur);
    }
    out
}

/// Render inline-markdown spans: **bold** / *italic* / `code` / [text](url).
/// Operates on a single wrapped line; returns the styled Spans for it.
fn render_inline<'a>(text: &str, base: Style) -> Vec<Span<'a>> {
    let mut out: Vec<Span<'a>> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let mut buf = String::new();
    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                out.push(Span::styled(std::mem::take(&mut buf), base));
            }
        };
    }
    while i < chars.len() {
        let c = chars[i];
        // Inline code: `code`
        if c == '`' {
            flush!();
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end] != '`' { end += 1; }
            if end < chars.len() {
                let s: String = chars[start..end].iter().collect();
                out.push(Span::styled(
                    s,
                    c_code_inline().bg(Color::DarkGray),
                ));
                i = end + 1;
                continue;
            }
        }
        // Link: [text](url)
        if c == '[' {
            // peek for `](`
            let mut j = i + 1;
            while j < chars.len() && chars[j] != ']' { j += 1; }
            if j + 2 < chars.len() && chars[j] == ']' && chars[j + 1] == '(' {
                let k_start = j + 2;
                let mut k = k_start;
                while k < chars.len() && chars[k] != ')' { k += 1; }
                if k < chars.len() {
                    flush!();
                    let txt: String = chars[i + 1..j].iter().collect();
                    out.push(Span::styled(
                        txt,
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::UNDERLINED),
                    ));
                    i = k + 1;
                    continue;
                }
            }
        }
        // Bold / italic. Try **...** / __...__ first, then *...* / _..._.
        let is_bold_delim = || -> Option<(usize, usize)> {
            if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
                let start = i + 2;
                let mut j = start;
                while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '*') {
                    j += 1;
                }
                if j + 1 < chars.len() { return Some((start, j)); }
            }
            if c == '_' && i + 1 < chars.len() && chars[i + 1] == '_' {
                let start = i + 2;
                let mut j = start;
                while j + 1 < chars.len() && !(chars[j] == '_' && chars[j + 1] == '_') {
                    j += 1;
                }
                if j + 1 < chars.len() { return Some((start, j)); }
            }
            None
        };
        if let Some((s, e)) = is_bold_delim() {
            flush!();
            let txt: String = chars[s..e].iter().collect();
            out.push(Span::styled(txt, base.add_modifier(Modifier::BOLD)));
            i = e + 2;
            continue;
        }
        let is_italic_delim = || -> Option<(usize, usize)> {
            if c == '*' {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j] != '*' { j += 1; }
                if j < chars.len() { return Some((start, j)); }
            }
            if c == '_' {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j] != '_' { j += 1; }
                if j < chars.len() { return Some((start, j)); }
            }
            None
        };
        if let Some((s, e)) = is_italic_delim() {
            flush!();
            let txt: String = chars[s..e].iter().collect();
            out.push(Span::styled(txt, base.add_modifier(Modifier::ITALIC)));
            i = e + 1;
            continue;
        }

        buf.push(c);
        i += 1;
    }
    flush!();
    out
}

/// Render Markdown `text` as a list of Lines, using the given `body_style`
/// as a fallback for plain text. Headings/lists/quotes/fences get their
/// own styling layers on top.
fn md_to_lines<'a>(text: &str, width: usize, body_style: Style) -> Vec<Line<'a>> {
    let mut out: Vec<Line<'a>> = Vec::new();
    let blocks = md_parse_blocks(text);
    for blk in blocks {
        match blk.kind {
            BlockKind::Rule => {
                let w = width.max(20);
                out.push(Line::from(Span::styled(
                    "─".repeat(w),
                    body_style.add_modifier(Modifier::DIM),
                )));
                continue;
            }
            BlockKind::Fence { lang: _ } => {
                // Fenced code: indented 6 + rail + code hue.
                // Indent is fixed; if width is very small, we still try.
                let indent_w = 6usize.min(width);
                let indent = " ".repeat(indent_w);
                let rail = Span::styled("│", c_code_block());
                let fence_w = width.saturating_sub(indent_w + 2).max(8);
                if blk.text.is_empty() {
                    out.push(Line::from(vec![
                        Span::raw(indent.clone()),
                        rail.clone(),
                        Span::raw(" "),
                    ]));
                } else {
                    for raw_line in blk.text.split('\n') {
                        let wrapped = wrap_str_display_width_inner(raw_line, fence_w);
                        for wl in wrapped {
                            out.push(Line::from(vec![
                                Span::raw(indent.clone()),
                                rail.clone(),
                                Span::raw(" "),
                                Span::styled(wl, c_code_block()),
                            ]));
                        }
                    }
                }
                continue;
            }
            BlockKind::Heading(level) => {
                // c_user() / c_assistant() already carry BOLD + a base
                // fg color, so we patch them directly (no need to chain
                // through Style::fg which expects Color, not Style).
                let (prefix, style) = match level {
                    1 => ("# ", c_user()),
                    2 => ("## ", c_user()),
                    3 => ("### ", c_assistant()),
                    4 => ("#### ", Style::default().add_modifier(Modifier::BOLD).fg(Color::Reset)),
                    5 => ("##### ", Style::default().add_modifier(Modifier::BOLD).fg(Color::Gray)),
                    6 => ("###### ", Style::default().add_modifier(Modifier::DIM | Modifier::BOLD).fg(Color::Gray)),
                    _ => ("", Style::default()),
                };
                let indent = "      ";
                let pw = display_width_str(prefix);
                let wrap_w = width.saturating_sub(6).max(8);
                let lines = wrap_str_display_width_inner(&blk.text, wrap_w.saturating_sub(pw).max(4));
                let cont_pad = " ".repeat(pw);
                for (idx, l) in lines.iter().enumerate() {
                    let p = if idx == 0 { prefix } else { &cont_pad };
                    let mut spans = vec![Span::raw(indent.to_string())];
                    spans.push(Span::styled(p.to_string(), style));
                    spans.extend(render_inline(l, style));
                    out.push(Line::from(spans));
                }
                continue;
            }
            BlockKind::Quote => {
                let indent = "      ";
                let rail = "▍ ";
                let rail_style = Style::default().fg(Color::Cyan);
                let wrap_w = width.saturating_sub(6 + 2).max(8);
                let lines = wrap_str_display_width_inner(&blk.text, wrap_w);
                for l in lines {
                    let mut spans = vec![
                        Span::raw(indent.to_string()),
                        Span::styled(rail.to_string(), rail_style),
                    ];
                    spans.extend(render_inline(&l, body_style.add_modifier(Modifier::ITALIC)));
                    out.push(Line::from(spans));
                }
                continue;
            }
            BlockKind::ListItem { ordered: _, marker, depth } => {
                let indent_level = (depth as usize).min(3);
                let outer = "      ".to_string() + &"  ".repeat(indent_level);
                let marker_span = Span::styled(marker.to_string(), c_dim());
                let marker_w = display_width_str(marker);
                let wrap_w = width
                    .saturating_sub(6 + indent_level * 2 + marker_w)
                    .max(8);
                let lines = wrap_str_display_width_inner(&blk.text, wrap_w);
                let cont_pad = " ".repeat(marker_w);
                for (idx, l) in lines.iter().enumerate() {
                    let mut spans = vec![Span::raw(outer.clone())];
                    if idx == 0 {
                        spans.push(marker_span.clone());
                    } else {
                        spans.push(Span::raw(outer.clone()));
                        spans.pop();
                        spans.push(Span::raw(" ".repeat(marker_w)));
                    }
                    spans.extend(render_inline(l, body_style));
                    out.push(Line::from(spans));
                }
                continue;
            }
            BlockKind::Paragraph => {
                let indent = "      ";
                let wrap_w = width.saturating_sub(6).max(8);
                let lines = wrap_str_display_width_inner(&blk.text, wrap_w);
                for l in lines {
                    let mut spans = vec![Span::raw(indent.to_string())];
                    spans.extend(render_inline(&l, body_style));
                    out.push(Line::from(spans));
                }
                continue;
            }
        }
    }
    out
}

fn c_code_block() -> Style { Style::default().fg(Color::Magenta) }
fn c_code_inline() -> Style { Style::default().fg(Color::Magenta) }

fn display_width_str(s: &str) -> usize {
    s.chars().map(|c| display_width(c)).sum()
}

// ────────────────────────────────────────────────────────────────────────
// Entry point: draws panes in strict top-to-bottom order.
// ────────────────────────────────────────────────────────────────────────

pub fn render_full(f: &mut Frame<'_>, state: &mut TuiAppState, layout: &AppLayout) {
    render_status_bar(f, state, layout.status_bar);
    if layout.approvals.height > 0 {
        render_approvals_pane(f, state, layout.approvals);
    }
    render_conversation(f, state, layout.conversation);
    if layout.turn_status.height > 0 {
        render_turn_status(f, state, layout.turn_status);
    }
    render_prompt_widget(f, state, layout.prompt);
    render_shortcuts_bar(f, state, layout.shortcuts);
}

// ── 1. Status bar (1 row) ───────────────────────────────────────────────

fn render_status_bar(f: &mut Frame<'_>, state: &TuiAppState, area: Rect) {
    if area.is_empty() { return; }
    let provider = if state.provider_label.is_empty() { "—" } else { state.provider_label.as_str() };
    let model = if state.model_label.is_empty() { "—" } else { state.model_label.as_str() };
    let session_short = state.session_id
        .as_deref()
        .map(|s| {
            let n = s.len().min(8);
            s[..n].to_string()
        })
        .unwrap_or_else(|| "—".into());

    let mut spans: Vec<Span> = vec![
        Span::raw("  "),
        style_s(c_accent(), "●"),
        Span::raw(" "),
        style_s(c_accent(), "Grodex"),
        Span::raw("   "),
        style_s(c_dim(), format!("{provider}/{model}")),
    ];

    // Right-aligned context data.
    let gen_s = format!("G={}", state.capability_generation);
    let trust = if state.workspace_trusted { "trusted" } else { "untrusted" };
    let right_part = format!("{gen_s} · {trust} · session {session_short}  ");
    let right_w = right_part.chars().count();
    let left_w: usize = spans.iter().map(|s| s.width()).sum();
    let pad = (area.width as usize).saturating_sub(left_w).saturating_sub(right_w).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(style_s(c_muted(), right_part));

    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, area);
}

// ── 2. Pending approvals pane ───────────────────────────────────────────

fn render_approvals_pane(f: &mut Frame<'_>, state: &TuiAppState, area: Rect) {
    if area.is_empty() { return; }
    let title = Line::from(vec![
        Span::raw(" "),
        style_s(c_warn().add_modifier(Modifier::BOLD), "⚠ Pending approvals"),
    ]);
    let block = Block::default()
        .title(title)
        .borders(B::TOP | B::BOTTOM)
        .border_style(Style::default().fg(c_footer_top()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.is_empty() || inner.height == 0 { return; }

    let max_rows = inner.height as usize;
    let mut rows: Vec<Line> = Vec::with_capacity(max_rows);
    for (i, r) in state.pending_approvals.iter().enumerate() {
        if rows.len() >= max_rows { break; }
        let sel_style = if i == state.selected_approval_idx {
            c_approval_bg_sel()
        } else { Style::default() };
        let headline = Line::from(vec![
            style_s(sel_style, format!("  #{i} ")),
            style_s(c_tool_name().add_modifier(Modifier::BOLD), format!("{}()", r.tool_name)),
            Span::raw("  "),
            style_s(c_error(), format!("[{}]", r.risk)),
            Span::raw("  "),
            style_s(c_dim(), format!("~{}s", r.remaining_s)),
        ]);
        rows.push(headline);
        if rows.len() >= max_rows { break; }
        let sum_w = (inner.width.max(6) as usize).saturating_sub(4);
        for line in wrap_str(&r.summary, sum_w).into_iter().take(1) {
            rows.push(Line::from(vec![
                Span::raw("    "),
                style_s(c_muted(), line),
            ]));
        }
    }
    let items: Vec<ListItem> = rows.into_iter().map(ListItem::new).collect();
    f.render_widget(List::new(items), inner);
}

// ── 3. Conversation (transcript) ────────────────────────────────────────

fn render_conversation(f: &mut Frame<'_>, state: &mut TuiAppState, area: Rect) {
    if area.is_empty() { return; }
    let inner = area; // chrome-less: maximises width, matches grok scrollback feel
    let inner_w = (inner.width as usize).saturating_sub(8);
    let mut rows: Vec<Line> = Vec::new();
    rows.push(Line::from(vec![Span::raw("")]));

    if state.messages.is_empty() {
        rows.push(Line::from(vec![
            Span::raw("  "),
            style_s(c_muted(), "Type a question below — no "),
            style_s(c_kbd(), " i "),
            style_s(c_muted(), " needed.  "),
            style_s(c_accent(), "Try typing `/`"),
            style_s(c_muted(), " to see built-in commands."),
        ]));
        rows.push(Line::from(vec![Span::raw("")]));
        rows.push(Line::from(vec![
            Span::raw("  "),
            style_s(c_muted(), "Suggestions:"),
        ]));
        for idea in [
            "Summarise the project structure and suggest next steps",
            "List the current open TODO items from the task docs",
            "Open `docs/10-tool-skill-mcp-v2-design.md` and summarise the gaps",
            "Run `cargo check --workspace` and fix warnings",
        ] {
            rows.push(Line::from(vec![
                Span::raw("    • "),
                style_s(c_dim(), idea.to_string()),
            ]));
        }
        rows.push(Line::from(vec![Span::raw("")]));
    } else {
        // ── Turn-anchored linear render + turn-foot tool summary ────────
        //
        // Display rules (Claude-style compact tool status):
        //   • Completed tools (has_result=true, done=true, not error) are
        //     NOT rendered one-by-one with their UUID badge — users
        //     reported that wall of "✓ exec(#deadbeef)  ok" made the
        //     history unreadable.
        //   • Instead, at the END of every turn we render a single
        //     summary line: counts of completed OK vs completed ERROR vs
        //     in-flight. The flight details are listed underneath with
        //     elapsed time + 1-line preview, so users see exactly what
        //     the agent is currently doing.
        //   • Error tools render inline immediately so the failure
        //     context is not hidden by aggregation.
        //   • Turn anchoring (User-message slice boundaries) is retained
        //     from the previous round to fix the classic "orphan tool
        //     printed above the next Grodex header" race.

        // Slice turns anchored at User messages
        let turns: Vec<std::ops::Range<usize>> = {
            let mut v: Vec<std::ops::Range<usize>> = Vec::new();
            let mut cur_start: Option<usize> = None;
            for (i, m) in state.messages.iter().enumerate() {
                if matches!(m, ChatMessage::User { .. }) {
                    if let Some(s) = cur_start.take() {
                        v.push(s..i);
                    }
                    cur_start = Some(i);
                }
            }
            match cur_start {
                Some(s) => v.push(s..state.messages.len()),
                None if !state.messages.is_empty() => v.push(0..state.messages.len()),
                None => {}
            }
            // Diagnostic guard — messages is non-empty (so we entered this
            // branch) but no turn ranges were produced. This is logically
            // impossible given the `None if !messages.is_empty()` fallback
            // above, but if a future refactor breaks that clause the user
            // would stare at a fully-blank transcript while `messages.len()`
            // is > 0 — classic "UI empty but snapshot restored" head-scratcher.
            // Fall back to a single whole-range turn AND notify so debugging
            // is instant.
            if v.is_empty() && !state.messages.is_empty() {
                state.push_log(format!(
                    "[render] ⚠ 警告：messages.len()={} 但 turn 切分为空，回退为单 turn 渲染（turn 锚点逻辑可能异常）",
                    state.messages.len()
                ));
                v.push(0..state.messages.len());
            }
            v
        };

        // Best-effort preview of what a tool is "doing right now", shown
        // under the turn-status summary for active tools. We try to pull
        // a human-readable string out of the args JSON so the line is
        // e.g. "$ grep …" for exec or "src/foo.rs" for read_file.
        fn tool_preview(name: &str, args_json: &str) -> String {
            use std::fmt::Write;
            let trimmed = args_json.trim();
            if trimmed.is_empty() {
                return String::new();
            }
            let value: Option<serde_json::Value> = serde_json::from_str(trimmed).ok();
            let v = match value {
                Some(v) => v,
                None => return format!("{}(…)", name),
            };

            // Tool-specific pickers ordered by commonality.
            let mut out = String::new();
            match name {
                "exec" | "shell" | "bash" | "RunCommand" => {
                    let cmd = v.get("command").and_then(|x| x.as_str())
                        .or_else(|| v.get("cmd").and_then(|x| x.as_str()))
                        .unwrap_or("…");
                    let _ = write!(out, "$ {}", truncate(cmd, 120));
                    if let Some(cwd) = v.get("cwd").and_then(|x| x.as_str()) {
                        if !cwd.is_empty() && cwd != "." && cwd != "/" {
                            let _ = write!(out, "  (in {})", truncate(cwd, 40));
                        }
                    }
                }
                "read_file" | "read" | "Read" => {
                    let p = v.get("file_path").and_then(|x| x.as_str())
                        .or_else(|| v.get("path").and_then(|x| x.as_str()))
                        .unwrap_or("…");
                    let mut extras: Vec<&str> = Vec::new();
                    if let Some(o) = v.get("offset").and_then(|x| x.as_u64()) {
                        extras.push("offset");
                        let _ = write!(out, "{} (L{}", truncate(p, 100), o);
                        if let Some(l) = v.get("limit").and_then(|x| x.as_u64()) {
                            let _ = write!(out, "-L{}", o + l);
                        }
                        out.push(')');
                    } else {
                        let _ = write!(out, "{}", truncate(p, 120));
                    }
                    //no-op for lint
                    let _ = extras;
                }
                "write_file" | "write" | "Write" => {
                    let p = v.get("file_path").and_then(|x| x.as_str())
                        .or_else(|| v.get("path").and_then(|x| x.as_str()))
                        .unwrap_or("…");
                    let sz = v.get("content").and_then(|x| x.as_str()).map(|s| s.len())
                        .unwrap_or(0);
                    let _ = write!(out, "{} ({} B)", truncate(p, 120), sz);
                }
                "edit" | "Edit" => {
                    let p = v.get("file_path").and_then(|x| x.as_str())
                        .or_else(|| v.get("path").and_then(|x| x.as_str()))
                        .unwrap_or("…");
                    let tag = if v.get("old_string").is_some() { "patch" } else { "replace" };
                    let _ = write!(out, "{} [{}]", truncate(p, 120), tag);
                }
                "SearchCodebase" | "Grep" | "grep" | "search" => {
                    let q = v.get("information_request").and_then(|x| x.as_str())
                        .or_else(|| v.get("pattern").and_then(|x| x.as_str()))
                        .unwrap_or("…");
                    let _ = write!(out, "? {}", truncate(q, 140));
                }
                "Glob" | "glob" | "LS" | "ls" => {
                    let pat = v.get("pattern").and_then(|x| x.as_str())
                        .or_else(|| v.get("path").and_then(|x| x.as_str()))
                        .unwrap_or("…");
                    let _ = write!(out, "dir/ {}", truncate(pat, 140));
                }
                other => {
                    // Generic: try to surface 2-3 scalar keys that look
                    // meaningful (short string values).
                    let obj = match v.as_object() {
                        Some(o) => o,
                        None => {
                            return format!("{}(…)", other);
                        }
                    };
                    let mut parts: Vec<String> = Vec::with_capacity(3);
                    for key in ["path", "file", "query", "pattern", "url", "command",
                                "name", "target", "scope", "dir", "directory", "id"] {
                        if let Some(val) = obj.get(key).and_then(|x| x.as_str()) {
                            if !val.is_empty() {
                                parts.push(format!("{}={}", key, truncate(val, 60)));
                            }
                        }
                        if parts.len() >= 3 { break; }
                    }
                    if parts.is_empty() {
                        return format!("{}(…)", other);
                    }
                    let _ = write!(out, "{}({})", other, parts.join(", "));
                }
            }
            out
        }

        fn truncate(s: &str, max: usize) -> &str {
            if s.chars().count() <= max { s }
            else {
                let mut end = 0usize;
                for (i, ch) in s.char_indices() {
                    if i >= max { break; }
                    end = i + ch.len_utf8();
                }
                &s[..end]
            }
        }

        // Compact human-friendly duration like "29s" or "3m12s" or "5ms".
        fn human_duration(d: std::time::Duration) -> String {
            let total_ms = d.as_millis();
            if total_ms < 1_000 {
                return format!("{total_ms}ms");
            }
            let secs = total_ms / 1_000;
            if secs < 60 {
                return format!("{}s", secs);
            }
            let m = secs / 60;
            let s = secs % 60;
            if m < 60 {
                return format!("{m}m{s:02}s");
            }
            let h = m / 60;
            let m = m % 60;
            format!("{h}h{m:02}m")
        }

        for turn_range in turns {
            let turn = &state.messages[turn_range];

            let mut assistant_header_rendered = false;
            // Turn-level tool tallies
            let mut completed_ok: Vec<(String, std::time::Duration)> = Vec::new();
            let mut completed_err: Vec<(String, std::time::Duration)> = Vec::new();
            let mut in_flight: Vec<(
                String,           // short title name(#short)
                std::time::Duration,
                String,           // preview string
                bool,             // is_running (else calling/parsing)
            )> = Vec::new();

            for msg in turn {
                match msg {
                    ChatMessage::User { text } => {
                        // User 消息：整行（含文字后的空白列）铺满淡灰背景，
                        // 形成"矩形块"视觉效果，完全对齐 Claude Code / Codex。
                        // 关键：ratatui Span.bg 只覆盖字符宽度，必须在行尾补
                        // 齐一个"剩余宽度的空格 Span"带相同 bg，才能把空白列
                        // 也填上颜色。不再输出 "You" 文字标签。
                        let user_bg = Style::default().bg(c_bg_user());
                        if text.is_empty() {
                            // 空 user 消息占位（·居中），也要全行填色。
                            let pad = " ".repeat(inner_w.max(6).saturating_sub(6));
                            rows.push(Line::from(vec![
                                Span::styled(format!("  · {pad}"), user_bg),
                            ]));
                        } else {
                            let lines = md_to_lines(text, inner_w.max(12), c_fg());
                            for mut line in lines {
                                // 先把现有每个 span 的 fg 合并到 user_bg（保留
                                // 粗体/斜体等 modifier，不丢失 markdown 格式），
                                // 统一背景色 = c_bg_user。
                                for span in line.spans.iter_mut() {
                                    let cur = span.style;
                                    span.style = user_bg
                                        .fg(cur.fg.unwrap_or(Color::Reset))
                                        .add_modifier(cur.add_modifier);
                                }
                                let content_width: usize = line
                                    .spans
                                    .iter()
                                    .map(|s| s.width())
                                    .sum();
                                // 行首 2 列 padding + 内容 + 行尾 fill 空格 = inner_w
                                let leading = 2usize;
                                let trailing = inner_w
                                    .saturating_sub(leading)
                                    .saturating_sub(content_width)
                                    .max(0);
                                let mut padded_spans: Vec<Span> =
                                    Vec::with_capacity(line.spans.len() + 2);
                                padded_spans.push(Span::styled(" ".repeat(leading), user_bg));
                                padded_spans.extend(line.spans);
                                if trailing > 0 {
                                    padded_spans.push(Span::styled(
                                        " ".repeat(trailing),
                                        user_bg,
                                    ));
                                }
                                line.spans = padded_spans;
                                rows.push(line);
                            }
                        }
                        // User 消息块下方空行分隔（无背景，避免上下两条 user 粘
                        // 在一起）。
                        rows.push(Line::from(vec![Span::raw("")]));
                    }

                    ChatMessage::Thinking { text, done } => {
                        let tag = if *done { "Thought" } else { "Thinking…" };
                        rows.push(Line::from(vec![
                            Span::styled("  ╎ ", Style::default().fg(c_thinking_rail())),
                            style_s(c_thinking_label(), tag),
                        ]));
                        if text.is_empty() {
                            let placeholder = if *done { "·" } else { "▋" };
                            rows.push(Line::from(vec![
                                Span::styled("  ╎ ", Style::default().fg(c_thinking_rail())),
                                style_s(c_thinking_text(), placeholder.to_string()),
                            ]));
                        } else {
                            const MAX_CHARS: usize = 1400;
                            const MAX_LINES: usize = 12;
                            let clamped = if text.chars().count() > MAX_CHARS {
                                let mut s: String = text.chars().take(MAX_CHARS).collect();
                                s.push('…');
                                s
                            } else {
                                text.clone()
                            };
                            let body_w = inner_w.saturating_sub(6).max(20);
                            for (i, raw) in clamped.split('\n').take(MAX_LINES).enumerate() {
                                for wl in wrap_str(raw, body_w) {
                                    rows.push(Line::from(vec![
                                        Span::styled("  ╎ ", Style::default().fg(c_thinking_rail())),
                                        style_s(c_thinking_text(), wl),
                                    ]));
                                }
                                if i + 1 == MAX_LINES {
                                    rows.push(Line::from(vec![
                                        Span::styled("  ╎ ", Style::default().fg(c_thinking_rail())),
                                        style_s(c_dim(), "… (truncated — view /export for full CoT)"),
                                    ]));
                                    break;
                                }
                            }
                        }
                        rows.push(Line::from(vec![Span::raw("")]));
                    }

                    ChatMessage::Assistant { text, done } => {
                        // 不再输出 "Grodex" 文字标签，与 User 一致走无标签风格。
                        // Assistant 与 User 通过视觉元素隐式区分：User 有淡灰
                        // 背景，Assistant 使用默认透明背景 + Tool/Thinking 各
                        // 自有彩色 rail（黄/紫 rail 本身就是「模型侧输出」的
                        // 视觉锚点），所以即使不带文字标签也能一目了然。
                        let _ = assistant_header_rendered;
                        assistant_header_rendered = true; // mark so turn-foot tool summary won't add an extra header line
                        if text.is_empty() {
                            let placeholder = if *done { "·" } else { "▋" };
                            rows.push(Line::from(vec![
                                Span::raw("  "),
                                style_s(c_fg(), placeholder.to_string()),
                            ]));
                        } else {
                            // 给正文左侧增加 2 列外边距（与 User 消息的
                            // 「缩进 2 + 正文」整体视觉对齐），使两行文字都从
                            // 同一显示列起手，保持版面的对齐整齐。
                            let mut lines = md_to_lines(text, inner_w.max(12), c_fg());
                            for line in lines.iter_mut() {
                                let mut padded: Vec<Span> = Vec::with_capacity(line.spans.len() + 1);
                                padded.push(Span::raw("  "));
                                padded.extend(line.spans.iter().cloned());
                                line.spans = padded;
                            }
                            rows.extend(lines);
                        }
                        rows.push(Line::from(vec![Span::raw("")]));
                    }

                    ChatMessage::Tool { name, call_id, args, result: _, is_error, done, has_result, started_at, finished_at } => {
                        // For completed tools, freeze the duration at the
                        // moment of completion (finished_at) so the number
                        // doesn't keep ticking on every render frame.
                        // For in-flight tools, keep using started_at.elapsed()
                        // so the live counter advances as expected.
                        let elapsed = match finished_at {
                            Some(end) => end.duration_since(*started_at),
                            None => started_at.elapsed(),
                        };
                        // NOTE: completed tools (both ok and error) are
                        // NO-OPs for inline rendering. Both are reflected
                        // in the single turn-foot summary line as counts,
                        // so the transcript stays clean — exactly Claude's
                        // behaviour where completed tools vanish from the
                        // "currently executing" sublist and only show in
                        // the aggregate "N done / M failed" line.
                        if *has_result && *is_error {
                            completed_err.push((name.clone(), elapsed));
                        } else if *has_result && *done {
                            completed_ok.push((name.clone(), elapsed));
                        } else {
                            // In-flight: calling (no args yet) or running
                            // (has args, agent waiting for result).
                            let is_running = *done || !args.trim().is_empty();
                            let short_id = match call_id.as_ref().filter(|s| !s.is_empty()) {
                                Some(id) => {
                                    let s: String = id.chars().take(8).collect();
                                    format!("{name}(#{s})")
                                }
                                None => format!("{name}()"),
                            };
                            let preview = tool_preview(name, args);
                            in_flight.push((short_id, elapsed, preview, is_running));
                        }
                    }

                    ChatMessage::System { text, is_error } => {
                        if *is_error {
                            rows.push(Line::from(vec![
                                Span::raw("    "),
                                style_s(c_error(), "✗ Error"),
                            ]));
                            for l in wrap_str(text, inner_w.saturating_sub(8)) {
                                rows.push(Line::from(vec![
                                    Span::raw("      "),
                                    style_s(c_error_bg(), format!(" {l} ")),
                                ]));
                            }
                        } else {
                            let prefix = "• ";
                            let cont_pad = "  ";
                            for (idx, raw_line) in text.split('\n').enumerate() {
                                if idx == 0 {
                                    rows.push(Line::from(vec![
                                        Span::raw("    "),
                                        style_s(c_dim().add_modifier(Modifier::BOLD), prefix),
                                        style_s(c_muted(), raw_line.to_string()),
                                    ]));
                                } else {
                                    rows.push(Line::from(vec![
                                        Span::raw("    "),
                                        style_s(c_dim().add_modifier(Modifier::BOLD), cont_pad),
                                        style_s(c_muted(), raw_line.to_string()),
                                    ]));
                                }
                            }
                        }
                        rows.push(Line::from(vec![Span::raw("")]));
                    }
                }
            }

            // ── Turn-foot tool summary (Claude-style) ────────────────
            //
            // Example layout:
            //   ⏺ Working 29s · 17 done · 1 failed · 3 running
            //     ⎿ $ cargo check --workspace (12s)
            //     ⎿ src/main.rs:1080-1095 (4s)
            //
            // If everything finished cleanly → single line with no
            // sub-items. Only IN-FLIGHT tools get the expanded list;
            // completed tools (both ok AND failed) contribute only to
            // the aggregate counters — exactly matching Claude where
            // "currently executing" is a transient-only view.

            let n_ok = completed_ok.len();
            let n_err = completed_err.len();
            let n_flying = in_flight.len();

            if n_ok == 0 && n_err == 0 && n_flying == 0 {
                continue; // turn has no tools, nothing to summarise
            }

            // ── Guarantee Grodex header renders BEFORE tool summary ─
            //
            // The classic "tool summary above Grodex title" race: model
            // can emit ToolCallStart BEFORE the first TextDelta token.
            // Without this guard, turn-foot would fire with
            // `assistant_header_rendered == false`, then the next render
            // frame (when Assistant text arrives) prints "Grodex" BELOW
            // the already-flushed summary. Fix: if the turn has ANY tool
            // activity to report but no header was drawn yet, emit a
            // transient `Grodex  ⏳ working…` header first. This also
            // covers the "tools-only turn" edge case where the model
            // just calls agents without answering.
            if !assistant_header_rendered {
                // Tools-only turn (no Assistant text yet) still needs a
                // visual anchor so the tool summary isn't confused with
                // the user's input. Instead of the word "Grodex" we emit
                // only the "⏳ working…" / "⚙ tools…" indicator, matching
                // the no-label visual style.
                let any_work_left = n_flying > 0;
                if any_work_left {
                    rows.push(Line::from(vec![
                        Span::raw("  "),
                        style_s(c_dim(), "⏳ working…"),
                    ]));
                    rows.push(Line::from(vec![Span::raw("")]));
                }
                assistant_header_rendered = true;
            }

            // ── Summary line pieces ────────────────────────────────
            let mut pieces: Vec<Span> = Vec::with_capacity(8);
            pieces.push(Span::styled("  ", Style::default()));
            pieces.push(match n_flying > 0 {
                true  => style_s(c_thinking_label(), "⏺ "),
                false => style_s(c_tool_ok(), "⏺ "),
            });

            // Optional thinking/working label + total time. For thinking
            // time we pick max(started_at.elapsed) across all in-flight
            // tools as a rough proxy; completed-only turns just say
            // "Tools completed in Xs".
            let now = std::time::Instant::now();
            let span_total = if !in_flight.is_empty() {
                in_flight.iter().map(|t| t.1).max().unwrap_or_default()
            } else if !completed_ok.is_empty() {
                completed_ok.iter().map(|t| t.1).max().unwrap_or_default()
            } else {
                std::time::Duration::ZERO
            };
            let _ = now; // keep Instant::now side effects deterministic (we don't use it today)
            let tag = match n_flying {
                0 if n_err > 0 => "Done (with errors)",
                0 => "Tools",
                _ => "Working",
            };
            pieces.push(style_s(c_thinking_label(), format!("{tag} ")));
            if span_total.as_millis() > 0 {
                pieces.push(style_s(c_dim(), format!("{} · ", human_duration(span_total))));
            } else if n_flying > 0 {
                pieces.push(style_s(c_dim(), "· ".to_string()));
            }

            if n_ok > 0 {
                let total_span = completed_ok.iter().map(|t| t.1.as_millis() as u64).sum::<u64>();
                let sum = std::time::Duration::from_millis(total_span);
                pieces.push(style_s(c_tool_ok(), format!("{n_ok} done")));
                // Only print aggregate span if it's noticeable to avoid
                // 17 tools × <1ms each turning into a wall of "(0ms)"
                if sum.as_millis() >= 200 {
                    pieces.push(style_s(c_dim(), format!("({}) ", human_duration(sum))));
                } else {
                    pieces.push(Span::raw(" "));
                }
            }
            if n_err > 0 {
                pieces.push(style_s(c_tool_err(), format!("{n_err} failed")));
                pieces.push(Span::raw(" "));
            }
            if n_flying > 0 {
                pieces.push(style_s(c_tool_run(), format!("{n_flying} running")));
            }
            // Trim trailing space spans (cosmetic)
            while let Some(last) = pieces.last() {
                if last.content.chars().all(|c| c == ' ') {
                    pieces.pop();
                } else {
                    break;
                }
            }
            rows.push(Line::from(pieces));

            // ── In-flight detail lines ─────────────────────────────
            // Ordered by elapsed (oldest first) so long-running work
            // stays at the top of the list — users can tell what the
            // bottleneck is at a glance.
            in_flight.sort_by(|a, b| b.1.cmp(&a.1));
            for (title, elapsed, preview, _is_running) in in_flight.iter() {
                let mut line_parts: Vec<Span> = Vec::with_capacity(6);
                line_parts.push(Span::styled("    ⎿ ", Style::default().fg(c_tool_rail())));
                line_parts.push(style_s(c_tool_name(), format!("{title} ")));
                if !preview.is_empty() {
                    line_parts.push(style_s(c_fg(), preview.clone()));
                    line_parts.push(Span::raw(" "));
                }
                line_parts.push(style_s(c_dim(), format!("({})", human_duration(*elapsed))));
                rows.push(Line::from(line_parts));
            }
            rows.push(Line::from(vec![Span::raw("")]));
        }
    }
    rows.push(Line::from(vec![Span::raw("")]));

    // Vertical scroll offset clamp + follow_mode 同步。
    //
    // * 计算本帧 max_offset（因内容量/窗口高度会变），把 scroll_conversation
    //   clamp 回合法范围（u16::MAX 哨兵在这里归约）。
    // * 如果 clamp 后 scroll 仍在底部（>= max_offset），且之前用户没显式
    //   向上滚，则重新进入 follow_bottom（对齐 Grok：手动 PageDown 到底后
    //   继续 streaming 自动跟随）。
    let rows_len = rows.len() as u16;
    let visible = inner.height.max(1);
    let max_offset = rows_len.saturating_sub(visible);
    let offset = state.scroll_conversation.min(max_offset);
    // 回写 clamped 值到 state：u16::MAX 会被归约，下一帧的 saturating_add
    // 不会陷入 "无限大" 状态，scroll_down(Some) 也能判断是否"触底"。
    state.scroll_conversation = offset;
    // 触底则恢复 follow_bottom（用户连续 PageDown / 下滚到底部）。
    if offset >= max_offset {
        state.scroll_follow_bottom = true;
    }
    let view: Vec<Line> = rows.into_iter().skip(offset as usize).collect();

    let p = Paragraph::new(view).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

// ── 4. Turn status line (1 row, hidden when idle) ───────────────────────

fn render_turn_status(f: &mut Frame<'_>, state: &TuiAppState, area: Rect) {
    if area.is_empty() { return; }
    let mut spans: Vec<Span> = vec![Span::raw("  ")];
    if state.is_streaming() {
        spans.push(style_s(c_accent(), "⏳ streaming"));
    }
    let active = state.active_tool_count();
    if active > 0 {
        spans.push(Span::raw("  ·  "));
        spans.push(style_s(c_tool_name(), format!("{active} tool active{}",
            if active == 1 { "" } else { "s" })));
    }
    // Approval attention.
    let n_pending = state.pending_approvals.len();
    if n_pending > 0 {
        spans.push(Span::raw("  ·  "));
        spans.push(style_s(c_warn().add_modifier(Modifier::BOLD),
            format!("{n_pending} approval{} waiting",
                if n_pending == 1 { "" } else { "s" })));
    }
    let p = Paragraph::new(Line::from(spans));
    f.render_widget(p, area);
}

// ── 5. Prompt Widget (Grok chrome: ╭─ title ─╮ / ╰─ info ─╯ borders) ───

fn render_prompt_widget(f: &mut Frame<'_>, state: &TuiAppState, area: Rect) {
    if area.is_empty() { return; }
    let (focused, border_color) = match state.input_mode {
        InputMode::Prompt => (true, c_border_active()),
        _ => (false, c_border_idle()),
    };

    let h = area.height as usize;
    let sty = Style::default().fg(border_color);

    // ── Draw the complete box manually (no Block widget) ──────────────
    // This guarantees ╭╮╰╯ corners are at the exact area edges, │ sides
    // connect top and bottom, and the bottom border is drawn by the info
    // line function — all in one consistent coordinate system.
    //
    // Layout (h=4, the minimum for empty input):
    //   y+0: ╭─ title ──────────────────────────╮   (top border)
    //   y+1: │ ❯ placeholder / input             │   (content)
    //   y+2: │                                    │   (content, if needed)
    //   y+h-1: ╰─ model · trust · line ─── PROMPT ╯   (info = bottom border)

    // Top border with title
    let session_title = state.session_id.as_deref()
        .map(|s| format!(" {} ", &s[..s.len().min(10)]))
        .unwrap_or_else(|| " new session ".to_string());
    let (top_spans, _) = build_prompt_borders(area, &session_title, border_color);
    if let Some(row) = top_spans {
        let top_area = Rect { x: area.x, y: area.y, width: area.width, height: 1 };
        let line = Paragraph::new(Line::from(row));
        f.render_widget(line, top_area);
    }

    // Left and right │ borders for every middle row
    let top_y = area.y;
    let bottom_y = area.y + area.height - 1;
    for y in (top_y + 1)..bottom_y {
        f.buffer_mut().set_string(area.x, y, "│", sty);
        f.buffer_mut().set_string(area.x + area.width - 1, y, "│", sty);
    }

    if h < 3 { return; }

    // Content area: inside the box, between top border and info line.
    // info line = bottom_y (drawn by render_prompt_info_line)
    // content = top_y+1 .. bottom_y-1
    let content_area = Rect {
        x: area.x + 1,
        y: top_y + 1,
        width: area.width.saturating_sub(2),
        height: (bottom_y - top_y - 1).max(1),
    };

    // Info area = the bottom border row, full width of area
    let info_area = Rect {
        x: area.x,
        y: bottom_y,
        width: area.width,
        height: 1,
    };

    // ── Render content lines (input + optional slash dropdown) ─────────
    let focused_style = match focused { true => c_prefix(), false => c_dim() };
    let empty = state.input_buffer.is_empty();
    let content_w = content_area.width.max(4) as usize;
    let text_w = (content_w - 2 - 1).max(10); // prefix 2 + margin 1

    let content_lines: Vec<String> = if empty {
        Vec::new()
    } else {
        let mut out = Vec::new();
        for para in state.input_buffer.split('\n') {
            if para.is_empty() {
                out.push(String::new());
                continue;
            }
            out.extend(wrap_str_display_width(para, text_w));
        }
        out
    };

    let max_content_lines = content_area.height as usize;
    let slash_open = state.slash.open;  // Just `/` also opens (matches can be empty = still list all below).
    let slash_rows_n = if slash_open { state.slash_menu_rows() } else { 0 };

    // How many vertical rows does the input portion (placeholder or real
    // lines) occupy? Input fills the top of the pane; the slash dropdown
    // renders immediately beneath it, inside the same content_area.
    let input_rows_n: usize = if empty {
        // Placeholder: single line — Grok-style minimal placeholder.
        1
    } else {
        content_lines.len().max(1)
    };
    // Split content_area vertically into (input sub-region, dropdown sub-region).
    let input_area_height = (input_rows_n as u16).min(content_area.height);
    let dropdown_area_height = (slash_rows_n as u16)
        .min(content_area.height.saturating_sub(input_area_height));
    let input_subarea = Rect {
        y: content_area.y,
        height: input_area_height,
        ..content_area
    };
    let dropdown_subarea = Rect {
        y: content_area.y.saturating_add(input_area_height),
        height: dropdown_area_height,
        ..content_area
    };

    // ── Render input (placeholder or real lines) ───────────────────────
    let mut line_spans: Vec<Line> = Vec::with_capacity(input_rows_n);
    let show_placeholder = empty;

    if show_placeholder {
        let primary = "  Ask anything · Shift+Enter for newline · / for commands";
        let trunc = primary.chars().take(content_w).collect::<String>();
        let st = if focused { c_dim() } else { Style::default().fg(Color::DarkGray) };
        line_spans.push(Line::from(vec![style_s(st, trunc)]));
    } else {
        // Check if there's an inline ghost suffix to show (Grok-style ghost
        // preview for the currently-selected slash completion). The ghost
        // only appears on the FIRST line of the input, appended after the
        // user's real text in dim italic so it's clearly a preview, not
        // something the user has typed yet.
        //
        // The ghost is line-local: if the input wraps across multiple lines
        // we only append it to the first line because the cursor (and thus
        // the partial `/mo` token) is always on line 0 of the wrapped
        // display — multi-line splits are the wrap algorithm, not real
        // newlines the user typed.
        let ghost_for_line0 = if focused {
            state.slash.ghost.as_ref()
        } else {
            None
        };

        for (idx, line) in content_lines.iter().take(input_rows_n).enumerate() {
            let prefix_span = if idx == 0 {
                style_s(focused_style, "❯ ".to_string())
            } else {
                Span::raw("  ".to_string())
            };
            let text_span = style_s(if focused { c_fg() } else { c_muted() }, line.clone());

            let mut line_elems = vec![prefix_span, text_span];

            if idx == 0 {
                if let Some(ghost) = ghost_for_line0 {
                    // Dim + italic = classic "ghost preview" look. Matches
                    // Grok's mid-text completion ghost opacity.
                    let ghost_style = Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC);
                    line_elems.push(Span::styled(ghost.suffix.clone(), ghost_style));
                }
            }

            line_spans.push(Line::from(line_elems));
        }
    }
    while line_spans.len() < input_rows_n {
        line_spans.push(Line::from(vec![]));
    }
    let para = Paragraph::new(line_spans).wrap(Wrap { trim: false });
    f.render_widget(para, input_subarea);

    // ── Render slash dropdown ───────────────────────────────────────────
    if slash_open && dropdown_subarea.height > 0 && !state.slash.matches.is_empty() {
        render_slash_dropdown(f, state, dropdown_subarea);
    }

    // ── Cursor positioning ──────────────────────────────────────────────
    if focused {
        // Cursor is always relative to input portion rows + wrap width.
        let (cur_row, cur_col) = cursor_row_col_for_render(
            &state.input_buffer,
            state.input_cursor,
            text_w,
        );
        let cur_row = cur_row.min(input_rows_n.saturating_sub(1)) as u16;
        let prefix_cols = 2u16;
        let cx = input_subarea.x
            .saturating_add(prefix_cols)
            .saturating_add(cur_col as u16)
            .min(input_subarea.x + input_subarea.width.saturating_sub(1));
        let cy = input_subarea.y
            .saturating_add(cur_row)
            .min(input_subarea.y + input_subarea.height.saturating_sub(1));
        f.set_cursor(cx, cy);
    }

    // ── Info row: model · flags · multiline · Enter hint ────────────────
    render_prompt_info_line(f, state, info_area, border_color);
}

/// Build spans for a text string with fuzzy/prefix match highlighting.
///
/// Characters at positions listed in `indices` get `match_style` (accent color),
/// all others get `normal_style`. Adjacent characters with the same style are
/// coalesced into a single `Span` to keep the span count low.
/// Mirrors Grok's `build_highlighted_spans` in slash_dropdown.rs.
fn build_highlighted_spans(
    text: &str,
    indices: &[u32],
    normal_style: Style,
    match_style: Style,
) -> Vec<Span<'static>> {
    if indices.is_empty() {
        return vec![Span::styled(text.to_string(), normal_style)];
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current = String::new();
    let mut current_is_match = false;
    let mut idx_iter = indices.iter().copied().peekable();

    for (char_idx, ch) in text.chars().enumerate() {
        let is_match = idx_iter.peek() == Some(&(char_idx as u32));
        if is_match {
            idx_iter.next();
        }

        if char_idx == 0 {
            current_is_match = is_match;
            current.push(ch);
        } else if is_match == current_is_match {
            current.push(ch);
        } else {
            // Style transition — flush current run.
            let style = if current_is_match {
                match_style
            } else {
                normal_style
            };
            spans.push(Span::styled(std::mem::take(&mut current), style));
            current_is_match = is_match;
            current.push(ch);
        }
    }

    if !current.is_empty() {
        let style = if current_is_match {
            match_style
        } else {
            normal_style
        };
        spans.push(Span::styled(current, style));
    }

    spans
}

/// Render the inline slash-command dropdown. Appears directly beneath the
/// user's typed input rows inside the prompt chrome so it looks like a
/// natural extension of the composer (Grok style). Uses the same border
/// color as the active prompt so the two pieces feel connected.
fn render_slash_dropdown(f: &mut Frame<'_>, state: &TuiAppState, area: Rect) {
    if area.is_empty() { return; }

    use ratatui::widgets::ListState;
    let total = state.slash.matches.len();
    let max_visible = (area.height as usize).min(total);
    let selected = state.slash.selected.min(total.saturating_sub(1));

    // Compute scroll offset so the selected row is always visible.
    // Strategy (same as Grok's slash_dropdown.rs render_dropdown):
    //   - If selected < max_visible/2 → scroll = 0 (top)
    //   - If selected near bottom → scroll = total - max_visible
    //   - Otherwise → scroll = selected - max_visible/2
    let scroll = if total <= max_visible || selected < max_visible / 2 {
        0
    } else if selected + max_visible / 2 >= total {
        total.saturating_sub(max_visible)
    } else {
        selected.saturating_sub(max_visible / 2)
    };

    // Slice the matches window [scroll .. scroll+max_visible].
    let items: Vec<ListItem> = state.slash.matches.iter()
        .skip(scroll)
        .take(max_visible)
        .enumerate()
        .map(|(_i, row)| {
            let cmd_idx = row.cmd_idx;
            let cmd = &BUILTIN_SLASH_COMMANDS[cmd_idx];
            // Mark TUI-local (⚡ = fully local, no backend participation)
            // vs ACP / Grodex / Unsupported / Hidden (◈ = registered as
            // fail-closed slash tokens; ALL still intercepted locally,
            // never sent to LLM, but logically belong to the ACP session).
            use super::state::SlashLocalKind;
            let (tag, tag_style) = match cmd.local {
                // Pure TUI-processed — no agent round trip, ever.
                SlashLocalKind::Exit
                | SlashLocalKind::Help
                | SlashLocalKind::DeleteCurrentSession
                | SlashLocalKind::ClearInput => (" ⚡", c_warn()),
                // Everything else: also LOCALLY HANDLED (never leaks to LLM)
                // but tagged with ◈ so the user understands these are
                // logically ACP/session-scoped actions.
                _ => (" ◈", c_accent()),
            };

            // Build per-character highlight spans for the command name.
            // Matched chars use the fuzzy accent color, others use the
            // regular accent — so "/mo" renders "mo" brighter in "/model".
            let name_normal = c_accent();
            let name_match = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            let name_spans = build_highlighted_spans(
                cmd.name,
                &row.match_indices,
                name_normal,
                name_match,
            );

            let mut spans: Vec<Span> = vec![
                Span::raw("  "),
                style_s(c_accent(), "/".to_string()),
            ];
            spans.extend(name_spans);
            spans.push(Span::styled(tag, tag_style));
            spans.push(Span::raw("   "));
            spans.push(Span::styled(cmd.description, c_dim()));

            let line = Line::from(spans);
            ListItem::new(line)
        })
        .collect();

    // Border looks like a natural extension of the prompt. Active side
    // borders connect visually with the prompt chrome's border color.
    let dropdown_block = Block::default()
        .borders(B::TOP | B::LEFT | B::RIGHT)
        .border_style(Style::default().fg(c_border_active()));
    let inner = dropdown_block.inner(area);
    f.render_widget(dropdown_block, area);

    if !inner.is_empty() && !items.is_empty() {
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">>");
        // The ListState selected index is relative to the *visible* slice,
        // so subtract the scroll offset from the absolute selected index.
        let rel_selected = selected.saturating_sub(scroll);
        let mut ls = ListState::default().with_selected(Some(rel_selected));
        f.render_stateful_widget(list, inner, &mut ls);
    }
}

/// Build the styled top border line for the prompt chrome (╭─ title … ─╮).
fn build_prompt_borders(
    area: Rect,
    title: &str,
    color: Color,
) -> (Option<Vec<Span<'static>>>, u16) {
    if area.width < 8 { return (None, area.x); }
    let w = area.width as usize;
    let sty = Style::default().fg(color);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(w);
    spans.push(Span::styled("╭".to_string(), sty));

    // Right-align the session title inside the border — grok puts the
    // session context here and uses `─` as the repeating separator.
    let title_w = title.chars().count();
    let dash_w = w.saturating_sub(2).saturating_sub(title_w); // 2 for ╭ + ╮
    // 2-cell inset from right for breathing room.
    let inset = 2usize;
    let before = dash_w.saturating_sub(title_w).saturating_sub(inset);
    for _ in 0..before {
        spans.push(Span::styled("─".to_string(), sty));
    }
    spans.push(Span::styled(title.to_string(), sty.add_modifier(Modifier::BOLD)));
    // Remaining dashes up to ╮
    let used_so_far = 1 + before + title_w; // ╭ + dashes + title
    let remaining = (w - 1).saturating_sub(used_so_far);
    for _ in 0..remaining {
        spans.push(Span::styled("─".to_string(), sty));
    }
    spans.push(Span::styled("╮".to_string(), sty));
    // Truncate/expand to exactly w cells for the row width.
    spans.truncate(w);
    while spans.len() < w {
        spans.push(Span::styled("─".to_string(), sty));
    }
    (Some(spans), area.x + 1)
}

fn render_prompt_info_line(
    f: &mut Frame<'_>,
    state: &TuiAppState,
    area: Rect,
    border_color: Color,
) {
    if area.is_empty() { return; }
    let w = area.width as usize;
    if w < 4 { return; }
    let border_s = Style::default().fg(border_color);

    // ── Build the bottom border line as a fixed-width string ──────────
    // Positions: [0]=╰  [w-1]=╯  everything else is ─ or text.
    // Using a char buffer guarantees corners land at exact edges.

    let model = if state.model_label.is_empty() { String::from("—") } else { state.model_label.clone() };
    let trust = if state.workspace_trusted { "trusted" } else { "untrusted" };
    let line_mode = if state.input_buffer.contains('\n') { "multi" } else { "single" };
    let left_text = format!(" {model} · {trust} · {line_mode} ");

    let mode_tag = match state.input_mode {
        InputMode::Prompt  => " PROMPT ",
        InputMode::Command => " CMD ",
        InputMode::Normal  => " IDLE ",
    };
    let mode_tag_s = match state.input_mode {
        InputMode::Prompt  => c_user(),
        InputMode::Command => c_warn().add_modifier(Modifier::BOLD),
        InputMode::Normal  => c_dim(),
    };
    let enter_hint = if matches!(state.input_mode, InputMode::Prompt) {
        " Enter to send · "
    } else {
        ""
    };

    // Compute widths
    let left_w = left_text.chars().count();
    let right_w = enter_hint.chars().count() + mode_tag.chars().count();

    // Layout: ╰ ── left_text ── (filler ─) ── enter_hint mode_tag ── ╯
    // Total non-corner content = left_w + filler + right_w
    // Total = 2 (corners) + left_w + filler + right_w = w
    let filler = (w - 2).saturating_sub(left_w + right_w);

    // Build spans left to right
    let mut spans: Vec<Span> = Vec::with_capacity(w);
    spans.push(Span::styled("╰".to_string(), border_s));
    spans.push(Span::styled(left_text, c_muted().add_modifier(Modifier::BOLD)));
    for _ in 0..filler {
        spans.push(Span::styled("─".to_string(), border_s));
    }
    if !enter_hint.is_empty() {
        spans.push(style_s(c_dim(), enter_hint.to_string()));
    }
    spans.push(style_s(mode_tag_s, mode_tag));
    spans.push(Span::styled("╯".to_string(), border_s));

    // Safety: truncate to exactly w to handle rounding from CJK widths
    let total_w: usize = spans.iter().map(|s| s.width()).sum();
    if total_w > w {
        while spans.iter().map(|s| s.width()).sum::<usize>() > w && spans.len() > 3 {
            spans.remove(spans.len() - 2);
        }
    }
    let line = Paragraph::new(Line::from(spans));
    f.render_widget(line, area);
}

// ── Display-width helpers (CJK-aware) ─────────────────────────────────
//
// The user reported that after typing Chinese the cursor sits "a few cells
// away from the text, in what looks like a weird space". Root cause: the
// previous implementation used `.chars().count()` as the displayed column,
// but terminals/ratatui actually render CJK ideographs as **WIDE** (2 cells
// wide), with wide punctuation and some symbols also taking 2 cells. So every
// CJK char caused the cursor to drift +1 cell right vs. where the glyph
// actually ended.
//
// We avoid pulling in `unicode-width` as a new explicit dep (it exists only
// as a transitive dep) and instead hand-roll the small subset we actually
// need, which matches ratatui's internal width contract for CJK-text-input.

pub(crate) fn display_width(c: char) -> usize {
    // Fast-path: ASCII printable.
    if (' '..='~').contains(&c) { return 1; }
    // Control chars and tabs count as 0; a real tab-expander would be nice
    // but the prompt editor doesn't support tabs anyway.
    if c < ' ' || c == '\x7f' { return 0; }
    // CJK Unified Ideographs (U+4E00..U+9FFF, Ext A U+3400..U+4DBF,
    // Ext B-F inside 0x20000+, compat ideographs U+F900..U+FAFF),
    // Hangul syllables U+AC00..U+D7AF, CJK fullwidth punctuation
    // U+3000..U+303F, Hiragana/Katakana U+3040..U+30FF,
    // Bopomofo U+3100..U+312F, Enclosed CJK U+3200..U+33FF,
    // CJK Compatibility Forms U+FE30..U+FE4F, Small Form Variants
    // U+FE50..U+FE6F, Fullwidth ASCII variants U+FF01..U+FF60,
    // Halfwidth/Hangul variants U+FFE0..U+FFE6, fullwidth general
    // punctuation, common emoji.
    let code = c as u32;
    if (0x1100..=0x115F).contains(&code)            // Hangul Jamo
        || (0x2E80..=0x303E).contains(&code)        // CJK radicals / punctuation
        || (0x3041..=0x33FF).contains(&code)        // Hira/Kata/Enclosed/Compat
        || (0x3400..=0x4DB5).contains(&code)        // Ext A
        || (0x4E00..=0x9FFF).contains(&code)        // CJK Unified
        || (0xA000..=0xA4C6).contains(&code)        // Yi syllables (approx)
        || (0xAC00..=0xD7A3).contains(&code)        // Hangul Syllables
        || (0xF900..=0xFAFF).contains(&code)        // Compat Ideographs
        || (0xFE30..=0xFE6F).contains(&code)        // Compat Forms / Small variants
        || (0xFF01..=0xFF60).contains(&code)        // Fullwidth ASCII
        || (0xFFE0..=0xFFE6).contains(&code)        // Fullwidth signs
        // Plane 2: SIP — CJK Ext B/C/D/E/F + Compat Supplement
        || (0x2_0000..=0x2_FFFD).contains(&code)
    {
        2
    } else {
        // Default: 1 column for Latin supplement, Greek, Cyrillic, combining
        // marks (approximated as 1 — correct display of zero-width combiners
        // requires a real Unicode segmentation pass; acceptable approx).
        1
    }
}

fn str_display_width(s: &str) -> usize {
    s.chars().map(display_width).sum()
}

/// Same as `prompt_content_lines` in state.rs but CJK display-width aware.
/// Used by render.rs for the actual cursor/wrap math.
pub(crate) fn cjk_aware_wrapped_rows(para: &str, wrap_w: usize) -> usize {
    let wrap = wrap_w.max(1);
    let mut rows = 0usize;
    let mut cur_w = 0usize;
    for c in para.chars() {
        let w = display_width(c);
        if cur_w + w > wrap {
            rows += 1;
            cur_w = w;
            // Corner case: a char wider than the wrap sits alone on its row
            // (can happen when wrap=1 and char is width 2, extremely narrow).
            if cur_w > wrap { cur_w = wrap; }
        } else {
            cur_w += w;
        }
    }
    rows + 1
}

/// Given a raw prompt buffer with real '\n' hard breaks and a wrap width,
/// compute the (row, displayed column) pair for the cursor. Uses display
/// width so CJK wide glyphs don't cause the cursor to drift into "ghost
/// space" to the right of the actual last glyph.
fn cursor_row_col_for_render(buf: &str, cursor_byte: usize, wrap_w: usize) -> (usize, usize) {
    let cursor_byte = cursor_byte.min(buf.len());
    let before = &buf[..cursor_byte];
    let mut paragraphs: Vec<&str> = before.split('\n').collect();
    if before.ends_with('\n') { paragraphs.push(""); }
    let wrap = wrap_w.max(1);

    let mut row = 0usize;
    for p in paragraphs.iter().take(paragraphs.len().saturating_sub(1)) {
        row += cjk_aware_wrapped_rows(p, wrap);
    }
    // Last paragraph: count cumulative displayed column and extra wraps.
    let last = paragraphs.last().copied().unwrap_or("");
    let mut col = 0usize;
    let mut extra_rows = 0usize;
    for c in last.chars() {
        let w = display_width(c);
        if col + w > wrap {
            extra_rows += 1;
            col = if w > wrap { wrap } else { w };
        } else {
            col += w;
        }
    }
    row += extra_rows;
    (row, col)
}

/// Wrap a single line/paragraph into displayed-width-aware chunks so the
/// prompt body wraps CJK text at the same boundaries the cursor math
/// assumes.
pub(crate) fn wrap_str_display_width(line: &str, wrap_w: usize) -> Vec<String> {
    let wrap = wrap_w.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for c in line.chars() {
        let w = display_width(c);
        if cur_w + w > wrap && !cur.is_empty() {
            out.push(cur);
            cur = String::new();
            cur_w = 0;
        }
        cur.push(c);
        cur_w += w;
        if cur_w > wrap {
            // Single glyph wider than the wrap: emit as its own row.
            out.push(cur);
            cur = String::new();
            cur_w = 0;
        }
    }
    out.push(cur);
    out
}

// ── 6. Shortcuts hint bar (Grok-style: minimal, key · label pairs) ──────
//
// Grok's shortcuts bar has NO [MODE] badge — it just lists the currently
// applicable key→hint pairs in order, truncating from the right when the
// terminal is too narrow. A tiny mode label and the approval-waiting counter
// live right-aligned at the end.

fn render_shortcuts_bar(f: &mut Frame<'_>, state: &TuiAppState, area: Rect) {
    if area.is_empty() { return; }
    let block = Block::default()
        .borders(B::TOP)
        .border_style(Style::default().fg(c_footer_top()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.is_empty() { return; }

    let avail = inner.width as usize;

    // ── Hint pairs (key · label). Order = visual priority left→right. ──
    struct Hint<'a> { kbd: &'a str, label: &'a str }
    let normal_hints = [
        Hint { kbd: "i",       label: "prompt" },
        Hint { kbd: ":",       label: "cmd" },
        Hint { kbd: "j/k",     label: "approvals" },
        Hint { kbd: "↑/↓",     label: "scroll" },
        Hint { kbd: "a/d/c",   label: "resolve" },
        Hint { kbd: "q",       label: "quit" },
    ];
    let prompt_hints: &[Hint] = if state.is_streaming() {
        &[
            Hint { kbd: "Ctrl-C", label: "stop" },
            Hint { kbd: "Esc",    label: "stop" },
            Hint { kbd: "Ctrl-J/K", label: "scroll" },
            Hint { kbd: "PgUp/Dn", label: "page" },
        ]
    } else {
        &[
            Hint { kbd: "Enter",   label: "send" },
            Hint { kbd: "Alt↵",    label: "newline" },
            Hint { kbd: "Ctrl-J/K", label: "scroll" },
            Hint { kbd: "PgUp/Dn", label: "page" },
            Hint { kbd: "Esc",     label: "normal" },
        ]
    };
    let cmd_hints = [
        Hint { kbd: "Enter",   label: "run" },
        Hint { kbd: "Esc",     label: "cancel" },
    ];
    let hints: &[Hint] = match state.input_mode {
        InputMode::Normal  => &normal_hints,
        InputMode::Prompt  => prompt_hints,
        InputMode::Command => &cmd_hints,
    };

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    let mut width_used = 1usize; // leading space
    let kbd_s = c_kbd();
    // Layout: ` kbd  label  ·  kbd  label`
    // Grok keeps the keys styled and labels muted, with a thin · separator.
    for (i, h) in hints.iter().enumerate() {
        let before = spans.len();
        if i > 0 {
            spans.push(Span::styled(" · ".to_string(), c_dim()));
            width_used += 3;
        }
        spans.push(style_s(kbd_s, format!(" {} ", h.kbd)));
        spans.push(style_s(c_muted(), format!(" {} ", h.label)));
        width_used += 2 + h.kbd.chars().count() + 1 + h.label.chars().count() + 2;

        // Truncation gate: if we've already used >= 65% of the bar, don't add
        // further pairs — keep the trailing mode/approval cluster readable.
        if width_used > (avail * 2 / 3) {
            // undo everything pushed by this iteration, stop the loop.
            spans.truncate(before);
            break;
        }
    }

    // ── Right cluster: mode tag (tiny) + approval counter ────────────
    let mode_tag = match state.input_mode {
        InputMode::Normal  => "NORMAL",
        InputMode::Prompt  => "PROMPT",
        InputMode::Command => "CMD",
    };
    let mode_s = match state.input_mode {
        InputMode::Normal  => c_dim().add_modifier(Modifier::BOLD),
        InputMode::Prompt  => c_user().add_modifier(Modifier::BOLD),
        InputMode::Command => c_warn().add_modifier(Modifier::BOLD),
    };
    let n = state.pending_approvals.len();
    let approval_tag = if n > 0 {
        Some(format!(" {n} waiting "))
    } else { None };

    let mut right: Vec<Span> = Vec::new();
    let mut right_w = mode_tag.chars().count() + 2;
    right.push(style_s(mode_s, format!(" {mode_tag} ")));
    if let Some(ref tag) = approval_tag {
        right.push(style_s(c_warn().add_modifier(Modifier::BOLD), tag.clone()));
        right_w += tag.chars().count();
    }

    let left_w: usize = spans.iter().map(|s| s.width()).sum();
    let pad = avail.saturating_sub(left_w).saturating_sub(right_w).max(1);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.extend(right);

    let line = Paragraph::new(Line::from(spans));
    f.render_widget(line, inner);
    let _ = Text::from(""); // silence unused import
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_row_col_single_wrap() {
        // Single paragraph of 30 chars, 10-char wrap: row 2 (0-indexed)
        let buf = "abcdefghij".repeat(3); // "abcdefghij" x 3 = 30 chars
        let (r, c) = cursor_row_col_for_render(&buf, buf.len(), 10);
        assert_eq!(r, 2); // lines 0,1,2
        assert_eq!(c, 10);
    }

    #[test]
    fn cursor_row_col_hard_newline_then_text() {
        let buf = "hi\nworld";
        // cursor after "world"
        let (r, c) = cursor_row_col_for_render(buf, buf.len(), 80);
        // "hi" → 1 row, "world" → 1 row => 0-indexed row 1, col 5
        assert_eq!(r, 1);
        assert_eq!(c, 5);
    }

    #[test]
    fn cursor_row_col_at_line_start() {
        let buf = "hello";
        // Cursor at byte 0 means start.
        let (r, c) = cursor_row_col_for_render(buf, 0, 80);
        assert_eq!(r, 0);
        assert_eq!(c, 0);
    }

    #[test]
    fn wrap_str_preserves_hard_newlines() {
        let out = wrap_str("a\nb", 80);
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn sanitize_single_line_matches_grok() {
        use super::super::event_handler::sanitize_single_line;
        // Only CR/LF are stripped — tabs, CJK, emojis, BEL all pass through.
        assert_eq!(sanitize_single_line("one\r\ntwo\nthree\rfour"), "onetwothreefour");
        assert_eq!(sanitize_single_line("a\tb 名 🚀 \x01"), "a\tb 名 🚀 \x01");
    }
}
