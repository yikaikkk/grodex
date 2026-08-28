//! Custom inline-viewport Terminal — ported from codex
//! (`codex-rs/tui/src/custom_terminal.rs`), which is itself derived from
//! `ratatui::Terminal` (MIT, Copyright 2016-2025 The Ratatui Developers).
//!
//! 与 ratatui 内置 Terminal 的关键差异（对齐 codex 设计）:
//!
//! * 没有 `Viewport` enum。`viewport_area` 完全手动管理:启动时零高度锚定在
//!   光标行（inline 模式,启动前的终端历史全部可见）,随内容增长向上延伸。
//! * `draw_with_size()`:调用方已查询过屏幕尺寸时不再重复查询 backend。
//! * `set_viewport_area()` / `clear_after_position()` / `note_history_rows_inserted()`:
//!   供 inline viewport 增长、历史推入（见 `insert_history`）使用。
//! * `clear_visible_screen()` / `clear_scrollback_and_visible_screen_ansi()`:
//!   清屏辅助。
//! * diff 输出带 `ClearToEnd` 优化与 OSC8 超链接合并（原样移植）。
//!
//! 另外补充了 ratatui `Frame` 的 `render_widget` / `render_stateful_widget`
//! 便捷方法,使 `ui/render.rs` 无需改动即可使用本 Frame。

use std::io;
use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::cursor::SetCursorStyle;
use crossterm::queue;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::backend::IntoCrossterm;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::buffer::CellDiffOption;
use ratatui::buffer::CellWidth;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::widgets::StatefulWidget;
use ratatui::widgets::Widget;

fn osc8_hyperlink_parts(symbol: &str) -> Option<(&str, &str)> {
    let content = symbol.strip_prefix("\x1b]8;;")?;
    let destination_end = content.find('\x07')?;
    let destination = &content[..destination_end];
    if destination.is_empty() {
        return None;
    }
    let visible = content[destination_end + 1..].strip_suffix("\x1b]8;;\x07")?;
    Some((destination, visible))
}

pub struct Frame<'a> {
    /// Where should the cursor be after drawing this frame?
    ///
    /// If `None`, the cursor is hidden and its position is controlled by the backend. If `Some((x,
    /// y))`, the cursor is shown and placed at `(x, y)` after the call to `Terminal::draw()`.
    pub(crate) cursor_position: Option<Position>,

    /// Visible cursor shape to apply after drawing this frame.
    cursor_style: SetCursorStyle,

    /// The area of the viewport
    pub(crate) viewport_area: Rect,

    /// The buffer that is used to draw the current frame
    pub(crate) buffer: &'a mut Buffer,
}

impl Frame<'_> {
    /// The area of the current frame
    ///
    /// This is guaranteed not to change during rendering, so may be called multiple times.
    pub const fn area(&self) -> Rect {
        self.viewport_area
    }

    /// Render a [`Widget`] to the current buffer (ratatui `Frame` 兼容方法,
    /// 供 `ui/render.rs` 原样使用)。
    pub fn render_widget<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer);
    }

    /// Render a [`StatefulWidget`] to the current buffer (ratatui `Frame` 兼容方法)。
    pub fn render_stateful_widget<W>(&mut self, widget: W, area: Rect, state: &mut W::State)
    where
        W: StatefulWidget,
    {
        widget.render(area, self.buffer, state);
    }

    /// After drawing this frame, make the cursor visible and put it at the specified (x, y)
    /// coordinates. If this method is not called, the cursor will be hidden.
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }

    /// After drawing this frame, set the terminal's visible cursor style.
    pub fn set_cursor_style(&mut self, style: SetCursorStyle) {
        self.cursor_style = style;
    }

    /// Gets the buffer that this `Frame` draws into as a mutable reference.
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Hash)]
pub struct Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    /// The backend used to interface with the terminal
    backend: B,
    /// Holds the results of the current and previous draw calls. The two are compared at the end
    /// of each draw pass to output the necessary updates to the terminal
    buffers: [Buffer; 2],
    /// Index of the current buffer in the previous array
    current: usize,
    /// Whether the cursor is currently hidden
    pub hidden_cursor: bool,
    /// Area of the viewport
    pub viewport_area: Rect,
    /// Last known size of the terminal. Used to detect if the internal buffers have to be resized.
    pub last_known_screen_size: Size,
    /// Last known position of the cursor. Used to find the new area when the viewport is inlined
    /// and the terminal resized.
    pub last_known_cursor_pos: Position,
    /// Count of visible history rows rendered above the viewport in inline mode.
    visible_history_rows: u16,
}

impl<B> Drop for Terminal<B>
where
    B: Backend<Error = io::Error>,
    B: Write,
{
    fn drop(&mut self) {
        // Attempt to restore the cursor state
        if let Err(err) = self.reset_cursor_style() {
            eprintln!("Failed to reset the cursor style: {err}");
        }

        if self.hidden_cursor
            && let Err(err) = self.show_cursor()
        {
            eprintln!("Failed to show the cursor: {err}");
        }
    }
}

impl<B> Terminal<B>
where
    B: Backend<Error = io::Error>,
    B: Write,
{
    /// Creates a new [`Terminal`] with the given [`Backend`].
    ///
    /// viewport 初始为零高度、锚定在当前光标行 —— 即 codex 的 inline 模式:
    /// 启动前的终端历史保留在屏幕/scrollback 上,全部可见。
    /// 光标探测失败（部分 PTY 不回答 CPR `ESC[6n`）时退回原点,不阻塞启动。
    pub fn with_options(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend.get_cursor_position().unwrap_or_else(|_| {
            Position { x: 0, y: 0 }
        });
        Ok(Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(0, cursor_pos.y, 0, 0),
            last_known_screen_size: screen_size,
            last_known_cursor_pos: cursor_pos,
            visible_history_rows: 0,
        })
    }

    /// Creates a new [`Terminal`] without probing the cursor position.
    ///
    /// 供 fullscreen（alternate screen）模式使用:viewport 由调用方在启动时
    /// 直接设为整屏,光标锚点无意义;跳过 CPR 探测可避免在不应答 `ESC[6n`
    /// 的终端上阻塞启动（crossterm 内部超时约 2 秒）。
    pub fn with_backend(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        Ok(Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::ZERO,
            last_known_screen_size: screen_size,
            last_known_cursor_pos: Position { x: 0, y: 0 },
            visible_history_rows: 0,
        })
    }

    /// Get a Frame object which provides a consistent view into the terminal state for rendering.
    pub fn get_frame(&mut self) -> Frame<'_> {
        Frame {
            cursor_position: None,
            cursor_style: SetCursorStyle::DefaultUserShape,
            viewport_area: self.viewport_area,
            buffer: self.current_buffer_mut(),
        }
    }

    /// Gets the current buffer as a reference.
    fn current_buffer(&self) -> &Buffer {
        &self.buffers[self.current]
    }

    /// Gets the current buffer as a mutable reference.
    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    /// Gets the previous buffer as a reference.
    fn previous_buffer(&self) -> &Buffer {
        &self.buffers[1 - self.current]
    }

    /// Gets the previous buffer as a mutable reference.
    fn previous_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[1 - self.current]
    }

    /// Gets the backend
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Gets the backend as a mutable reference
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Obtains a difference between the previous and the current buffer and passes it to the
    /// current backend for drawing.
    pub fn flush(&mut self) -> io::Result<()> {
        let updates = diff_buffers(self.previous_buffer(), self.current_buffer());
        let last_put_command = updates.iter().rfind(|command| command.is_put());
        if let Some(&DrawCommand::Put { x, y, .. }) = last_put_command {
            self.last_known_cursor_pos = Position { x, y };
        }
        draw(&mut self.backend, updates.into_iter())
    }

    /// Updates the Terminal so that internal buffers match the requested area.
    ///
    /// Requested area will be saved to remain consistent when rendering. This leads to a full clear
    /// of the screen.
    pub fn resize(&mut self, screen_size: Size) -> io::Result<()> {
        self.last_known_screen_size = screen_size;
        Ok(())
    }

    /// Sets the viewport area.
    pub fn set_viewport_area(&mut self, area: Rect) {
        self.current_buffer_mut().resize(area);
        self.previous_buffer_mut().resize(area);
        self.viewport_area = area;
        self.visible_history_rows = self.visible_history_rows.min(area.top());
    }

    /// Queries the backend for size and resizes if it doesn't match the previous size.
    pub fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.size()?;
        if screen_size != self.last_known_screen_size {
            self.resize(screen_size)?;
        }
        Ok(())
    }

    /// Draws a single frame to the terminal.
    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        let screen_size = self.size()?;
        self.draw_with_size(screen_size, render_callback)
    }

    /// Draws a single frame using a screen size already obtained by the caller.
    pub fn draw_with_size<F>(&mut self, screen_size: Size, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        if screen_size != self.last_known_screen_size {
            self.resize(screen_size)?;
        }
        let mut frame = self.get_frame();

        render(&mut frame);

        // We can't change the cursor position right away because we have to flush the frame to
        // stdout first. But we also can't keep the frame around, since it holds a &mut to
        // Buffer. Thus, we're taking the important data out of the Frame and dropping it.
        let cursor_position = frame.cursor_position;
        let cursor_style = frame.cursor_style;

        // Draw to stdout
        self.flush()?;

        match cursor_position {
            None => self.hide_cursor()?,
            Some(position) => {
                self.set_cursor_style(cursor_style)?;
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }

        self.swap_buffers();

        Backend::flush(&mut self.backend)?;

        Ok(())
    }

    /// Hides the cursor.
    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    /// Shows the cursor.
    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    /// Sets the visible terminal cursor style.
    pub fn set_cursor_style(&mut self, style: SetCursorStyle) -> io::Result<()> {
        queue!(self.backend, style)
    }

    /// Restores the user-configured terminal cursor style.
    pub fn reset_cursor_style(&mut self) -> io::Result<()> {
        self.set_cursor_style(SetCursorStyle::DefaultUserShape)
    }

    /// Gets the current cursor position.
    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    /// Sets the cursor position.
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    /// Clear the terminal (viewport) and force a full redraw on the next draw call.
    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        self.clear_after_position(self.viewport_area.as_position())
    }

    /// Clear from `position` through the end of the visible screen and force a full redraw.
    pub fn clear_after_position(&mut self, position: Position) -> io::Result<()> {
        self.backend.set_cursor_position(position)?;
        self.backend.clear_region(ClearType::AfterCursor)?;
        // Reset the back buffer to make sure the next update will redraw everything.
        self.previous_buffer_mut().reset();
        Ok(())
    }

    /// Clear the entire visible screen (not just the viewport) and force a full redraw.
    pub fn clear_visible_screen(&mut self) -> io::Result<()> {
        let home = Position { x: 0, y: 0 };
        self.set_cursor_position(home)?;
        self.backend.clear_region(ClearType::All)?;
        self.set_cursor_position(home)?;
        std::io::Write::flush(&mut self.backend)?;
        self.visible_history_rows = 0;
        self.previous_buffer_mut().reset();
        Ok(())
    }

    pub(crate) fn note_history_rows_inserted(&mut self, inserted_rows: u16) {
        self.visible_history_rows = self
            .visible_history_rows
            .saturating_add(inserted_rows)
            .min(self.viewport_area.top());
    }

    /// Clears the inactive buffer and swaps it with the current buffer
    pub fn swap_buffers(&mut self) {
        self.previous_buffer_mut().reset();
        self.current = 1 - self.current;
    }

    /// Queries the real size of the backend.
    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }
}

#[derive(Debug)]
enum DrawCommand {
    Put { x: u16, y: u16, cell: Cell },
    ClearToEnd { x: u16, y: u16, bg: Color },
}

impl DrawCommand {
    fn is_put(&self) -> bool {
        matches!(self, DrawCommand::Put { .. })
    }
}

fn diff_buffers(a: &Buffer, b: &Buffer) -> Vec<DrawCommand> {
    let next_buffer = &b.content;

    let mut updates = vec![];
    let mut last_nonblank_columns = vec![0; a.area.height as usize];
    for y in 0..a.area.height {
        let row_start = y as usize * a.area.width as usize;
        let row_end = row_start + a.area.width as usize;
        let row = &next_buffer[row_start..row_end];
        let bg = row.last().map(|cell| cell.bg).unwrap_or(Color::Reset);

        // Scan the row to find the rightmost column that still matters: any non-space glyph,
        // any cell whose bg differs from the row's trailing bg, or any cell with modifiers.
        // Multi-width glyphs extend that region through their full displayed width.
        // After that point the rest of the row can be cleared with a single ClearToEnd, a perf win
        // versus emitting multiple space Put commands.
        let mut last_nonblank_column = 0usize;
        let mut column = 0usize;
        while column < row.len() {
            let cell = &row[column];
            let width = usize::from(cell.cell_width());
            if cell.symbol() != " " || cell.bg != bg || cell.modifier != Modifier::empty() {
                last_nonblank_column = column + (width.saturating_sub(1));
            }
            column += width.max(1); // treat zero-width symbols as width 1
        }

        if last_nonblank_column + 1 < row.len() {
            let (x, y) = a.pos_of(row_start + last_nonblank_column + 1);
            updates.push(DrawCommand::ClearToEnd { x, y, bg });
        }

        last_nonblank_columns[y as usize] = last_nonblank_column as u16;
    }

    let mut cell_updates = a.diff_iter(b).collect::<Vec<_>>();
    // Ratatui's ForcedWidth path skips trailing-cell invalidation when a styled wide cell shrinks.
    let visible_on_blank = Modifier::REVERSED
        .union(Modifier::UNDERLINED)
        .union(Modifier::SLOW_BLINK)
        .union(Modifier::RAPID_BLINK)
        .union(Modifier::CROSSED_OUT);
    for (i, (current, previous)) in next_buffer.iter().zip(a.content.iter()).enumerate() {
        let CellDiffOption::ForcedWidth(current_width) = current.diff_option else {
            continue;
        };
        let current_width = usize::from(current_width.get());
        let previous_width = usize::from(previous.cell_width());
        if previous_width <= current_width
            || (previous.bg == Color::Reset && !previous.modifier.intersects(visible_on_blank))
        {
            continue;
        }

        for (index, cell) in next_buffer
            .iter()
            .enumerate()
            .skip(i + current_width)
            .take(previous_width - current_width)
        {
            #[allow(deprecated)]
            let is_skip = cell.diff_option == CellDiffOption::Skip
                || (cell.skip && cell.diff_option == CellDiffOption::None);
            if !is_skip {
                let (x, y) = a.pos_of(index);
                cell_updates.push((x, y, cell));
            }
        }
    }
    cell_updates.sort_unstable_by_key(|(x, y, _)| (*y, *x));
    cell_updates.dedup_by_key(|(x, y, _)| (*y, *x));

    for (x, y, cell) in cell_updates {
        let row = usize::from(y - a.area.y);
        if x <= last_nonblank_columns[row] {
            updates.push(DrawCommand::Put {
                x,
                y,
                cell: cell.clone(),
            });
        }
    }
    updates
}

fn draw<I>(writer: &mut impl Write, commands: I) -> io::Result<()>
where
    I: Iterator<Item = DrawCommand>,
{
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut last_pos: Option<Position> = None;
    let mut active_hyperlink: Option<String> = None;
    for command in commands {
        let (x, y) = match &command {
            DrawCommand::Put { x, y, .. } => (x, y),
            DrawCommand::ClearToEnd { x, y, .. } => (x, y),
        };
        let hyperlink = match &command {
            DrawCommand::Put { cell, .. } => osc8_hyperlink_parts(cell.symbol()),
            DrawCommand::ClearToEnd { .. } => None,
        };
        let destination = hyperlink.map(|(destination, _)| destination);
        let hyperlink_changed = active_hyperlink.as_deref() != destination;
        if hyperlink_changed && active_hyperlink.is_some() {
            queue!(writer, Print("\x1b]8;;\x07"))?;
        }
        // Move the cursor if the previous location was not (x - 1, y)
        if !matches!(last_pos, Some(p) if *x == p.x + 1 && *y == p.y) {
            queue!(writer, MoveTo(*x, *y))?;
        }
        last_pos = Some(Position { x: *x, y: *y });
        match &command {
            DrawCommand::Put { cell, .. } => {
                if cell.modifier != modifier {
                    let diff = ModifierDiff {
                        from: modifier,
                        to: cell.modifier,
                    };
                    diff.queue(writer)?;
                    modifier = cell.modifier;
                }
                if cell.fg != fg || cell.bg != bg {
                    queue!(
                        writer,
                        SetColors(Colors::new(
                            cell.fg.into_crossterm(),
                            cell.bg.into_crossterm()
                        ))
                    )?;
                    fg = cell.fg;
                    bg = cell.bg;
                }

                if hyperlink_changed && let Some(destination) = destination {
                    queue!(writer, Print(format!("\x1b]8;;{destination}\x07")))?;
                }
                let symbol = hyperlink.map_or_else(|| cell.symbol(), |(_, visible)| visible);
                queue!(writer, Print(symbol))?;
            }
            DrawCommand::ClearToEnd { bg: clear_bg, .. } => {
                queue!(writer, SetAttribute(crossterm::style::Attribute::Reset))?;
                modifier = Modifier::empty();
                queue!(writer, SetBackgroundColor((*clear_bg).into_crossterm()))?;
                bg = *clear_bg;
                queue!(writer, Clear(crossterm::terminal::ClearType::UntilNewLine))?;
            }
        }
        if hyperlink_changed {
            active_hyperlink = destination.map(str::to_owned);
        }
    }
    if active_hyperlink.is_some() {
        queue!(writer, Print("\x1b]8;;\x07"))?;
    }

    queue!(
        writer,
        SetForegroundColor(crossterm::style::Color::Reset),
        SetBackgroundColor(crossterm::style::Color::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )?;

    Ok(())
}

/// The `ModifierDiff` struct is used to calculate the difference between two `Modifier`
/// values. This is useful when updating the terminal display, as it allows for more
/// efficient updates by only sending the necessary changes.
struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    fn queue<W: io::Write>(self, w: &mut W) -> io::Result<()> {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}
