//! Inline viewport / scrollback 推入的行为测试。
//!
//! 用一个可编程的 mock backend(codex `CaptureBackend` 同思路)断言:
//! * `Terminal::with_options` 把 viewport 零高度锚定在光标行;
//! * draw 输出包含 MoveTo + 单元格文本;
//! * `insert_history_lines` 发出 DECSTBM scroll region 序列,把内容写到
//!   viewport 上方,并把 viewport 下移(腾出空间时)。

use std::io;
use std::io::Write;

use grodex_tui::custom_terminal::Terminal;
use grodex_tui::insert_history::insert_history_lines;
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

/// 收集全部输出字节的 mock backend。
struct MockBackend {
    output: Vec<u8>,
    size: Size,
    cursor: Position,
}

impl MockBackend {
    fn new(width: u16, height: u16, cursor_y: u16) -> Self {
        Self {
            output: Vec::new(),
            size: Size { width, height },
            cursor: Position { x: 0, y: cursor_y },
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
}

impl Write for MockBackend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Backend for MockBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, _content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.output.extend_from_slice(b"\x1b[?25l");
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.output.extend_from_slice(b"\x1b[?25h");
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn clear_region(&mut self, _clear_type: ClearType) -> io::Result<()> {
        Ok(())
    }

    fn append_lines(&mut self, _line_count: u16) -> io::Result<()> {
        Ok(())
    }

    fn scroll_region_up(
        &mut self,
        _region: std::ops::Range<u16>,
        _scroll_by: u16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn scroll_region_down(
        &mut self,
        _region: std::ops::Range<u16>,
        _scroll_by: u16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: self.size,
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn viewport_anchors_at_cursor_row_zero_height() {
    let backend = MockBackend::new(80, 24, /*cursor_y*/ 10);
    let terminal = Terminal::with_options(backend).expect("terminal");
    // inline 模式:viewport 零高度、锚定在光标行(启动前的历史可见)。
    assert_eq!(terminal.viewport_area, Rect::new(0, 10, 0, 0));
}

#[test]
fn draw_renders_text_into_backend_output() {
    let backend = MockBackend::new(80, 24, /*cursor_y*/ 10);
    let mut terminal = Terminal::with_options(backend).expect("terminal");
    terminal.set_viewport_area(Rect::new(0, 10, 80, 5));

    terminal
        .draw(|frame| {
            let area = frame.area();
            Paragraph::new("hello inline").render(area, frame.buffer_mut());
        })
        .expect("draw");

    let out = terminal.backend().output();
    // 中间的空格与默认缓冲相同,diff 会跳过,所以两个词分段出现。
    assert!(out.contains("hello"), "got: {out:?}");
    assert!(out.contains("inline"));
    // 渲染后隐藏光标(未设置 cursor_position)。
    assert!(out.contains("\x1b[?25l"));
}

#[test]
fn insert_history_writes_decstbm_above_viewport() {
    let backend = MockBackend::new(80, 24, /*cursor_y*/ 10);
    let mut terminal = Terminal::with_options(backend).expect("terminal");
    // viewport 驻底:行 14..24,上方 14 行可作 scroll region。
    let viewport = Rect::new(0, 14, 80, 10);
    terminal.set_viewport_area(viewport);
    // 模拟一次 draw,让 last_known_cursor_pos 落在 viewport 内。
    terminal.draw(|frame| {
        frame.set_cursor_position((0, frame.area().height - 1));
    }).expect("draw");

    let lines = vec![Line::from("first"), Line::from("second"), Line::from("third")];
    insert_history_lines(&mut terminal, lines).expect("insert");

    let out = terminal.backend().output();
    // DECSTBM:scroll region = 屏幕顶..viewport 顶(1-based:1..14)。
    assert!(out.contains("\x1b[1;14r"), "expected DECSTBM 1;14r, got: {out:?}");
    assert!(out.contains("\x1b[r"), "expected scroll region reset");
    // 历史行写在 viewport 上方(cursor_top = 13 → CUP \x1b[14;1H)。
    assert!(out.contains("\x1b[14;1H"), "expected cursor at row 13 (1-based 14), got: {out:?}");
    // 三行内容逐行写入(\r\n 分隔)。
    assert!(out.contains("first"), "got: {out:?}");
    assert!(out.contains("second"));
    assert!(out.contains("third"));
    // viewport 未动(已在屏底,无下移)。
    assert_eq!(terminal.viewport_area, viewport);
    // 光标回到 last_known_cursor_pos(draw 时设置的 (0, viewport 高-1))。
    assert_eq!(terminal.last_known_cursor_pos, Position { x: 0, y: 9 });
}

#[test]
fn insert_history_moves_viewport_down_when_not_at_bottom() {
    let backend = MockBackend::new(80, 24, /*cursor_y*/ 10);
    let mut terminal = Terminal::with_options(backend).expect("terminal");
    // viewport 不在屏底:行 10..20,屏高 24,底部还有 4 行。
    let viewport = Rect::new(0, 10, 80, 10);
    terminal.set_viewport_area(viewport);

    // 插 2 行:viewport 应下移 2(y: 10 → 12)腾出写入空间。
    let lines = vec![Line::from("alpha"), Line::from("beta")];
    insert_history_lines(&mut terminal, lines).expect("insert");

    assert_eq!(terminal.viewport_area.y, 12, "viewport should move down by 2");
    assert_eq!(terminal.viewport_area.height, 10);
    // viewport 下移用 RI(ESC M)+ 临时 scroll region。
    let out = terminal.backend().output();
    assert!(out.contains("\x1bM"), "expected RI sequence, got: {out:?}");
    assert!(out.contains("\x1b[11;24r"), "expected temp scroll region 11..24, got: {out:?}");
    assert!(out.contains("alpha"));
}

#[test]
fn insert_history_preserves_line_style() {
    let backend = MockBackend::new(80, 24, /*cursor_y*/ 10);
    let mut terminal = Terminal::with_options(backend).expect("terminal");
    terminal.set_viewport_area(Rect::new(0, 14, 80, 10));

    let styled: Line<'static> = Line::from("colored").style(ratatui::style::Color::Green);
    insert_history_lines(&mut terminal, vec![styled]).expect("insert");

    let out = terminal.backend().output();
    // 绿色前景 SGR(32) 出现在输出中。
    assert!(out.contains("\x1b[32m") || out.contains("\x1b[38;"), "expected green fg SGR, got: {out:?}");
    assert!(out.contains("colored"));
}

#[test]
fn insert_history_wraps_long_lines_to_width() {
    let backend = MockBackend::new(20, 24, /*cursor_y*/ 10);
    let mut terminal = Terminal::with_options(backend).expect("terminal");
    terminal.set_viewport_area(Rect::new(0, 14, 20, 10));

    // 宽 20 的终端,一行 45 字符 → 折成 3 行。
    let long = "abcdefghij klmnopqrst uvwxyz abcd efghij klmno";
    insert_history_lines(&mut terminal, vec![Line::from(long)]).expect("insert");

    let out = terminal.backend().output();
    for word_part in ["abcdefghij", "klmno"] {
        assert!(out.contains(word_part), "expected {word_part} in wrapped output, got: {out:?}");
    }
}

#[test]
fn set_viewport_area_clamps_visible_history_rows() {
    let backend = MockBackend::new(80, 24, /*cursor_y*/ 10);
    let mut terminal = Terminal::with_options(backend).expect("terminal");
    terminal.set_viewport_area(Rect::new(0, 14, 80, 10));
    // 无法直接读私有字段;通过再次 set_viewport_area 收缩不 panic 即可。
    terminal.set_viewport_area(Rect::new(0, 20, 80, 4));
    assert_eq!(terminal.viewport_area, Rect::new(0, 20, 80, 4));
}

#[test]
fn styled_line_render_smoke() {
    // render 层使用的 Frame 兼容方法在 mock backend 上可用。
    let backend = MockBackend::new(40, 10, /*cursor_y*/ 5);
    let mut terminal = Terminal::with_options(backend).expect("terminal");
    terminal.set_viewport_area(Rect::new(0, 5, 40, 5));
    terminal
        .draw(|frame| {
            Paragraph::new(Line::from("styled".bold())).render(frame.area(), frame.buffer_mut());
            frame.set_cursor_position((3, 4));
        })
        .expect("draw");
    let out = terminal.backend().output();
    assert!(out.contains("styled"));
    // set_cursor_position → show cursor + 记录光标位置(mock 不发 CUP 字节)。
    assert!(out.contains("\x1b[?25h"), "expected cursor shown: {out:?}");
    assert_eq!(terminal.last_known_cursor_pos, Position { x: 3, y: 4 });
}
