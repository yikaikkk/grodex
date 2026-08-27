//! Inline viewport backend — Codex-style normal-screen rendering.
//!
//! Instead of entering alternate screen, we render to the normal screen
//! starting from the cursor position at startup. This preserves terminal
//! scrollback and allows native text selection.
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────────────────────────┐
//! │ 已完成对话 / 原来的 shell 内容 │  ← 真实 terminal scrollback
//! │                              │
//! ├──────────────────────────────┤
//! │ 当前流式回答                  │
//! │ 状态 / 审批                   │  ← Inline viewport (ratatui)
//! │ 输入框                        │
//! └──────────────────────────────┘
//! ```
//!
//! `InlineBackend` wraps `CrosstermBackend` and adds `y_offset` to all
//! y coordinates, so ratatui renders to the correct area without knowing
//! about the offset.

use std::fmt;
use std::io::{self, Write};

use crossterm::cursor::{MoveTo, SavePosition, RestorePosition};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::Command;
use crossterm::queue;
use ratatui::backend::{Backend, ClearType as RatatuiClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Set scroll region (DECSTBM): `CSI <top>;<bottom> r`.
/// 1-based: row 1 is the first line.
pub struct SetScrollRegionCmd(pub std::ops::Range<u16>);

impl Command for SetScrollRegionCmd {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reset scroll region: `CSI r`.
pub struct ResetScrollRegionCmd;

impl Command for ResetScrollRegionCmd {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[r")
    }
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A backend wrapper that adds a y-offset to all y coordinates.
///
/// This allows ratatui to render to a sub-area of the screen starting
/// at `y_offset` rows from the top, without entering alternate screen.
/// Ratatui thinks it has a terminal of height `screen_height - y_offset`,
/// but all draw commands are translated to the correct absolute position.
pub struct InlineBackend<B>
where
    B: Backend<Error = io::Error>,
{
    inner: B,
    /// The y-offset (row number) where the viewport starts.
    /// All ratatui y coordinates have this added when drawing.
    y_offset: u16,
}

impl<B> InlineBackend<B>
where
    B: Backend<Error = io::Error>,
{
    pub fn new(inner: B, y_offset: u16) -> Self {
        Self { inner, y_offset }
    }

    /// Update the y-offset (e.g., after terminal resize or history insertion).
    pub fn set_y_offset(&mut self, offset: u16) {
        self.y_offset = offset;
    }

    /// Get the current y-offset.
    pub fn y_offset(&self) -> u16 {
        self.y_offset
    }

    /// Probe the current cursor position using the inner backend.
    /// Returns the (x, y) position, or (0, 0) if probing fails.
    pub fn probe_cursor_position(&mut self) -> (u16, u16) {
        self.inner
            .get_cursor_position()
            .map(|p| (p.x, p.y))
            .unwrap_or((0, 0))
    }
}

impl<B> Backend for InlineBackend<B>
where
    B: Backend<Error = io::Error>,
{
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Translate y coordinates by adding y_offset.
        let offset = self.y_offset;
        let translated = content.map(move |(x, y, cell)| (x, y + offset, cell));
        self.inner.draw(translated)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, io::Error> {
        // Translate the absolute position to viewport-relative.
        let pos = self.inner.get_cursor_position()?;
        Ok(Position {
            x: pos.x,
            y: pos.y.saturating_sub(self.y_offset),
        })
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        // Translate viewport-relative to absolute.
        let pos: Position = position.into();
        self.inner
            .set_cursor_position(Position {
                x: pos.x,
                y: pos.y + self.y_offset,
            })
    }

    fn clear(&mut self) -> io::Result<()> {
        // Inline viewport 模式：绝不能清屏（会擦掉真实 scrollback）。
        // ratatui 的 diff-based draw() 只重绘变化的 cell，不需要全屏 clear。
        // viewport 区域的清除由 clear_viewport_area()（直接写 stderr）负责，
        // 不走 Backend::clear。这里返回 Ok(()) 即可。
        Ok(())
    }

    fn clear_region(&mut self, _clear_type: RatatuiClearType) -> io::Result<()> {
        // Same as clear(): never clear in inline viewport mode.
        Ok(())
    }

    fn size(&self) -> Result<Size, io::Error> {
        // Return viewport size, not full screen size.
        // This tells ratatui how much space it has to render.
        let full = self.inner.size()?;
        Ok(Size {
            width: full.width,
            height: full.height.saturating_sub(self.y_offset),
        })
    }

    fn window_size(&mut self) -> Result<WindowSize, io::Error> {
        let full = self.inner.window_size()?;
        let viewport_cols_rows = Size::new(
            full.columns_rows.width,
            full.columns_rows
                .height
                .saturating_sub(self.y_offset),
        );
        Ok(WindowSize {
            columns_rows: viewport_cols_rows,
            pixels: full.pixels,
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ── Scroll region operations (对齐 codex) ─────────────────────────

/// Scroll the rows in `region` upward by `scroll_by` rows.
///
/// This is equivalent to ratatui's `Backend::scroll_region_up` (available
/// with the `scrolling-regions` feature in ratatui ≥ 0.30). Since grodex
/// uses ratatui 0.27, we implement it manually via ANSI escape sequences.
///
/// Mechanism:
/// 1. Set scroll region (DECSTBM) to cover `region`
/// 2. Move cursor to the bottom row of the region
/// 3. Output `scroll_by` line-feeds — each LF scrolls the region up by 1
/// 4. Reset scroll region
///
/// The top `scroll_by` rows are pushed into the terminal's real scrollback.
pub fn scroll_region_up<W: Write>(
    writer: &mut W,
    region: std::ops::Range<u16>,
    scroll_by: u16,
) -> io::Result<()> {
    if scroll_by == 0 || region.is_empty() {
        return Ok(());
    }
    // DECSTBM uses 1-based row numbers.
    let top_1based = region.start + 1;
    let bottom_1based = region.end; // region.end is exclusive in 0-based = last_row+1, which equals 1-based bottom
    queue!(writer, SetScrollRegionCmd(top_1based..bottom_1based))?;
    // Move cursor to the bottom row of the scroll region.
    queue!(writer, MoveTo(0, region.end - 1))?;
    for _ in 0..scroll_by {
        // LF at the bottom of a scroll region scrolls the region up by 1.
        queue!(writer, Print("\n"))?;
    }
    queue!(writer, ResetScrollRegionCmd)?;
    writer.flush()
}

// ── History insertion (对齐 codex insert_history.rs Standard mode) ───

/// Insert finalized chat lines above the viewport, pushing them into the
/// terminal's real scrollback.
///
/// The mechanism (from codex):
/// 1. Set scroll region to rows `1..viewport_y` (1-based: rows above viewport)
/// 2. Place cursor at `viewport_y - 1` (bottom row of the scroll region)
/// 3. For each line: `\r\n` then write the text
///    - `\r\n` at the bottom of the scroll region: the region scrolls up
///      by 1 row, cursor stays at the bottom (standard terminal behavior)
///    - Text is written at the cursor position (bottom of scroll region)
/// 4. Reset scroll region, restore cursor
///
/// Result: history lines appear contiguously above the viewport. The
/// oldest rows are pushed into the terminal's native scrollback, where
/// the user can browse them with the terminal's native scroll wheel.
///
/// Returns the new viewport_y (shifted down by the number of inserted rows).
pub fn insert_history_lines<W: Write>(
    writer: &mut W,
    lines: &[String],
    viewport_y: u16,
    _screen_height: u16,
    wrap_width: usize,
) -> io::Result<u16> {
    if lines.is_empty() || viewport_y == 0 {
        return Ok(viewport_y);
    }

    // Pre-wrap lines to terminal width.
    let wrapped: Vec<String> = lines
        .iter()
        .flat_map(|line| wrap_line(line, wrap_width))
        .collect();
    let wrapped_count = wrapped.len() as u16;

    // Clamp to available space above viewport.
    let insert_count = wrapped_count.min(viewport_y);

    // Save cursor position so we can restore it after the operation.
    queue!(writer, SavePosition)?;

    // Set scroll region to the area above the viewport (1-based).
    // Row 0 → 1-based 1, row viewport_y-1 → 1-based viewport_y.
    queue!(writer, SetScrollRegionCmd(1..viewport_y))?;

    // Place cursor at the bottom row of the scroll region.
    let cursor_row = viewport_y - 1;
    queue!(writer, MoveTo(0, cursor_row))?;

    // Write each line: \r\n scrolls the region up, then write text.
    // The cursor stays at cursor_row (bottom of scroll region) after
    // each \r\n, so successive writes stack upward naturally.
    for line in &wrapped[..insert_count as usize] {
        queue!(writer, Print("\r\n"))?;
        queue!(writer, Clear(ClearType::UntilNewLine))?;
        queue!(writer, Print(line.as_str()))?;
    }

    // Reset scroll region and restore cursor.
    queue!(writer, ResetScrollRegionCmd)?;
    queue!(writer, RestorePosition)?;
    writer.flush()?;

    // The viewport effectively moves down by the number of inserted rows
    // (the rows that were just written now sit above the viewport).
    Ok(viewport_y + insert_count)
}

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

/// Clear the viewport area (from y_offset to bottom of screen).
/// Called before the first render to ensure no stale content remains.
pub fn clear_viewport_area<W: Write>(
    writer: &mut W,
    y_offset: u16,
    screen_height: u16,
) -> io::Result<()> {
    for row in y_offset..screen_height {
        queue!(writer, MoveTo(0, row), Clear(ClearType::UntilNewLine))?;
    }
    writer.flush()
}

/// Ensure there is enough space for the viewport at the bottom of the screen.
/// If the cursor is too close to the bottom, print newlines to push content
/// up and create space.
pub fn ensure_viewport_space<W: Write>(
    writer: &mut W,
    cursor_y: u16,
    screen_height: u16,
    min_viewport_rows: u16,
) -> io::Result<u16> {
    let available = screen_height.saturating_sub(cursor_y);
    if available < min_viewport_rows {
        // Not enough space: print newlines to scroll the terminal
        let needed = min_viewport_rows - available;
        for _ in 0..needed {
            queue!(writer, Print("\r\n"))?;
        }
        writer.flush()?;
        // After printing newlines, the cursor (and thus viewport) is at the bottom
        Ok(screen_height.saturating_sub(min_viewport_rows))
    } else {
        Ok(cursor_y)
    }
}
