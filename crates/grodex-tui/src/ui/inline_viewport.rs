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

use crossterm::cursor::MoveTo;
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::Command;
use crossterm::queue;
use ratatui::backend::{Backend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Rect, Size};

/// Set scroll region (DECSTBM): `CSI <top>;<bottom> r`.
/// 1-based: row 1 is the first line.
struct SetScrollRegion(pub std::ops::Range<u16>);

impl Command for SetScrollRegion {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b[{};{}r", self.0.start, self.0.end)
    }
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reset scroll region: `CSI r`.
struct ResetScrollRegion;

impl Command for ResetScrollRegion {
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
    B: Backend,
{
    inner: B,
    /// The y-offset (row number) where the viewport starts.
    /// All ratatui y coordinates have this added when drawing.
    y_offset: u16,
}

impl<B> InlineBackend<B>
where
    B: Backend,
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
        self.inner.get_cursor().unwrap_or((0, 0))
    }
}

impl<B> Backend for InlineBackend<B>
where
    B: Backend,
{
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

    fn get_cursor(&mut self) -> io::Result<(u16, u16)> {
        // Translate the absolute position to viewport-relative.
        let (x, y) = self.inner.get_cursor()?;
        Ok((x, y.saturating_sub(self.y_offset)))
    }

    fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
        // Translate viewport-relative to absolute.
        self.inner.set_cursor(x, y + self.y_offset)
    }

    fn clear(&mut self) -> io::Result<()> {
        // Inline viewport 模式：绝不能清屏（会擦掉真实 scrollback）。
        // ratatui 的 diff-based draw() 只重绘变化的 cell，不需要全屏 clear。
        // viewport 区域的清除由 clear_viewport_area()（直接写 stderr）负责，
        // 不走 Backend::clear。这里返回 Ok(()) 即可。
        Ok(())
    }

    fn size(&self) -> io::Result<Rect> {
        // Return viewport size, not full screen size.
        // This tells ratatui how much space it has to render.
        let full = self.inner.size()?;
        Ok(Rect::new(
            0,
            0,
            full.width,
            full.height.saturating_sub(self.y_offset),
        ))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
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

// ── History insertion ──────────────────────────────────────────────

/// Insert finalized chat lines into the terminal scrollback above the viewport.
///
/// Uses `SetScrollRegion` to limit scrolling to the area above the viewport,
/// then writes lines using `MoveTo` + `Print`. This pushes old content up
/// into the real terminal scrollback, exactly like Codex.
///
/// After insertion, the viewport's y_offset is updated to account for the
/// new lines (viewport moves down).
pub fn insert_history_lines<W: Write>(
    writer: &mut W,
    lines: &[String],
    viewport_y: u16,
    screen_height: u16,
    wrap_width: usize,
) -> io::Result<u16> {
    if lines.is_empty() {
        return Ok(viewport_y);
    }

    // Pre-wrap lines to terminal width.
    let wrapped: Vec<String> = lines
        .iter()
        .flat_map(|line| wrap_line(line, wrap_width))
        .collect();
    let wrapped_count = wrapped.len() as u16;

    // If viewport is not at the bottom of the screen, scroll it down
    // to make room for the history lines.
    let viewport_bottom = viewport_y.saturating_add(1); // viewport includes at least 1 row
    let actual_viewport_bottom = screen_height;
    if viewport_bottom < actual_viewport_bottom {
        let scroll_amount = wrapped_count.min(actual_viewport_bottom - viewport_bottom);
        let top_1based = viewport_y + 1;
        queue!(writer, SetScrollRegion(top_1based..screen_height))?;
        queue!(writer, MoveTo(0, viewport_y))?;
        for _ in 0..scroll_amount {
            // ESC M = Reverse Index (scroll up by 1 line within scroll region)
            queue!(writer, Print("\x1bM"))?;
        }
        queue!(writer, ResetScrollRegion)?;
    }

    // Set scroll region to the area ABOVE the viewport.
    // Lines written here will scroll within this region, pushing
    // old content up into real terminal scrollback.
    //
    // ┌─Screen───────────────────────┐
    // │┌╌Scroll region╌╌╌╌╌╌╌╌╌╌╌╌╌╌┐│
    // │┆  (old content scrolls up)  ┆│
    // │┆  (new history lines)       ┆│
    // │█╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┘│
    // │╭─Viewport───────────────────╮│
    // ││  (live ratatui rendering)  ││
    // │╰────────────────────────────╯│
    // └──────────────────────────────┘
    if viewport_y > 0 {
        queue!(writer, SetScrollRegion(1..viewport_y))?;
        let cursor_top = viewport_y.saturating_sub(1);
        queue!(writer, MoveTo(0, cursor_top))?;
        for line in &wrapped {
            queue!(writer, Print("\r\n"))?;
            // Truncate line to terminal width
            let truncated = if line.len() > wrap_width {
                &line[..wrap_width]
            } else {
                line.as_str()
            };
            queue!(writer, Print(truncated))?;
            queue!(writer, Clear(ClearType::UntilNewLine))?;
        }
        queue!(writer, ResetScrollRegion)?;
    }

    writer.flush()?;
    Ok(viewport_y + wrapped_count)
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
