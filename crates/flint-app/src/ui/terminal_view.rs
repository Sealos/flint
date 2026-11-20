//! A shell tab in the bottom dock (#14; UP-41, UP-42): an interactive terminal in a pod's
//! container, read from a core [`ExecSession`] and drawn as a grid of monospace cells.
//!
//! The core owns the terminal (ARCHITECTURE.md, "Exec terminal"): it parses the pod's output and
//! publishes at most one [`ExecSnapshot`] per frame, which this view draws as is. Keystrokes,
//! pastes, resizes, scrolling and selections go back through the session.
//!
//! - **Drawing** ([`paint_grid`]): one background quad per run of cells sharing a color, then the
//!   text of each row shaped in [`Segment`]s with every glyph forced to the cell width, so the
//!   grid stays aligned; wide characters and combining marks are shaped on their own. Colors come
//!   from the theme ([`TerminalPalette`]); the cursor is a block, beam or underline in the accent
//!   color; without focus a block is hollow and a beam or underline fades.
//! - **Keys** ([`terminal_keys`]): the key handler encodes what does not type text, using the
//!   modes the program set; text, including what an input method composes, arrives through the
//!   platform text input ([`TerminalInput`]). The terminal's key context passes Tab, Esc, the
//!   arrows and Ctrl combinations on to the shell instead of the app
//!   (`registry::TERMINAL_PASSTHROUGH`).
//! - **Size:** the grid is measured from the element's size and the mono font every frame; a
//!   change is sent to the pod after [`RESIZE_DELAY`] without further change (the first at once).
//! - **Mouse:** dragging selects (double click: words, triple click: lines, Option: a block);
//!   the wheel scrolls the history, or sends arrow keys to full-screen programs that ask for it.
//! - **After the command ends** the screen stays scrollable and copyable until the tab closes;
//!   closing the tab drops the session, which ends the command.
//!
//! Callbacks scheduled from here (the snapshot pump, timers, deferred resizes, the text input)
//! hold the view weakly, so a closed tab is freed at once.

use std::cell::Cell as StdCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use flint_core::terminal::{Cell, CursorShape, SelectionKind, Side, TermMode, TerminalSize};
use flint_core::{ExecSession, ExecSnapshot};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _, Theme};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dock::{DockItem, TabKey, TabStatus, cluster_chip};
use super::dock_model;
use super::log_model::Tone;
use super::log_view::Note;
use super::notice::{notice, notice_with};
use super::registry::{self, CopyText, PasteText, RestartShell, ScrollToBottom};
use super::terminal_keys::{self, ALT_IS_META};
use super::terminal_model::{
    self, CellSize, CursorLook, PADDING, Segment, ShellRequest, WheelLines, cell_at, cursor_look,
    row_segments, spans,
};
use super::terminal_palette::{CellStyle, TerminalPalette, cell_style};

/// How long the grid's size must stay the same before the pod is told (dragging the dock's edge
/// resizes on every frame).
pub const RESIZE_DELAY: Duration = Duration::from_millis(80);
/// How long the bell flashes the screen.
const BELL_FLASH: Duration = Duration::from_millis(150);

/// Where the grid was laid out in the last frame: what turns a pointer into a cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridLayout {
    /// The top-left cell's corner, in window coordinates.
    pub origin: Point<Pixels>,
    pub cell: CellSize,
    /// The grid that fits.
    pub size: TerminalSize,
}

impl GridLayout {
    /// The screen cell under `position` (window coordinates), and which half of it.
    pub fn cell_at(&self, position: Point<Pixels>, size: TerminalSize) -> (u16, u16, Side) {
        let x = f32::from(position.x - self.origin.x);
        let y = f32::from(position.y - self.origin.y);
        cell_at(x, y, self.cell, size)
    }

    /// Cell (`row`, `col`)'s bounds in window coordinates.
    pub fn cell_bounds(&self, row: u16, col: u16, width: u16) -> Bounds<Pixels> {
        Bounds::new(
            point(
                self.origin.x + px(f32::from(col) * self.cell.width),
                self.origin.y + px(f32::from(row) * self.cell.height),
            ),
            size(px(f32::from(width) * self.cell.width), px(self.cell.height)),
        )
    }
}

/// A left-button press in the grid, until it is released.
#[derive(Debug, Clone, Copy)]
struct Press {
    row: u16,
    col: u16,
    side: Side,
    kind: SelectionKind,
    /// A selection started (at once for words, lines and Shift-click; on the first move for a
    /// plain drag, so a click alone selects nothing).
    started: bool,
}

pub struct TerminalView {
    request: ShellRequest,
    /// Dropping the view drops the session, which ends the command.
    session: ExecSession,
    /// The snapshot drawn.
    snapshot: Arc<ExecSnapshot>,
    /// Written by the grid while it paints, read by the mouse and text input handlers.
    layout: Rc<StdCell<Option<GridLayout>>>,
    /// The grid size the session has (asked for last).
    size: TerminalSize,
    /// A size waiting out [`RESIZE_DELAY`].
    pending_size: Option<TerminalSize>,
    resized: bool,
    wheel: WheelLines,
    press: Option<Press>,
    /// A selection is active (the core holds it; this says whether Copy has something).
    selection: bool,
    /// Text an input method is composing, drawn at the cursor until it is committed.
    marked: Option<String>,
    /// The bell rang a moment ago.
    bell: bool,
    note: Option<Note>,
    /// Why the tab ended the session itself ([`TerminalView::end`]).
    ended: Option<String>,
    /// The toolbar's cluster chip, as the dock names the tab's cluster: its label, its tooltip
    /// (the full context and server), and whether the context points elsewhere now.
    chip: (SharedString, SharedString, bool),
    focus: FocusHandle,
    _pump: Task<()>,
    _resize: Task<()>,
    _bell: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl TerminalView {
    pub fn new(
        session: ExecSession,
        request: ShellRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut updates = session.updates();
        let pump = cx.spawn(async move |this, cx| {
            while updates.changed().await {
                let snapshot = updates.latest();
                if this
                    .update(cx, |view, cx| view.set_snapshot(snapshot, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let focus = cx.focus_handle();
        // The cursor is solid only while the tab has focus.
        let subscriptions = vec![
            cx.on_focus(&focus, window, |_, _, cx| cx.notify()),
            cx.on_blur(&focus, window, |_, _, cx| cx.notify()),
        ];
        let chip = (
            dock_model::chip_labels(std::slice::from_ref(&request.cluster))
                .remove(&request.cluster)
                .unwrap_or_default()
                .into(),
            format!("Context {}", request.cluster).into(),
            false,
        );
        TerminalView {
            size: session.request().size,
            snapshot: session.peek(),
            session,
            request,
            layout: Rc::default(),
            pending_size: None,
            resized: false,
            wheel: WheelLines::default(),
            press: None,
            selection: false,
            marked: None,
            bell: false,
            note: None,
            ended: None,
            chip,
            focus,
            _pump: pump,
            _resize: Task::ready(()),
            _bell: Task::ready(()),
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    pub fn request(&self) -> &ShellRequest {
        &self.request
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> &Arc<ExecSnapshot> {
        &self.snapshot
    }

    /// The grid as laid out in the last frame.
    #[cfg(test)]
    pub fn layout(&self) -> Option<GridLayout> {
        self.layout.get()
    }

    /// Takes in a newer snapshot: flashes for a bell, and forgets input state once the command
    /// has ended.
    fn set_snapshot(&mut self, snapshot: Arc<ExecSnapshot>, cx: &mut Context<Self>) {
        if snapshot.terminal.bells > self.snapshot.terminal.bells {
            self.ring(cx);
        }
        if snapshot.state.is_finished() {
            self.marked = None;
        }
        // The terminal drops a selection on its own (the program cleared the screen, switched
        // screens or wrote over it); one scrolled out of view is still there to copy.
        if self.press.is_none() {
            self.selection = snapshot.terminal.has_selection;
        }
        self.snapshot = snapshot;
        cx.notify();
    }

    fn ring(&mut self, cx: &mut Context<Self>) {
        self.bell = true;
        self._bell = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BELL_FLASH).await;
            let _ = this.update(cx, |view, cx| {
                view.bell = false;
                cx.notify();
            });
        });
    }

    /// Sets the toolbar's cluster chip (the dock calls it as the tab's cluster changes).
    pub fn set_cluster(
        &mut self,
        label: String,
        tooltip: String,
        repointed: bool,
        cx: &mut Context<Self>,
    ) {
        let chip = (label.into(), tooltip.into(), repointed);
        if chip != self.chip {
            self.chip = chip;
            cx.notify();
        }
    }

    /// The toolbar's cluster chip.
    #[cfg(test)]
    pub fn chip(&self) -> &(SharedString, SharedString, bool) {
        &self.chip
    }

    /// Ends the session, saying why in the tab (its context was re-pointed at another server).
    pub fn end(&mut self, why: String, cx: &mut Context<Self>) {
        self.session.close();
        self.ended = Some(why);
        self.marked = None;
        cx.notify();
    }

    /// Whether the command still runs (or starts), so keystrokes reach it.
    pub fn is_live(&self) -> bool {
        self.ended.is_none() && !self.snapshot.state.is_finished()
    }

    pub fn has_selection(&self) -> bool {
        self.selection
    }

    /// The view is scrolled up into the history.
    pub fn is_scrolled_back(&self) -> bool {
        self.snapshot.terminal.display_offset > 0
    }

    /// There is history above what is shown.
    pub fn can_scroll_back(&self) -> bool {
        let t = &self.snapshot.terminal;
        t.display_offset < t.history_lines
    }

    /// The size the grid should have now: the one waiting, else the session's.
    fn wanted_size(&self) -> TerminalSize {
        self.pending_size.unwrap_or(self.size)
    }

    /// The grid measured `size` this frame: tell the pod once it stays put.
    fn grid_resized(&mut self, size: TerminalSize, cx: &mut Context<Self>) {
        if size == self.wanted_size() {
            // The first layout matched the size the session started with.
            self.resized = true;
            return;
        }
        if size == self.size {
            // Back where it was before the delay ran out.
            self.pending_size = None;
            self._resize = Task::ready(());
            return;
        }
        if !self.resized {
            // The first layout: the shell should start at the right size.
            self.send_resize(size);
            return;
        }
        self.pending_size = Some(size);
        self._resize = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_DELAY).await;
            let _ = this.update(cx, |view, _| view.send_resize(size));
        });
    }

    fn send_resize(&mut self, size: TerminalSize) {
        self.resized = true;
        self.pending_size = None;
        self._resize = Task::ready(());
        if size != self.size {
            self.size = size;
            self.session.resize(size);
        }
    }

    /// Sends keystrokes to the command.
    pub(super) fn send(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if !self.is_live() || bytes.is_empty() {
            return;
        }
        self.wheel.reset();
        self.session.write(bytes);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.is_live() {
            return;
        }
        let mode = self.snapshot.terminal.mode;
        if let Some(bytes) = terminal_keys::to_bytes(&event.keystroke, mode, ALT_IS_META) {
            cx.stop_propagation();
            self.send(bytes, cx);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let Some(layout) = self.layout.get() else {
            return;
        };
        let size = self.snapshot.terminal.size;
        let (row, col, side) = layout.cell_at(event.position, size);
        if event.modifiers.shift && self.selection {
            self.session.update_selection(row, col, side);
            self.press = Some(Press {
                row,
                col,
                side,
                kind: SelectionKind::Simple,
                started: true,
            });
            return;
        }
        let kind = terminal_model::selection_kind(event.click_count, event.modifiers.alt);
        let started = kind != SelectionKind::Simple;
        if started {
            self.session.start_selection(kind, row, col, side);
            self.selection = true;
        } else if self.selection {
            self.clear_selection();
        }
        self.press = Some(Press {
            row,
            col,
            side,
            kind,
            started,
        });
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let (Some(press), Some(layout)) = (self.press.as_mut(), self.layout.get()) else {
            return;
        };
        let (row, col, side) = layout.cell_at(event.position, self.snapshot.terminal.size);
        if !press.started {
            if (row, col, side) == (press.row, press.col, press.side) {
                return;
            }
            press.started = true;
            self.session
                .start_selection(press.kind, press.row, press.col, press.side);
            self.selection = true;
        }
        self.session.update_selection(row, col, side);
        cx.notify();
    }

    /// Ends a press: a drag's selection ends where the button was released, even off the grid
    /// or past the last move seen.
    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(press) = self.press.take() else {
            return;
        };
        let Some(layout) = self.layout.get() else {
            return;
        };
        let (row, col, side) = layout.cell_at(event.position, self.snapshot.terminal.size);
        if press.started {
            self.session.update_selection(row, col, side);
        } else if (row, col, side) != (press.row, press.col, press.side) {
            self.session
                .start_selection(press.kind, press.row, press.col, press.side);
            self.session.update_selection(row, col, side);
            self.selection = true;
        } else {
            return;
        }
        cx.notify();
    }

    fn clear_selection(&mut self) {
        self.selection = false;
        self.session.clear_selection();
    }

    /// Selects from one cell to another, inclusive.
    pub(super) fn select(&mut self, start: (u16, u16), end: (u16, u16), cx: &mut Context<Self>) {
        self.session
            .start_selection(SelectionKind::Simple, start.0, start.1, Side::Left);
        self.session.update_selection(end.0, end.1, Side::Right);
        self.selection = true;
        cx.notify();
    }

    fn scroll_wheel(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.get() else {
            return;
        };
        let line_height = px(layout.cell.height);
        let dy = f32::from(event.delta.pixel_delta(line_height).y);
        let lines = self.wheel.lines(dy, layout.cell.height);
        if lines == 0 {
            return;
        }
        cx.stop_propagation();
        let mode = self.snapshot.terminal.mode;
        if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) && self.is_live() {
            // A full-screen program (less, vi) with no history: scroll it with the arrow keys,
            // as other terminals do.
            let key = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            let bytes = key.repeat(lines.unsigned_abs() as usize);
            self.session.write(bytes);
        } else {
            self.session.scroll(lines);
        }
    }

    fn render_toolbar(&self, theme: &Theme) -> impl IntoElement {
        let (tone, state) = match &self.ended {
            Some(_) => (Tone::Warning, "Ended: the context changed".to_owned()),
            None => terminal_model::state_text(&self.snapshot.state),
        };
        let color = tone_color(tone, theme);
        let full_state = state.clone();
        let title = self.snapshot.terminal.title.clone();
        h_flex()
            .id("terminal-toolbar")
            .flex_wrap()
            .min_h(px(super::workspace::HEADER_HEIGHT))
            .py_0p5()
            .px_2()
            .gap_x_1p5()
            .flex_none()
            .min_w_0()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                h_flex()
                    .id("terminal-state")
                    .gap_1()
                    .flex_shrink(1.)
                    .min_w(px(60.))
                    .max_w(px(360.))
                    .text_color(color)
                    .child(div().size_1p5().flex_none().rounded_full().bg(color))
                    .child(div().min_w_0().truncate().child(state))
                    .tooltip(move |window, cx| Tooltip::new(full_state.clone()).build(window, cx))
                    .test_support(),
            )
            // Which cluster the shell runs on, beside its state (the tab says it too).
            .child(cluster_chip(
                "terminal-cluster",
                self.chip.0.clone(),
                self.chip.2,
                Some(self.chip.1.clone()),
                theme,
            ))
            .when_some(title, |el, title| {
                el.child(
                    div()
                        .id("terminal-title")
                        .min_w_0()
                        .flex_shrink(1.)
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(title)
                        .test_support(),
                )
            })
            .child(div().flex_1())
            .when(self.is_scrolled_back(), |el| {
                el.child(
                    Button::new("terminal-bottom")
                        .icon(IconName::ArrowDown)
                        .xsmall()
                        .ghost()
                        .tooltip(format!(
                            "Scroll to the bottom ({})",
                            registry::key_hint("shift-end")
                        ))
                        .on_click(registry::on_click(ScrollToBottom)),
                )
            })
            .child(
                Button::new("terminal-copy")
                    .icon(IconName::Copy)
                    .xsmall()
                    .ghost()
                    .disabled(!self.selection)
                    .tooltip(format!(
                        "Copy the selection ({})",
                        registry::key_hint(registry::TERMINAL_COPY[0])
                    ))
                    .on_click(registry::on_click(CopyText)),
            )
            .child(
                Button::new("terminal-paste")
                    .xsmall()
                    .ghost()
                    .label("Paste")
                    .disabled(!self.is_live())
                    .tooltip(format!(
                        "Paste into the shell ({})",
                        registry::key_hint(registry::TERMINAL_PASTE[0])
                    ))
                    .on_click(registry::on_click(PasteText)),
            )
    }

    /// The strips under the toolbar: why the command ended, and the last copy.
    fn render_notes(&self, theme: &Theme) -> Vec<AnyElement> {
        let mut notes = Vec::new();
        let ended = match &self.snapshot.state {
            _ if self.ended.is_some() => self.ended.clone().map(|why| (theme.warning, why)),
            flint_core::ExecState::Exited { code } => Some((
                theme.muted_foreground,
                match code {
                    Some(code) => format!(
                        "The shell exited with code {code}. Its output stays here until you \
                         close the tab."
                    ),
                    None => "The shell exited. Its output stays here until you close the tab."
                        .to_owned(),
                },
            )),
            flint_core::ExecState::Failed(reason) => {
                Some((theme.danger, format!("The shell could not run: {reason}")))
            }
            _ => None,
        };
        if let Some((color, text)) = ended {
            let restart = Button::new("terminal-restart")
                .xsmall()
                .label("Restart shell")
                .tab_stop(false)
                .tooltip(format!(
                    "Start a new shell in this tab ({})",
                    registry::key_hint("secondary-r")
                ))
                .on_click(registry::on_click(RestartShell));
            notes.push(
                notice_with("terminal-ended", text, color, restart)
                    .test_support()
                    .into_any_element(),
            );
        }
        if let Some(note) = &self.note {
            notes.push(
                notice(
                    "terminal-note",
                    note.text.clone(),
                    tone_color(note.tone, theme),
                )
                .test_support()
                .into_any_element(),
            );
        }
        notes
    }

    pub(super) fn set_note(&mut self, tone: Tone, text: String) {
        self.note = Some(Note { tone, text });
    }

    /// What the grid paints this frame.
    fn frame(&self, theme: &Theme, window: &Window) -> GridFrame {
        GridFrame {
            snapshot: self.snapshot.clone(),
            palette: TerminalPalette::new(theme),
            font: font(theme.mono_font_family.clone()),
            font_size: theme.mono_font_size,
            focused: self.focus.is_focused(window),
            live: self.is_live(),
            marked: self.marked.clone(),
            bell: self.bell.then(|| theme.foreground.opacity(0.06)),
        }
    }
}

fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
    match tone {
        Tone::Muted => theme.muted_foreground,
        Tone::Live => theme.success,
        Tone::Warning => theme.warning,
        Tone::Danger => theme.danger,
    }
}

impl DockItem for TerminalView {
    fn tab_key(&self) -> TabKey {
        TabKey::Shell(self.request.clone())
    }

    fn tab_title(&self) -> SharedString {
        self.request.title().into()
    }

    fn tab_tooltip(&self) -> SharedString {
        self.request.tooltip().into()
    }

    fn tab_status(&self) -> TabStatus {
        use flint_core::ExecState as S;
        if self.ended.is_some() {
            return TabStatus::Ended;
        }
        match &self.snapshot.state {
            S::Connecting => TabStatus::Starting,
            S::Running { .. } => TabStatus::Live,
            S::Exited { .. } | S::Closed => TabStatus::Ended,
            S::Failed(_) => TabStatus::Failed,
        }
    }

    fn tab_focus(&self) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let frame = self.frame(&theme, window);
        let layout = self.layout.clone();
        let wanted = self.wanted_size();
        // The first layout is reported even when it matches the starting size, so later
        // changes are debounced.
        let first = !self.resized;
        let view = cx.weak_entity();
        let focus = self.focus.clone();
        let grid = canvas(
            {
                let view = view.clone();
                move |bounds, window, cx| {
                    let grid = measure(&frame, bounds, window);
                    layout.set(Some(grid));
                    if grid.size != wanted || first {
                        // Not while drawing: the view is told after this frame.
                        cx.defer(move |cx| {
                            let _ = view.update(cx, |view, cx| view.grid_resized(grid.size, cx));
                        });
                    }
                    (frame, grid)
                }
            },
            move |bounds, (frame, grid), window, cx| {
                window.handle_input(
                    &focus,
                    TerminalInput {
                        view: view.clone(),
                        bounds,
                    },
                    cx,
                );
                // A drag goes on past the grid's edge (the cell is clamped), so it follows the
                // pointer over the whole window, not only over the grid.
                let moved = view.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = moved.update(cx, |view, cx| view.mouse_move(event, window, cx));
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                        let _ = view.update(cx, |view, cx| view.mouse_up(event, window, cx));
                    }
                });
                paint_grid(&frame, &grid, bounds, window, cx);
            },
        )
        .size_full();
        v_flex()
            .id("terminal-view")
            .key_context(registry::TERMINAL)
            .track_focus(&self.focus)
            .map(|el| Self::register_actions(el, cx))
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .min_h_0()
            .bg(theme.background)
            .child(self.render_toolbar(&theme))
            .children(self.render_notes(&theme))
            .child(
                div()
                    .id("terminal-grid")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .cursor(CursorStyle::IBeam)
                    // not-an-action: presses and drags select screen cells (SelectText is the action).
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_scroll(cx.listener(Self::scroll_wheel))
                    .child(grid)
                    .test_support(),
            )
            .test_support()
    }
}

/// Everything the grid needs to paint one frame, taken from the view while it renders.
struct GridFrame {
    snapshot: Arc<ExecSnapshot>,
    palette: TerminalPalette,
    font: Font,
    font_size: Pixels,
    focused: bool,
    /// The command still runs: an ended session draws no cursor.
    live: bool,
    marked: Option<String>,
    /// The bell's flash over the screen, while it shows.
    bell: Option<Hsla>,
}

/// The grid's cell size from the mono font, and how many cells fit `bounds`.
fn measure(frame: &GridFrame, bounds: Bounds<Pixels>, window: &Window) -> GridLayout {
    let text = window.text_system();
    let font_id = text.resolve_font(&frame.font);
    let size = f32::from(frame.font_size);
    let advance = text
        .advance(font_id, frame.font_size, 'm')
        .map_or(size * 0.6, |a| f32::from(a.width));
    let cell = CellSize::new(advance, size);
    GridLayout {
        origin: point(bounds.origin.x + px(PADDING), bounds.origin.y + px(PADDING)),
        cell,
        size: terminal_model::grid_size(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            cell,
        ),
    }
}

/// Paints the snapshot's screen: backgrounds, selection, text, the cursor and composed text.
fn paint_grid(
    frame: &GridFrame,
    grid: &GridLayout,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let terminal = &frame.snapshot.terminal;
    let palette = &frame.palette;
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        for row in 0..terminal.size.rows {
            let cells = terminal.row(row);
            let styles: Vec<CellStyle> = cells.iter().map(|c| cell_style(c, palette)).collect();
            // Backgrounds, then the selection tint over them.
            let backgrounds: Vec<Option<Hsla>> = styles.iter().map(|s| s.bg).collect();
            for (col, width, color) in spans(&backgrounds) {
                window.paint_quad(fill(grid.cell_bounds(row, col, width), color));
            }
            let selected: Vec<Option<bool>> =
                cells.iter().map(|c| c.selected.then_some(true)).collect();
            for (col, width, _) in spans(&selected) {
                window.paint_quad(fill(grid.cell_bounds(row, col, width), palette.selection));
            }
            for segment in row_segments(cells, &styles, |cell: &Cell, style: &CellStyle| {
                style.is_blank(cell)
            }) {
                paint_segment(frame, grid, row, &segment, window, cx);
            }
        }
        if let Some(cursor) = terminal.cursor {
            paint_cursor(
                frame,
                grid,
                cursor.row,
                cursor.col,
                cursor.shape,
                window,
                cx,
            );
        }
        if let Some(flash) = frame.bell {
            window.paint_quad(fill(bounds, flash));
        }
    });
}

/// The text run for `len` bytes drawn in `style`.
fn text_run(frame: &GridFrame, style: &CellStyle, len: usize) -> TextRun {
    let mut font = frame.font.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color: style.fg,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(style.fg),
            wavy: false,
        }),
        strikethrough: style.strikethrough.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(style.fg),
        }),
    }
}

fn paint_segment(
    frame: &GridFrame,
    grid: &GridLayout,
    row: u16,
    segment: &Segment<CellStyle>,
    window: &mut Window,
    cx: &mut App,
) {
    let runs: Vec<TextRun> = segment
        .runs
        .iter()
        .map(|(len, style)| text_run(frame, style, *len))
        .collect();
    // Every glyph one cell wide keeps the columns aligned whatever the font's fallbacks do.
    let force_width = (!segment.isolated).then(|| px(grid.cell.width));
    let line = window.text_system().shape_line(
        segment.text.clone().into(),
        frame.font_size,
        &runs,
        force_width,
    );
    let origin = grid.cell_bounds(row, segment.col, 1).origin;
    let _ = line.paint(
        origin,
        px(grid.cell.height),
        TextAlign::Left,
        None,
        window,
        cx,
    );
}

/// The cursor: a block (with its character in the cursor's text color), a beam or an
/// underline; hollow while the tab does not have focus, and gone once the session ended
/// ([`cursor_look`]).
fn paint_cursor(
    frame: &GridFrame,
    grid: &GridLayout,
    row: u16,
    col: u16,
    shape: CursorShape,
    window: &mut Window,
    cx: &mut App,
) {
    let terminal = &frame.snapshot.terminal;
    if row >= terminal.size.rows || col >= terminal.size.cols {
        return;
    }
    let cell = &terminal.row(row)[usize::from(col)];
    let wide = cell
        .flags
        .contains(flint_core::terminal::CellFlags::WIDE_CHAR);
    let mut bounds = grid.cell_bounds(row, col, if wide { 2 } else { 1 });
    let color = frame.palette.cursor;
    if let Some(marked) = &frame.marked {
        // Text being composed sits on the cursor, underlined, until it is committed.
        let len = marked.len();
        let style = CellStyle {
            fg: frame.palette.foreground,
            bg: None,
            bold: false,
            italic: false,
            underline: true,
            strikethrough: false,
            hidden: false,
        };
        let line = window.text_system().shape_line(
            marked.clone().into(),
            frame.font_size,
            &[TextRun {
                background_color: Some(frame.palette.background),
                ..text_run(frame, &style, len)
            }],
            None,
        );
        let _ = line.paint(
            bounds.origin,
            px(grid.cell.height),
            TextAlign::Left,
            None,
            window,
            cx,
        );
        return;
    }
    let Some(look) = cursor_look(shape, frame.focused, frame.live) else {
        return;
    };
    match look {
        CursorLook::Block => {
            window.paint_quad(fill(bounds, color));
            if cell.c != ' ' && !cell_style(cell, &frame.palette).hidden {
                let mut text = String::from(cell.c);
                if let Some(extra) = &cell.zerowidth {
                    text.extend(extra.iter());
                }
                let style = CellStyle {
                    fg: frame.palette.cursor_text,
                    ..cell_style(cell, &frame.palette)
                };
                let line = window.text_system().shape_line(
                    text.clone().into(),
                    frame.font_size,
                    &[text_run(frame, &style, text.len())],
                    None,
                );
                let _ = line.paint(
                    bounds.origin,
                    px(grid.cell.height),
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }
        CursorLook::Hollow => {
            window.paint_quad(outline(bounds, color, BorderStyle::Solid));
        }
        CursorLook::Beam { faded } => {
            bounds.size.width = px(2.);
            window.paint_quad(fill(bounds, color.opacity(if faded { 0.5 } else { 1. })));
        }
        CursorLook::Underline { faded } => {
            bounds.origin.y += bounds.size.height - px(2.);
            bounds.size.height = px(2.);
            window.paint_quad(fill(bounds, color.opacity(if faded { 0.5 } else { 1. })));
        }
    }
}

/// The platform's text input for a shell tab: typed and composed text goes to the shell. It
/// holds the view weakly; the window keeps the last frame's handler until the next one.
struct TerminalInput {
    view: WeakEntity<TerminalView>,
    bounds: Bounds<Pixels>,
}

impl TerminalInput {
    fn with<R>(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut TerminalView, &mut Context<TerminalView>) -> R,
    ) -> Option<R> {
        self.view.update(cx, f).ok()
    }
}

/// The length of `text` in UTF-16 code units, which platform text input counts in.
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

impl InputHandler for TerminalInput {
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<UTF16Selection> {
        self.with(cx, |view, _| {
            let end = view.marked.as_deref().map_or(0, utf16_len);
            UTF16Selection {
                range: end..end,
                reversed: false,
            }
        })
    }

    fn marked_text_range(&mut self, _: &mut Window, cx: &mut App) -> Option<Range<usize>> {
        self.with(cx, |view, _| {
            view.marked.as_deref().map(|m| 0..utf16_len(m))
        })
        .flatten()
    }

    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut App,
    ) {
        self.with(cx, |view, cx| {
            view.marked = None;
            view.send(text.as_bytes().to_vec(), cx);
            cx.notify();
        });
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut App,
    ) {
        self.with(cx, |view, cx| {
            view.marked = (!text.is_empty()).then(|| text.to_owned());
            cx.notify();
        });
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut App) {
        self.with(cx, |view, cx| {
            view.marked = None;
            cx.notify();
        });
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        // The input method's window goes by the cursor.
        self.with(cx, |view, _| {
            let layout = view.layout.get()?;
            let cursor = view.snapshot.terminal.cursor?;
            Some(layout.cell_bounds(cursor.row, cursor.col, 1))
        })
        .flatten()
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut App,
    ) -> Option<usize> {
        None
    }

    fn element_bounds(&mut self, _: &mut Window, _: &mut App) -> Option<Bounds<Pixels>> {
        Some(self.bounds)
    }

    fn accepts_text_input(&mut self, _: &mut Window, cx: &mut App) -> bool {
        self.with(cx, |view, _| view.is_live()).unwrap_or(false)
    }

    /// A held key repeats, as in other terminals, instead of offering accented letters.
    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}

#[path = "terminal_actions.rs"]
mod actions;

#[cfg(test)]
#[path = "terminal_tests.rs"]
mod tests;
