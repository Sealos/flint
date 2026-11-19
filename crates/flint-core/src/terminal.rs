//! The terminal model behind an exec session: an `alacritty_terminal` grid fed with what a pod's
//! TTY prints, and immutable snapshots of it for a front end to draw.
//!
//! The model lives in the core rather than in the app so that parsing a burst of output (`cat` of
//! a large file) never runs on the UI thread, the grid survives a closed or rebuilt view, replies
//! the terminal owes the remote program (cursor position reports, device attributes) go straight
//! back to the pod without a round trip through the UI, and all of it is testable without a
//! window. Front ends get a [`TerminalSnapshot`] of the visible screen (ADR-0002) and send
//! scroll, selection, and input through the session. Nothing here knows about GPUI.
//!
//! Colors are left as the terminal reports them ([`Color`]): named and indexed colors are mapped
//! to the theme by the front end, except where the remote program redefined them (OSC 4), which
//! the snapshot resolves to RGB.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config, Osc52, Term};
use alacritty_terminal::vte::ansi::Processor;

pub use alacritty_terminal::index::Side;
pub use alacritty_terminal::term::TermMode;
pub use alacritty_terminal::term::cell::Flags as CellFlags;
pub use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Rgb};

/// Lines kept above the screen for scrolling back.
pub const SCROLLBACK_LINES: usize = 10_000;

/// A terminal's size in character cells. Both sides are at least 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    /// A size of `cols` by `rows`, with zero raised to 1 (a window can be laid out that small).
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

impl Default for TerminalSize {
    /// 80 by 24, the classic terminal size.
    fn default() -> Self {
        Self::new(80, 24)
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        usize::from(self.rows)
    }

    fn columns(&self) -> usize {
        usize::from(self.cols)
    }
}

/// One visible cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub c: char,
    /// Combining characters drawn on top of `c`.
    pub zerowidth: Option<Box<[char]>>,
    pub fg: Color,
    pub bg: Color,
    /// Bold, italic, underline, inverse, wide characters and their spacers, and so on.
    pub flags: CellFlags,
    /// Inside the current selection.
    pub selected: bool,
}

/// Where the cursor is drawn, in screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
}

/// The visible screen at one moment. Immutable; shared with front ends.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSnapshot {
    pub size: TerminalSize,
    /// `size.rows * size.cols` cells, row by row from the top of the screen.
    pub cells: Vec<Cell>,
    /// `None` while the program hides the cursor or the view is scrolled away from it.
    pub cursor: Option<Cursor>,
    /// How many lines the view is scrolled up into the history (0: at the bottom, following
    /// output).
    pub display_offset: usize,
    /// Lines of history above the screen.
    pub history_lines: usize,
    /// Terminal modes the front end needs to encode input: application cursor keys, bracketed
    /// paste, mouse reporting, the alternate screen.
    pub mode: TermMode,
    /// The title the remote program set (OSC 0/2), if any.
    pub title: Option<String>,
    /// How many times the bell rang; a front end can flash when it changes.
    pub bells: u64,
}

impl TerminalSnapshot {
    /// The cells of screen row `row` (0 is the top). Panics when `row` is off the screen.
    pub fn row(&self, row: u16) -> &[Cell] {
        let cols = usize::from(self.size.cols);
        let start = usize::from(row) * cols;
        &self.cells[start..start + cols]
    }

    /// The text of screen row `row`, without trailing blanks or wide-character spacers.
    pub fn row_text(&self, row: u16) -> String {
        let mut text = String::new();
        for cell in self.row(row) {
            if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cell.c);
            if let Some(extra) = &cell.zerowidth {
                text.extend(extra.iter());
            }
        }
        text.truncate(text.trim_end().len());
        text
    }

    /// The whole screen as text, one line per row, for search, accessibility, and tests.
    pub fn text(&self) -> String {
        (0..self.size.rows)
            .map(|r| self.row_text(r))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// How a selection grows from where it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    /// Cell by cell (a drag).
    Simple,
    /// Whole words (a double click).
    Word,
    /// Whole lines (a triple click).
    Lines,
    /// A rectangle (an Option-drag).
    Block,
}

impl SelectionKind {
    fn to_alacritty(self) -> SelectionType {
        match self {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Lines => SelectionType::Lines,
            SelectionKind::Block => SelectionType::Block,
        }
    }
}

/// Collects what the terminal asks of its host while it parses.
#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Vec<TermEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        if let Ok(mut events) = self.0.lock() {
            events.push(event);
        }
    }
}

/// A terminal emulator without a display: bytes in, screen snapshots and replies out.
pub struct TerminalModel {
    term: Term<Listener>,
    parser: Processor,
    events: Listener,
    size: TerminalSize,
    title: Option<String>,
    bells: u64,
}

impl TerminalModel {
    pub fn new(size: TerminalSize) -> Self {
        // The fields are public: a struct literal can hold zeros.
        let size = TerminalSize::new(size.cols, size.rows);
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            // A remote program must not read the user's clipboard, nor fill it unasked.
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let events = Listener::default();
        Self {
            term: Term::new(config, &size, events.clone()),
            parser: Processor::new(),
            events,
            size,
            title: None,
            bells: 0,
        }
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// The modes the remote program has set.
    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Feeds output from the remote program. Returns the bytes the terminal must send back, such
    /// as answers to cursor position and device attribute queries (empty almost always).
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        let events = std::mem::take(&mut *self.events.0.lock().unwrap_or_else(|e| e.into_inner()));
        let mut reply = Vec::new();
        for event in events {
            match event {
                TermEvent::PtyWrite(text) => reply.extend_from_slice(text.as_bytes()),
                TermEvent::Title(title) => self.title = Some(title),
                TermEvent::ResetTitle => self.title = None,
                TermEvent::Bell => self.bells += 1,
                // Clipboard access is disabled (see `new`); color and pixel-size queries go
                // unanswered, as on terminals that do not support them.
                _ => {}
            }
        }
        reply
    }

    pub fn resize(&mut self, size: TerminalSize) {
        let size = TerminalSize::new(size.cols, size.rows);
        if size != self.size {
            self.size = size;
            self.term.resize(size);
        }
    }

    /// How many lines the view is scrolled up into the history.
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Scrolls the view by `lines`: positive goes up into the history, negative back down.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// Scrolls back to the bottom, where new output appears.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// The grid point under screen cell (`row`, `col`), clamped to the screen.
    fn point(&self, row: u16, col: u16) -> Point {
        let row = row.min(self.size.rows - 1);
        let col = col.min(self.size.cols - 1);
        let offset = self.term.grid().display_offset() as i32;
        Point::new(Line(i32::from(row) - offset), Column(usize::from(col)))
    }

    /// Starts a selection at screen cell (`row`, `col`), on its `side` half.
    pub fn start_selection(&mut self, kind: SelectionKind, row: u16, col: u16, side: Side) {
        let point = self.point(row, col);
        self.term.selection = Some(Selection::new(kind.to_alacritty(), point, side));
    }

    /// Extends the current selection to screen cell (`row`, `col`). Does nothing without one.
    pub fn update_selection(&mut self, row: u16, col: u16, side: Side) {
        let point = self.point(row, col);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// The selected text, history included, or `None` without a selection.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// The bytes to send for pasting `text`. In bracketed paste mode the text is wrapped so the
    /// program knows it was pasted, with any escape character removed so the paste cannot end
    /// itself early; otherwise line breaks become carriage returns, as typed Return keys would.
    pub fn paste_bytes(&self, text: &str) -> Vec<u8> {
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            let mut out = b"\x1b[200~".to_vec();
            out.extend(text.replace('\x1b', "").bytes());
            out.extend_from_slice(b"\x1b[201~");
            out
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        }
    }

    /// The visible screen.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let content = self.term.renderable_content();
        let colors = content.colors;
        let resolve = |color: Color| -> Color {
            let index = match color {
                Color::Named(named) => named as usize,
                Color::Indexed(index) => usize::from(index),
                Color::Spec(_) => return color,
            };
            colors[index].map_or(color, Color::Spec)
        };
        let offset = content.display_offset;
        let cols = usize::from(self.size.cols);
        let rows = usize::from(self.size.rows);
        let blank = Cell {
            c: ' ',
            zerowidth: None,
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: CellFlags::empty(),
            selected: false,
        };
        let mut cells = vec![blank; cols * rows];
        let selection = content.selection;
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset as i32;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= rows || col >= cols {
                continue;
            }
            let cell = indexed.cell;
            cells[row as usize * cols + col] = Cell {
                c: cell.c,
                zerowidth: cell.zerowidth().map(Box::from),
                fg: resolve(cell.fg),
                bg: resolve(cell.bg),
                flags: cell.flags,
                selected: selection.is_some_and(|s| s.contains(indexed.point)),
            };
        }
        let cursor_row = content.cursor.point.line.0 + offset as i32;
        let cursor = (content.cursor.shape != CursorShape::Hidden
            && (0..rows as i32).contains(&cursor_row))
        .then_some(Cursor {
            row: cursor_row as u16,
            col: content.cursor.point.column.0 as u16,
            shape: content.cursor.shape,
        });
        TerminalSnapshot {
            size: self.size,
            cells,
            cursor,
            display_offset: offset,
            history_lines: self.term.grid().history_size(),
            mode: content.mode,
            title: self.title.clone(),
            bells: self.bells,
        }
    }
}

impl std::fmt::Debug for TerminalModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalModel")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(cols: u16, rows: u16) -> TerminalModel {
        TerminalModel::new(TerminalSize::new(cols, rows))
    }

    #[test]
    fn sizes_are_at_least_one_cell() {
        assert_eq!(TerminalSize::new(0, 0), TerminalSize { cols: 1, rows: 1 });
        assert_eq!(TerminalSize::default(), TerminalSize { cols: 80, rows: 24 });
        // Zeros in a struct literal (a collapsed dock) are raised too.
        let mut term = TerminalModel::new(TerminalSize { cols: 0, rows: 0 });
        assert_eq!(term.size(), TerminalSize::new(1, 1));
        term.resize(TerminalSize { cols: 0, rows: 5 });
        term.start_selection(SelectionKind::Simple, 9, 9, Side::Left);
        assert_eq!(term.snapshot().size, TerminalSize::new(1, 5));
    }

    #[test]
    fn text_lands_on_the_grid_with_the_cursor_after_it() {
        let mut term = model(20, 4);
        assert!(term.advance(b"hello\r\nworld").is_empty());
        let snap = term.snapshot();
        assert_eq!(snap.row_text(0), "hello");
        assert_eq!(snap.row_text(1), "world");
        assert_eq!(snap.cells.len(), 80);
        let cursor = snap.cursor.expect("visible");
        assert_eq!((cursor.row, cursor.col), (1, 5));
        assert_eq!(snap.text(), "hello\nworld\n\n");
    }

    #[test]
    fn sgr_colors_and_attributes_are_kept() {
        let mut term = model(10, 2);
        term.advance(b"\x1b[1;31mR\x1b[0m\x1b[38;2;1;2;3mT\x1b[48;5;200mI");
        let row = term.snapshot().row(0).to_vec();
        assert_eq!(row[0].fg, Color::Named(NamedColor::Red));
        assert!(row[0].flags.contains(CellFlags::BOLD));
        assert_eq!(row[1].fg, Color::Spec(Rgb { r: 1, g: 2, b: 3 }));
        assert!(!row[1].flags.contains(CellFlags::BOLD));
        assert_eq!(row[2].bg, Color::Indexed(200));
    }

    #[test]
    fn redefined_palette_colors_resolve_to_rgb() {
        let mut term = model(10, 2);
        term.advance(b"\x1b]4;1;rgb:ff/80/00\x07\x1b[31mX");
        let snap = term.snapshot();
        assert_eq!(
            snap.row(0)[0].fg,
            Color::Spec(Rgb {
                r: 0xff,
                g: 0x80,
                b: 0
            })
        );
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut term = model(10, 2);
        term.advance("日本".as_bytes());
        let snap = term.snapshot();
        assert!(snap.row(0)[0].flags.contains(CellFlags::WIDE_CHAR));
        assert!(snap.row(0)[1].flags.contains(CellFlags::WIDE_CHAR_SPACER));
        assert_eq!(snap.row_text(0), "日本");
    }

    #[test]
    fn queries_are_answered_with_bytes_for_the_remote_program() {
        let mut term = model(20, 5);
        // Cursor position report: row 2, column 4 (1-based).
        let reply = term.advance(b"\r\nabc\x1b[6n");
        assert_eq!(reply, b"\x1b[2;4R");
    }

    #[test]
    fn title_and_bell_are_tracked() {
        let mut term = model(10, 2);
        term.advance(b"\x1b]2;my pod\x07\x07");
        let snap = term.snapshot();
        assert_eq!(snap.title.as_deref(), Some("my pod"));
        assert_eq!(snap.bells, 1);
    }

    #[test]
    fn clipboard_requests_are_ignored() {
        let mut term = model(10, 2);
        // OSC 52 read request: must not be answered with the clipboard.
        let reply = term.advance(b"\x1b]52;c;?\x07");
        assert!(reply.is_empty());
    }

    #[test]
    fn hidden_cursor_is_none() {
        let mut term = model(10, 2);
        term.advance(b"\x1b[?25l");
        assert!(term.snapshot().cursor.is_none());
        term.advance(b"\x1b[?25h");
        assert!(term.snapshot().cursor.is_some());
    }

    #[test]
    fn output_scrolls_into_history_and_the_view_can_follow() {
        let mut term = model(10, 3);
        for i in 0..10 {
            term.advance(format!("line{i}\r\n").as_bytes());
        }
        let snap = term.snapshot();
        assert_eq!(snap.row_text(0), "line8");
        assert_eq!(snap.history_lines, 8);
        term.scroll(2);
        let snap = term.snapshot();
        assert_eq!(snap.display_offset, 2);
        assert_eq!(snap.row_text(0), "line6");
        assert!(snap.cursor.is_none(), "the cursor row is below the view");
        term.scroll_to_bottom();
        assert_eq!(term.snapshot().row_text(0), "line8");
    }

    #[test]
    fn resize_rewraps_the_screen() {
        let mut term = model(10, 3);
        term.advance(b"abc");
        term.resize(TerminalSize::new(4, 2));
        let snap = term.snapshot();
        assert_eq!(snap.size, TerminalSize::new(4, 2));
        assert_eq!(snap.cells.len(), 8);
        assert_eq!(snap.row_text(0), "abc");
    }

    #[test]
    fn selection_marks_cells_and_copies_text() {
        let mut term = model(20, 3);
        term.advance(b"hello world\r\nsecond");
        term.start_selection(SelectionKind::Simple, 0, 6, Side::Left);
        term.update_selection(1, 2, Side::Right);
        assert_eq!(term.selection_text().as_deref(), Some("world\nsec"));
        let snap = term.snapshot();
        assert!(!snap.row(0)[5].selected);
        assert!(snap.row(0)[6].selected);
        assert!(snap.row(1)[2].selected);
        assert!(!snap.row(1)[3].selected);

        term.start_selection(SelectionKind::Word, 0, 1, Side::Left);
        assert_eq!(term.selection_text().as_deref(), Some("hello"));
        term.start_selection(SelectionKind::Lines, 1, 0, Side::Left);
        // Line selections end with the line break.
        assert_eq!(term.selection_text().as_deref(), Some("second\n"));
        term.clear_selection();
        assert_eq!(term.selection_text(), None);
    }

    #[test]
    fn selection_in_scrolled_view_uses_history() {
        let mut term = model(10, 2);
        for i in 0..5 {
            term.advance(format!("l{i}\r\n").as_bytes());
        }
        // Screen: "l4" and an empty line; history: l0 to l3.
        term.scroll(4);
        let snap = term.snapshot();
        assert_eq!(snap.row_text(0), "l0");
        term.start_selection(SelectionKind::Simple, 0, 0, Side::Left);
        term.update_selection(0, 1, Side::Right);
        assert_eq!(term.selection_text().as_deref(), Some("l0"));
    }

    #[test]
    fn paste_follows_bracketed_paste_mode() {
        let mut term = model(10, 2);
        assert_eq!(term.paste_bytes("a\nb\r\nc"), b"a\rb\rc");
        term.advance(b"\x1b[?2004h");
        assert!(term.mode().contains(TermMode::BRACKETED_PASTE));
        assert_eq!(
            term.paste_bytes("x\x1b[201~y"),
            b"\x1b[200~x[201~y\x1b[201~".to_vec()
        );
    }
}
