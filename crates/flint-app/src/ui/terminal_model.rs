//! The pure parts of a shell tab (#14; UP-41, UP-42): what it runs, how big its grid is for a
//! pixel size, which cell a pointer is over, how a wheel turns into lines, how a screen row
//! splits into runs of text to shape, and the state's wording. No GPUI types, so all of it is
//! testable without a window; the view (`terminal_view.rs`) draws with them.

use flint_core::terminal::{Cell, CellFlags, CursorShape, SelectionKind, Side, TerminalSize};
use flint_core::{ClusterId, ExecRequest, ExecState};

use super::log_model::Tone;

/// A row is this many times the font size tall.
pub const LINE_HEIGHT_RATIO: f32 = 1.3;
/// Space between the grid and the tab's edges, in pixels.
pub const PADDING: f32 = 4.;

/// What a shell tab runs: a shell in one container of a pod. Opening the same request again
/// shows the open tab while its command runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShellRequest {
    pub cluster: ClusterId,
    pub namespace: String,
    pub pod: String,
    /// `None` lets the API server pick, which works when the pod has one container.
    pub container: Option<String>,
}

impl ShellRequest {
    /// The core request: `bash`, else `/bin/sh` (the core's default), at `size`.
    pub fn exec_request(&self, size: TerminalSize) -> ExecRequest {
        let mut request =
            ExecRequest::shell(self.cluster.clone(), &self.namespace, &self.pod).size(size);
        if let Some(container) = &self.container {
            request = request.container(container.clone());
        }
        request
    }

    /// `Shell: web-1 · app`, the tab's title: the pod and container, as a log tab names them.
    pub fn title(&self) -> String {
        match &self.container {
            Some(container) => format!("Shell: {} · {container}", self.pod),
            None => format!("Shell: {}", self.pod),
        }
    }

    /// What the tab's tooltip says.
    pub fn tooltip(&self) -> String {
        let mut text = format!("Shell in {}/{}", self.namespace, self.pod);
        if let Some(container) = &self.container {
            text.push_str(&format!(", container {container}"));
        }
        text.push_str(&format!("\non {}", self.cluster));
        text
    }
}

/// One cell's size in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellSize {
    pub width: f32,
    pub height: f32,
}

impl CellSize {
    /// A cell of a monospace font `font_size` big whose glyphs advance `advance`.
    pub fn new(advance: f32, font_size: f32) -> Self {
        Self {
            width: advance.max(1.),
            height: (font_size * LINE_HEIGHT_RATIO).round().max(1.),
        }
    }
}

/// The grid that fits an area `width` by `height` pixels (padding included), at least the
/// smallest terminal ([`TerminalSize::new`]).
pub fn grid_size(width: f32, height: f32, cell: CellSize) -> TerminalSize {
    let fit = |space: f32, step: f32| -> u16 {
        let n = ((space - 2. * PADDING) / step).floor();
        if n.is_finite() && n > 0. {
            n.min(f32::from(u16::MAX)) as u16
        } else {
            0
        }
    };
    TerminalSize::new(fit(width, cell.width), fit(height, cell.height))
}

/// The screen cell under a point `x`, `y` pixels from the grid's top-left corner, and which half
/// of it: what a selection starts or ends on. Points off the grid are clamped to its edge.
pub fn cell_at(x: f32, y: f32, cell: CellSize, size: TerminalSize) -> (u16, u16, Side) {
    let clamp = |v: f32, step: f32, count: u16| -> (u16, f32) {
        let pos = v / step;
        if pos < 0. {
            (0, 0.)
        } else if pos >= f32::from(count) {
            (count - 1, 1.)
        } else {
            (pos.floor() as u16, pos.fract())
        }
    };
    let (row, _) = clamp(y, cell.height, size.rows);
    let (col, within) = clamp(x, cell.width, size.cols);
    let side = if within < 0.5 {
        Side::Left
    } else {
        Side::Right
    };
    (row, col, side)
}

/// How a click (and the drag after it) selects: cells, whole words on a double click, whole
/// lines on a triple click, a rectangle with Option (Alt) held.
pub fn selection_kind(click_count: usize, alt: bool) -> SelectionKind {
    match click_count {
        _ if alt => SelectionKind::Block,
        2 => SelectionKind::Word,
        n if n >= 3 => SelectionKind::Lines,
        _ => SelectionKind::Simple,
    }
}

/// Turns wheel and trackpad movement into whole lines, keeping the fraction for next time so
/// slow trackpad scrolling still moves.
#[derive(Debug, Default)]
pub struct WheelLines {
    remainder: f32,
}

impl WheelLines {
    /// The lines to scroll for `dy` pixels (positive: up into the history) at `line_height`.
    pub fn lines(&mut self, dy: f32, line_height: f32) -> i32 {
        let total = self.remainder + dy / line_height;
        let whole = total.trunc();
        self.remainder = total - whole;
        whole as i32
    }

    pub fn reset(&mut self) {
        self.remainder = 0.;
    }
}

/// The most a single paste sends to a shell. The session's input queue is unbounded (#152), and
/// a shell reads a paste as typed input, so a huge clipboard is refused instead of queued.
pub const MAX_PASTE_BYTES: usize = 256 * 1024;

/// What pasting a text into a shell takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteCheck {
    /// Send it: one line.
    Paste,
    /// Send it only if the program takes pastes bracketed (the core checks its live mode,
    /// `ExecSession::paste_if_bracketed`); otherwise ask first, since each of its `lines`
    /// lines would run as it arrives.
    Lines { lines: usize },
    /// Refuse it: it holds `bytes` bytes, more than [`MAX_PASTE_BYTES`].
    TooLarge { bytes: usize },
}

/// What pasting `text` takes. A line break at the very end does not count as another line.
pub fn paste_check(text: &str) -> PasteCheck {
    if text.len() > MAX_PASTE_BYTES {
        return PasteCheck::TooLarge { bytes: text.len() };
    }
    let lines = text
        .trim_end_matches(['\r', '\n'])
        .split("\r\n")
        .flat_map(|line| line.split(['\r', '\n']))
        .count();
    if lines > 1 {
        PasteCheck::Lines { lines }
    } else {
        PasteCheck::Paste
    }
}

/// What the paste confirmation says.
pub fn paste_question(lines: usize) -> (String, String) {
    (
        format!("Paste {lines} lines?"),
        "Each line runs as it is pasted: this shell did not ask for bracketed paste, so it \
         cannot tell a paste from typing."
            .to_owned(),
    )
}

/// Why a paste was refused.
pub fn paste_too_large(bytes: usize) -> String {
    format!(
        "Not pasted: the clipboard holds {}, more than the {} a shell tab pastes at once.",
        byte_size(bytes),
        byte_size(MAX_PASTE_BYTES)
    )
}

/// `512 bytes`, `256 KB`, `3.2 MB`.
fn byte_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} bytes")
    }
}

/// How the cursor is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorLook {
    /// A filled block, its character in the cursor's text color.
    Block,
    /// A block's outline.
    Hollow,
    Beam {
        faded: bool,
    },
    Underline {
        faded: bool,
    },
}

/// The cursor for the program's `shape`: solid while the tab has focus, a hollow block or a
/// faded beam or underline without it, and none once the session ended (nothing more can be
/// typed, so a cursor would only suggest otherwise).
pub fn cursor_look(shape: CursorShape, focused: bool, live: bool) -> Option<CursorLook> {
    if !live {
        return None;
    }
    match shape {
        CursorShape::Block if focused => Some(CursorLook::Block),
        CursorShape::Block | CursorShape::HollowBlock => Some(CursorLook::Hollow),
        CursorShape::Beam => Some(CursorLook::Beam { faded: !focused }),
        CursorShape::Underline => Some(CursorLook::Underline { faded: !focused }),
        CursorShape::Hidden => None,
    }
}

/// The state in a few words, and the tone to draw it in.
pub fn state_text(state: &ExecState) -> (Tone, String) {
    match state {
        ExecState::Connecting => (Tone::Muted, "Connecting…".to_owned()),
        ExecState::Running { command } => (Tone::Live, format!("Running {}", command.join(" "))),
        ExecState::Exited { code: Some(0) } => (Tone::Muted, "Exited (code 0)".to_owned()),
        ExecState::Exited { code: Some(code) } => (Tone::Warning, format!("Exited (code {code})")),
        ExecState::Exited { code: None } => (Tone::Muted, "Exited".to_owned()),
        // Why is said once, in the tab's banner ([`failure_words`]).
        ExecState::Failed(_) => (Tone::Danger, "Failed".to_owned()),
        ExecState::Closed => (Tone::Muted, "Closed".to_owned()),
    }
}

/// Why a shell could not run, in plain words, and what the server (or the connection) said, as
/// the detail: the tab's banner says both, once. Common exec failures get a sentence of their
/// own; any other keeps the server's text after a plain lead.
pub fn failure_words(reason: &str) -> (&'static str, String) {
    let lower = reason.to_lowercase();
    let has = |needle: &str| lower.contains(needle);
    let headline = if has("executable file not found") || has("no such file or directory") {
        "Neither bash nor sh is in this container."
    } else if has("container not found")
        || has("not running")
        || has("completed pod")
        || has("is waiting to start")
        || has("containercreating")
    {
        "The container is not running, so a shell can't start in it."
    } else if lower.starts_with("forbidden") || has("is forbidden") {
        "You don't have permission to open a shell in this pod."
    } else if lower.starts_with("notfound") || has("not found") {
        "The pod was not found, or the server can't open a shell in it."
    } else if lower.starts_with("the exec connection broke") {
        "The connection to the shell broke."
    } else {
        "The shell could not start."
    };
    (headline, reason.trim().to_owned())
}

/// A piece of a screen row shaped and drawn as one line of text from column `col`. Every
/// character in it takes one cell, so a monospace grid stays aligned; a wide character (two
/// cells) or one with combining marks gets a segment of its own.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment<S> {
    pub col: u16,
    pub text: String,
    /// Consecutive runs of the text, in bytes, with their style.
    pub runs: Vec<(usize, S)>,
    /// A wide character or one with combining marks: drawn at its natural width.
    pub isolated: bool,
}

/// Splits a screen row into [`Segment`]s. `styles[i]` is cell `i`'s style; `blank` says whether
/// a cell draws nothing (a space without underline), so text is broken there instead of shaped.
pub fn row_segments<S: Clone + PartialEq>(
    row: &[Cell],
    styles: &[S],
    blank: impl Fn(&Cell, &S) -> bool,
) -> Vec<Segment<S>> {
    let mut segments = Vec::new();
    let mut current: Option<Segment<S>> = None;
    let flush = |current: &mut Option<Segment<S>>, segments: &mut Vec<Segment<S>>| {
        if let Some(segment) = current.take() {
            segments.push(segment);
        }
    };
    for (ix, (cell, style)) in row.iter().zip(styles).enumerate() {
        if cell
            .flags
            .intersects(CellFlags::WIDE_CHAR_SPACER | CellFlags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        if blank(cell, style) {
            flush(&mut current, &mut segments);
            continue;
        }
        let col = ix as u16;
        let isolated = cell.flags.contains(CellFlags::WIDE_CHAR) || cell.zerowidth.is_some();
        let mut text = String::from(cell.c);
        if let Some(extra) = &cell.zerowidth {
            text.extend(extra.iter());
        }
        if isolated {
            flush(&mut current, &mut segments);
            segments.push(Segment {
                col,
                runs: vec![(text.len(), style.clone())],
                text,
                isolated: true,
            });
            continue;
        }
        let segment = current.get_or_insert_with(|| Segment {
            col,
            text: String::new(),
            runs: Vec::new(),
            isolated: false,
        });
        segment.text.push_str(&text);
        match segment.runs.last_mut() {
            Some((len, last)) if last == style => *len += text.len(),
            _ => segment.runs.push((text.len(), style.clone())),
        }
    }
    flush(&mut current, &mut segments);
    segments
}

/// Consecutive cells of a row that share a value (a background color), as (first column,
/// cell count, value); cells without one are skipped.
pub fn spans<T: PartialEq + Copy>(values: &[Option<T>]) -> Vec<(u16, u16, T)> {
    let mut spans: Vec<(u16, u16, T)> = Vec::new();
    for (ix, value) in values.iter().enumerate() {
        let Some(value) = *value else { continue };
        match spans.last_mut() {
            Some((start, len, last)) if *last == value && usize::from(*start + *len) == ix => {
                *len += 1;
            }
            _ => spans.push((ix as u16, 1, value)),
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use flint_core::terminal::{Color, NamedColor};

    use super::*;

    const CELL: CellSize = CellSize {
        width: 8.,
        height: 16.,
    };

    fn cell(c: char) -> Cell {
        Cell {
            c,
            zerowidth: None,
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: CellFlags::empty(),
            selected: false,
        }
    }

    fn row(text: &str) -> Vec<Cell> {
        text.chars().map(cell).collect()
    }

    #[test]
    fn requests_name_the_container_and_build_the_core_request() {
        let request = ShellRequest {
            cluster: "kind".into(),
            namespace: "demo".into(),
            pod: "web-1".into(),
            container: Some("app".into()),
        };
        assert_eq!(request.title(), "Shell: web-1 · app");
        assert_eq!(
            request.tooltip(),
            "Shell in demo/web-1, container app\non kind"
        );
        let exec = request.exec_request(TerminalSize::new(100, 30));
        assert_eq!(exec.container.as_deref(), Some("app"));
        assert_eq!(exec.size, TerminalSize::new(100, 30));
        assert_eq!(exec.command, None, "bash, then /bin/sh");
        let any = ShellRequest {
            container: None,
            ..request
        };
        assert_eq!(any.title(), "Shell: web-1");
        assert_eq!(any.exec_request(TerminalSize::default()).container, None);
    }

    // covers UP-41: the terminal resizes with the window.
    #[test]
    fn the_grid_fills_the_area_less_its_padding() {
        assert_eq!(
            CellSize::new(7.2, 12.),
            CellSize {
                width: 7.2,
                height: 16.
            }
        );
        // 808 - 8 = 800 wide: 100 columns; 400 - 8 = 392 high: 24 rows.
        assert_eq!(grid_size(808., 400., CELL), TerminalSize::new(100, 24));
        assert_eq!(grid_size(815.9, 407.9, CELL), TerminalSize::new(100, 24));
        // Too small (or not laid out yet): the smallest terminal.
        assert_eq!(grid_size(0., 0., CELL), TerminalSize::new(2, 1));
        assert_eq!(grid_size(f32::NAN, 10., CELL), TerminalSize::new(2, 1));
    }

    // covers UP-41: dragging selects the cells under the pointer.
    #[test]
    fn points_map_to_cells_and_their_halves() {
        let size = TerminalSize::new(10, 5);
        assert_eq!(cell_at(0., 0., CELL, size), (0, 0, Side::Left));
        assert_eq!(cell_at(11., 17., CELL, size), (1, 1, Side::Left));
        assert_eq!(cell_at(13., 17., CELL, size), (1, 1, Side::Right));
        // Off the grid: clamped to the nearest edge cell.
        assert_eq!(cell_at(-5., -5., CELL, size), (0, 0, Side::Left));
        assert_eq!(cell_at(500., 500., CELL, size), (4, 9, Side::Right));
    }

    #[test]
    fn the_cursor_follows_focus_and_goes_once_the_session_ends() {
        use CursorShape::*;
        assert_eq!(cursor_look(Block, true, true), Some(CursorLook::Block));
        assert_eq!(cursor_look(Block, false, true), Some(CursorLook::Hollow));
        assert_eq!(
            cursor_look(Beam, false, true),
            Some(CursorLook::Beam { faded: true })
        );
        assert_eq!(cursor_look(Hidden, true, true), None);
        for shape in [Block, HollowBlock, Beam, Underline] {
            assert_eq!(cursor_look(shape, true, false), None, "{shape:?}");
        }
    }

    // covers UP-41: several lines pasted into a shell without bracketed paste would each run,
    // so they are confirmed first; a huge clipboard is refused.
    #[test]
    fn pastes_are_checked_before_they_run() {
        assert_eq!(paste_check("ls -la"), PasteCheck::Paste);
        assert_eq!(paste_check("ls -la\n"), PasteCheck::Paste);
        assert_eq!(
            paste_check("echo a\necho b\r\necho c\n"),
            PasteCheck::Lines { lines: 3 }
        );
        assert_eq!(paste_check("a\rb"), PasteCheck::Lines { lines: 2 });
        let huge = "x".repeat(MAX_PASTE_BYTES + 1);
        assert_eq!(
            paste_check(&huge),
            PasteCheck::TooLarge {
                bytes: MAX_PASTE_BYTES + 1
            }
        );
        assert_eq!(paste_question(3).0, "Paste 3 lines?");
        assert_eq!(
            paste_too_large(3 * 1024 * 1024 + 200 * 1024),
            "Not pasted: the clipboard holds 3.2 MB, more than the 256 KB a shell tab pastes at \
             once."
        );
        assert_eq!(byte_size(512), "512 bytes");
    }

    #[test]
    fn clicks_choose_how_a_selection_grows() {
        assert_eq!(selection_kind(1, false), SelectionKind::Simple);
        assert_eq!(selection_kind(2, false), SelectionKind::Word);
        assert_eq!(selection_kind(3, false), SelectionKind::Lines);
        assert_eq!(selection_kind(4, false), SelectionKind::Lines);
        assert_eq!(selection_kind(1, true), SelectionKind::Block);
    }

    #[test]
    fn the_wheel_scrolls_whole_lines_and_keeps_the_rest() {
        let mut wheel = WheelLines::default();
        assert_eq!(wheel.lines(40., 16.), 2);
        // 8 px left over plus 10 more: one more line.
        assert_eq!(wheel.lines(10., 16.), 1);
        assert_eq!(wheel.lines(-32., 16.), -1);
        wheel.reset();
        assert_eq!(wheel.lines(15., 16.), 0);
    }

    // covers UP-41: the tab says whether the shell connects, runs, exited (with its code) or
    // failed (and why).
    #[test]
    fn states_read_well() {
        assert_eq!(
            state_text(&ExecState::Connecting),
            (Tone::Muted, "Connecting…".into())
        );
        assert_eq!(
            state_text(&ExecState::Running {
                command: vec!["/bin/sh".into()]
            }),
            (Tone::Live, "Running /bin/sh".into())
        );
        assert_eq!(
            state_text(&ExecState::Exited { code: Some(0) }),
            (Tone::Muted, "Exited (code 0)".into())
        );
        assert_eq!(
            state_text(&ExecState::Exited { code: Some(130) }),
            (Tone::Warning, "Exited (code 130)".into())
        );
        // Why it failed is said once, in the banner, not again beside the state.
        assert_eq!(
            state_text(&ExecState::Failed("pods \"x\" not found".into())),
            (Tone::Danger, "Failed".into())
        );
        assert_eq!(state_text(&ExecState::Closed).1, "Closed");
    }

    // covers UP-41: common exec failures read in plain words, the server's text kept as the
    // detail.
    #[test]
    fn exec_failures_read_in_plain_words() {
        let words = |reason: &str| failure_words(reason).0;
        assert_eq!(
            words("Forbidden: pods \"web\" is forbidden: User \"jane\" cannot create pods/exec"),
            "You don't have permission to open a shell in this pod."
        );
        assert_eq!(
            words("NotFound: subresource exec is not recorded"),
            "The pod was not found, or the server can't open a shell in it."
        );
        assert_eq!(
            words("unable to upgrade connection: container not found (\"app\")"),
            "The container is not running, so a shell can't start in it."
        );
        assert_eq!(
            words(
                "BadRequest: cannot exec into a container in a completed pod; current phase is Succeeded"
            ),
            "The container is not running, so a shell can't start in it."
        );
        assert_eq!(
            words("exec: \"/bin/sh\": stat /bin/sh: no such file or directory"),
            "Neither bash nor sh is in this container."
        );
        assert_eq!(
            words("the exec connection broke: reset"),
            "The connection to the shell broke."
        );
        assert_eq!(
            failure_words("InternalError: boom"),
            (
                "The shell could not start.",
                "InternalError: boom".to_owned()
            )
        );
    }

    fn blank(cell: &Cell, _: &u8) -> bool {
        cell.c == ' '
    }

    // covers UP-41: colored output keeps its columns.
    #[test]
    fn rows_split_into_styled_runs_broken_at_blanks() {
        let cells = row("ls  -la");
        let styles = [1u8, 1, 0, 0, 2, 2, 3];
        let segments = row_segments(&cells, &styles, blank);
        assert_eq!(
            segments,
            vec![
                Segment {
                    col: 0,
                    text: "ls".into(),
                    runs: vec![(2, 1)],
                    isolated: false
                },
                Segment {
                    col: 4,
                    text: "-la".into(),
                    runs: vec![(2, 2), (1, 3)],
                    isolated: false
                },
            ]
        );
    }

    // covers UP-41: wide characters take two cells and the text after them stays aligned.
    #[test]
    fn wide_and_combining_characters_get_segments_of_their_own() {
        let mut cells = row("a界 b");
        cells[1].flags = CellFlags::WIDE_CHAR;
        cells[2] = Cell {
            flags: CellFlags::WIDE_CHAR_SPACER,
            ..cell(' ')
        };
        cells[3] = cell('e');
        cells[3].zerowidth = Some(Box::new(['\u{301}']));
        cells.push(cell('c'));
        let styles = vec![0u8; cells.len()];
        let segments = row_segments(&cells, &styles, blank);
        let shown: Vec<(u16, &str, bool)> = segments
            .iter()
            .map(|s| (s.col, s.text.as_str(), s.isolated))
            .collect();
        assert_eq!(
            shown,
            vec![
                (0, "a", false),
                (1, "界", true),
                (3, "e\u{301}", true),
                (4, "c", false)
            ]
        );
        assert_eq!(segments[1].runs, vec![("界".len(), 0)]);
    }

    #[test]
    fn spans_merge_adjacent_equal_values() {
        let values = [None, Some(1), Some(1), Some(2), None, Some(2), Some(2)];
        assert_eq!(spans(&values), vec![(1, 2, 1), (3, 1, 2), (5, 2, 2)]);
        assert!(spans::<u8>(&[None, None]).is_empty());
    }
}
