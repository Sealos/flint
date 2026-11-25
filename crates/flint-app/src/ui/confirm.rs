//! Confirmation dialogs: a title, a sentence or two, and two buttons (Expose to AI, saving logs,
//! a multi-line paste). Every one opens through [`open`], so test builds can say what the open
//! dialog says (`shown`): GPUI keeps only a builder for a dialog, and nothing can read its text
//! back.

use std::cell::{Cell, RefCell};
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher as _;
use std::rc::{Rc, Weak};
use std::sync::OnceLock;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::write_binding::ConnectionGuard;

/// What a confirmation says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogText {
    pub title: String,
    pub text: String,
    pub ok: String,
    /// The Cancel button's text; empty for a notice with only OK (About Flint).
    pub cancel: String,
    /// The element id of the description, for view tests.
    pub id: &'static str,
    /// Enter cancels instead of confirming, for a confirmation whose OK ends work that cannot be
    /// got back (quitting, closing the window): only the OK button confirms it (a click, or Space
    /// while it has focus), and Enter cancels wherever focus is. Escape cancels either way.
    pub default_cancel: bool,
}

/// The key context of a [`DialogText::default_cancel`] confirmation's description (which holds
/// focus while it is open) and of its buttons: Enter and Escape are bound to Cancel there. The
/// dialog's own keys (Enter confirms) are turned off, so they cannot answer for it either; with
/// focus on the dialog's own node, [`init`] answers them.
pub const DEFAULT_CANCEL: &str = "DefaultCancel";

/// What a gated confirmation ([`open_gated`]) says beside its buttons, and whether it can be
/// confirmed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmState {
    /// What confirming would do, or why it cannot be confirmed.
    pub note: String,
    /// OK is disabled, and its handler refuses a click, Enter and automation alike.
    pub blocked: bool,
}

/// Reads a gated confirmation's state from its content, on every frame and every confirm.
type Gate = Rc<dyn Fn(&App) -> ConfirmState>;

/// The share of the window's height a confirmation's text may take before it scrolls, leaving
/// room for a wrapped title and the buttons at the minimum window size.
const DESCRIPTION_MAX_SHARE: f32 = 0.45;
/// The same for a confirmation with controls under its text (the drain options).
const DESCRIPTION_MAX_SHARE_OVER_CONTROLS: f32 = 0.3;
/// The share of the window's height the pinned lines and the text under them may take together.
const PINNED_DESCRIPTION_MAX_SHARE: f32 = 0.6;
/// The same with controls under the text, which need their own room.
const PINNED_DESCRIPTION_MAX_SHARE_OVER_CONTROLS: f32 = 0.42;
/// The most characters of a name a dialog title quotes.
const TITLE_NAME_CHARS: usize = 48;
/// The most characters of a value a pinned line can show, however narrow they are.
const MAX_LINE_CHARS: usize = 240;
/// What a pinned value keeps until its width is known, in the dialog's first frame.
const FIRST_FRAME_CHARS: usize = 40;
/// The least height the text under pinned lines keeps, however much they wrap.
const MIN_TEXT_HEIGHT: f32 = 48.;
/// How far one arrow key scrolls a long text.
const SCROLL_LINE: f32 = 20.;

/// Opens a confirmation saying `dialog`; confirming runs `on_ok` and closes it. Test builds open
/// it at once, without the entrance animation, so automation sees it without waiting.
pub fn open(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    open_with(window, cx, dialog, None, move |window, cx| {
        on_ok(window, cx);
        true
    });
}

/// [`open`], with the last `pinned` lines of the text always on show above the rest, which
/// scrolls: what a write confirmation names as its target must be read without scrolling.
pub fn open_pinned(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    pinned: usize,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    let on_ok = move |window: &mut Window, cx: &mut App| {
        on_ok(window, cx);
        true
    };
    open_dialog(
        window,
        cx,
        dialog,
        pinned,
        None,
        None,
        Guards::default(),
        on_ok,
    );
}

/// Says why the object a confirmation is about has changed since its review, if it has: the
/// confirmation is then closed unanswered ([`close_changed`]).
#[derive(Clone)]
pub(in crate::ui) struct ObjectGuard(Rc<Changed>);

/// Why an object changed since its review, if it did.
type Changed = dyn Fn(&App) -> Option<String>;

impl ObjectGuard {
    pub(in crate::ui) fn new(changed: impl Fn(&App) -> Option<String> + 'static) -> Self {
        Self(Rc::new(changed))
    }

    pub(in crate::ui) fn changed(&self, cx: &App) -> Option<String> {
        (self.0)(cx)
    }
}

/// What an open confirmation must still hold to be answered.
#[derive(Clone, Default)]
struct Guards {
    connection: Option<ConnectionGuard>,
    object: Option<ObjectGuard>,
}

/// [`open_pinned`] for a write: it can be answered only while `guard` holds. Once it does not,
/// OK runs nothing and closes the dialog, and [`close_stale`] closes it unanswered.
pub(in crate::ui) fn open_pinned_guarded(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    pinned: usize,
    content: Option<AnyView>,
    guard: ConnectionGuard,
    on_ok: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    let guards = Guards {
        connection: Some(guard),
        object: None,
    };
    open_dialog(window, cx, dialog, pinned, content, None, guards, on_ok);
}

/// [`open_pinned_guarded`] for a change to one object: it is also closed, unanswered, once
/// `object` says that object changed ([`close_changed`]).
pub(in crate::ui) fn open_pinned_watched(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    pinned: usize,
    guard: ConnectionGuard,
    object: ObjectGuard,
    on_ok: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    let guards = Guards {
        connection: Some(guard),
        object: Some(object),
    };
    open_dialog(window, cx, dialog, pinned, None, None, guards, on_ok);
}

/// [`open`], with `content` (a view of its own, such as pickers) drawn under the text, and an
/// `on_ok` that may keep the dialog open (`false`), when the content says why it cannot be
/// confirmed yet.
pub fn open_with(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    content: Option<AnyView>,
    on_ok: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    open_dialog(
        window,
        cx,
        dialog,
        0,
        content,
        None,
        Guards::default(),
        on_ok,
    );
}

/// [`open_with`], with `state` read from the content: its note is drawn beside the buttons, and
/// while it is blocked OK is disabled and `on_ok` does not run, however it is asked for. Not for
/// a [`DialogText::default_cancel`] confirmation, which has a footer of its own.
pub fn open_gated(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    content: AnyView,
    state: impl Fn(&App) -> ConfirmState + 'static,
    on_ok: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    debug_assert!(!dialog.default_cancel);
    let gate: Gate = Rc::new(state);
    open_dialog(
        window,
        cx,
        dialog,
        0,
        Some(content),
        Some(gate),
        Guards::default(),
        on_ok,
    );
}

/// [`open_gated`], which also closes and runs nothing once `guard` stops holding.
pub(in crate::ui) fn open_gated_guarded(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    content: AnyView,
    guard: ConnectionGuard,
    state: impl Fn(&App) -> ConfirmState + 'static,
    on_ok: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    debug_assert!(!dialog.default_cancel);
    let gate: Gate = Rc::new(state);
    open_dialog(
        window,
        cx,
        dialog,
        0,
        Some(content),
        Some(gate),
        Some(guard),
        on_ok,
    );
}

#[allow(clippy::too_many_arguments)]
fn open_dialog(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    pinned: usize,
    content: Option<AnyView>,
    gate: Option<Gate>,
    guards: Guards,
    on_ok: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    #[cfg(any(test, feature = "automation"))]
    remember(window, dialog.clone(), cx);
    let on_ok = Rc::new(on_ok);
    // Focus moves into the description, whose key context turns Enter into Cancel.
    let default_cancel = dialog.default_cancel.then(|| cx.focus_handle());
    let focus = default_cancel.clone();
    if let Some(focus) = &default_cancel {
        cx.default_global::<DefaultCancels>()
            .0
            .push(focus.downgrade());
    }
    let (scrolling, target) = split_pinned(&dialog.text, pinned);
    let fitted = Rc::new(RefCell::new(FittedTarget::new(target)));
    let scrolling = SharedString::from(scrolling);
    let scroll = ScrollHandle::new();
    let text_focus = cx.focus_handle();
    let target_height = Rc::new(Cell::new(px(0.)));
    // What a gated footer's buttons answer from: opening the dialog does not focus it.
    let gate_focus = gate.as_ref().map(|_| cx.focus_handle());
    let live = LiveDialog::open(window, guards, cx);
    window.open_alert_dialog(cx, move |alert, window, cx| {
        let on_ok = on_ok.clone();
        let gate = gate.clone();
        let (live_ok, live_cancel) = (live.clone(), live.clone());
        let gated = gate.as_ref().map(|gate| gate(cx));
        // A long text scrolls, so it cannot push the buttons or its last lines out of the dialog.
        let viewport = window.viewport_size().height;
        let text_height = if pinned > 0 {
            let share = if content.is_some() {
                PINNED_DESCRIPTION_MAX_SHARE_OVER_CONTROLS
            } else {
                PINNED_DESCRIPTION_MAX_SHARE
            };
            (viewport * share - target_height.get()).max(px(MIN_TEXT_HEIGHT))
        } else if content.is_some() {
            viewport * DESCRIPTION_MAX_SHARE_OVER_CONTROLS
        } else {
            viewport * DESCRIPTION_MAX_SHARE
        };
        // What a pinned line had to shorten is here in full, in the text that scrolls.
        let body = fitted.borrow().scrolling_text(&scrolling);
        // A Tab stop only when there is something to scroll to.
        let overflows = scroll.max_offset().y > px(0.);
        // The dialog's own background when unfocused, so focus does not move the text.
        let border = if text_focus.is_focused(window) {
            cx.theme().ring
        } else {
            cx.theme().background
        };
        #[cfg(feature = "automation")]
        let alert = alert.entrance(gpui_kit::component::dialog::DialogEntrance::None);
        alert
            .confirm()
            .title(dialog.title.clone())
            .description(
                div()
                    .id(dialog.id)
                    .aria_label(dialog.text.clone())
                    // First, so that nothing can push it out of the dialog.
                    .when(pinned > 0, |this| {
                        this.child(pinned_target(fitted.clone(), target_height.clone()))
                    })
                    // Only the text scrolls: controls under it stay in place and clickable.
                    .when(!body.is_empty(), |this| {
                        this.child(
                            div()
                                .id("confirm-text")
                                .max_h(text_height)
                                .overflow_y_scroll()
                                .track_scroll(&scroll)
                                .test_support()
                                .track_focus(&text_focus.clone().tab_stop(overflows))
                                .border_1()
                                .border_color(border)
                                .on_key_down({
                                    let scroll = scroll.clone();
                                    move |event, window, cx| {
                                        scroll_by_key(&scroll, &event.keystroke, window, cx)
                                    }
                                })
                                .child(
                                    div()
                                        .id("confirm-text-body")
                                        .aria_label(body.clone())
                                        .child(body.clone())
                                        .test_support(),
                                ),
                        )
                    })
                    .when_some(content.clone(), |this, content| this.child(content))
                    .when_some(default_cancel.as_ref(), |this, focus| {
                        this.track_focus(focus).key_context(DEFAULT_CANCEL)
                    })
                    .when_some(gate_focus.as_ref(), |this, focus| this.track_focus(focus))
                    .test_support(),
            )
            .ok_text(dialog.ok.clone())
            .cancel_text(dialog.cancel.clone())
            .when(dialog.cancel.is_empty(), |this| this.show_cancel(false))
            .when_some(default_cancel.as_ref(), |this, focus| {
                this.keyboard(false)
                    .footer(default_cancel_footer(&dialog, focus))
            })
            .when_some(
                gate_focus.as_ref().zip(gated.as_ref()),
                |this, (focus, state)| this.footer(gated_footer(&dialog, state, focus, cx)),
            )
            // A press drawn before this dialog was answered must not close the one under it.
            .on_cancel(move |_, window, cx| live_cancel.retire(window, cx))
            .on_ok(move |_, window, cx| {
                if !live_ok.is_newest(window, cx)
                    || gate.as_ref().is_some_and(|gate| gate(cx).blocked)
                {
                    return false;
                }
                let before = window.focused(cx);
                // Its connection changed: whatever asks, this runs nothing and only closes it.
                if live_ok.is_stale(cx) {
                    if live_ok.retire(window, cx) {
                        close_returning_focus(before, window, cx);
                    }
                    return false;
                }
                if on_ok(window, cx) && live_ok.retire(window, cx) {
                    close_returning_focus(before, window, cx);
                }
                // Closed above, or kept open. Answering `true` would close it through the
                // dialog's deferred path, which puts focus back after the exit animation, over
                // whatever received it meanwhile.
                false
            })
    });
    // The dialog took focus as it opened: hand it on to the description.
    if let Some(focus) = focus {
        focus.focus(window, cx);
    }
    // Whether the text overflows is known once it is laid out.
    window.on_next_frame(|window, _| window.refresh());
}

actions!(
    dialog,
    [
        /// Answer the open dialog with its third choice (Discard, beside Cancel and Save).
        ConfirmAlternative
    ]
);

/// A confirmation with a third answer beside Cancel and OK: `alternative` (Discard, when OK
/// saves). Enter confirms and Escape cancels, as in [`open`]; the alternative is its button or
/// [`ConfirmAlternative`]. The dialog closes before either answer runs.
pub fn open_with_alternative(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    alternative: String,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
    on_alternative: impl Fn(&mut Window, &mut App) + 'static,
) {
    debug_assert!(!dialog.default_cancel);
    #[cfg(any(test, feature = "automation"))]
    remember(window, dialog.clone(), cx);
    let on_ok = Rc::new(on_ok);
    let on_alternative = Rc::new(on_alternative);
    // The buttons answer from here, inside the dialog, whatever holds focus.
    let focus = cx.focus_handle();
    let answer_from = focus.clone();
    let live = LiveDialog::open(window, None, cx);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (on_ok, on_alternative) = (on_ok.clone(), on_alternative.clone());
        let (live_ok, live_cancel, live_alternative) = (live.clone(), live.clone(), live.clone());
        #[cfg(feature = "automation")]
        let alert = alert.entrance(gpui_kit::component::dialog::DialogEntrance::None);
        alert
            .confirm()
            .title(dialog.title.clone())
            .description(
                div()
                    .id(dialog.id)
                    .aria_label(dialog.text.clone())
                    .track_focus(&focus)
                    .on_action(move |_: &ConfirmAlternative, window, cx| {
                        if live_alternative.retire(window, cx) {
                            window.close_dialog(cx);
                            on_alternative(window, cx);
                        }
                    })
                    .child(dialog.text.clone())
                    .test_support(),
            )
            .ok_text(dialog.ok.clone())
            .cancel_text(dialog.cancel.clone())
            .footer(alternative_footer(&dialog, &alternative, &focus))
            .on_cancel(move |_, window, cx| live_cancel.retire(window, cx))
            .on_ok(move |_, window, cx| {
                if live_ok.retire(window, cx) {
                    window.close_dialog(cx);
                    on_ok(window, cx);
                }
                false
            })
    });
    answer_from.focus(window, cx);
}

/// The buttons of [`open_with_alternative`]: Cancel, the alternative, and OK.
fn alternative_footer(
    dialog: &DialogText,
    alternative: &str,
    focus: &FocusHandle,
) -> impl IntoElement {
    use gpui_kit::base::actions::{Cancel, Confirm};
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;

    use super::registry;

    DialogFooter::new()
        .child(
            Button::new("cancel")
                .label(dialog.cancel.clone())
                .on_click(registry::on_click_in(focus.clone(), Cancel)),
        )
        .child(
            Button::new("alternative")
                .label(alternative.to_owned())
                .on_click(registry::on_click_in(focus.clone(), ConfirmAlternative)),
        )
        .child(
            Button::new("ok")
                .label(dialog.ok.clone())
                .primary()
                .on_click(registry::on_click_in(
                    focus.clone(),
                    Confirm { secondary: false },
                )),
        )
}

/// The confirmations not yet answered, per window, oldest first. A dialog's controls stay on
/// screen until the next frame, so a press can reach one that was already answered.
#[derive(Default)]
struct LiveDialogs(std::collections::HashMap<WindowId, Vec<Weak<Guards>>>);

impl Global for LiveDialogs {}

/// One confirmation's place in [`LiveDialogs`], for as long as the dialog holds it, with the
/// connection it can be answered on and the object it is about, if any.
#[derive(Clone)]
struct LiveDialog(Rc<Guards>);

impl LiveDialog {
    fn open(window: &Window, guards: Guards, cx: &mut App) -> Self {
        let dialog = Self(Rc::new(guards));
        let live = cx.default_global::<LiveDialogs>();
        live.0.retain(|_, stack| {
            stack.retain(|d| d.strong_count() > 0);
            !stack.is_empty()
        });
        live.0
            .entry(window.window_handle().window_id())
            .or_default()
            .push(Rc::downgrade(&dialog.0));
        dialog
    }

    /// Whether this is the newest confirmation still unanswered in `window`. With no dialog open
    /// there, none is, however it was closed.
    fn is_newest(&self, window: &mut Window, cx: &mut App) -> bool {
        let id = window.window_handle().window_id();
        if !window.has_active_dialog(cx) {
            cx.default_global::<LiveDialogs>().0.remove(&id);
            return false;
        }
        cx.try_global::<LiveDialogs>()
            .and_then(|live| live.0.get(&id))
            .and_then(|stack| stack.iter().rev().find(|d| d.strong_count() > 0))
            .is_some_and(|newest| newest.ptr_eq(&Rc::downgrade(&self.0)))
    }

    /// Whether the connection this dialog is about has changed since it opened.
    fn is_stale(&self, cx: &App) -> bool {
        self.0.connection.as_ref().is_some_and(|g| !g.holds(cx))
    }

    /// Why the object this dialog is about has changed since its review, if it has.
    fn object_changed(&self, cx: &App) -> Option<String> {
        self.0.object.as_ref()?.changed(cx)
    }

    /// Marks this dialog answered, if it is the newest one: whether it was.
    fn retire(&self, window: &mut Window, cx: &mut App) -> bool {
        if !self.is_newest(window, cx) {
            return false;
        }
        let me = Rc::downgrade(&self.0);
        if let Some(stack) = cx
            .global_mut::<LiveDialogs>()
            .0
            .get_mut(&window.window_handle().window_id())
        {
            stack.retain(|d| !d.ptr_eq(&me));
        }
        true
    }
}

/// Closes the newest confirmation from code, keeping [`LiveDialogs`] in step.
pub fn close_newest(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    if let Some(stack) = cx.default_global::<LiveDialogs>().0.get_mut(&id) {
        stack.retain(|d| d.strong_count() > 0);
        stack.pop();
    }
    window.close_dialog(cx);
}

/// The newest confirmation open in `window`, if any.
fn newest(window: &mut Window, cx: &mut App) -> Option<LiveDialog> {
    if !window.has_active_dialog(cx) {
        return None;
    }
    cx.try_global::<LiveDialogs>()
        .and_then(|live| live.0.get(&window.window_handle().window_id()))
        .and_then(|stack| stack.iter().rev().find_map(Weak::upgrade))
        .map(LiveDialog)
}

/// Whether the newest confirmation open in `window` is about a connection that has changed.
pub(in crate::ui) fn newest_is_stale(window: &mut Window, cx: &mut App) -> bool {
    newest(window, cx).is_some_and(|newest| newest.is_stale(cx))
}

/// Closes, unanswered, each newest confirmation whose object changed since its review: why the
/// first one closed, if any did.
pub(in crate::ui) fn close_changed(window: &mut Window, cx: &mut App) -> Option<String> {
    let mut first = None;
    while let Some(why) = newest(window, cx).and_then(|newest| newest.object_changed(cx)) {
        close_newest(window, cx);
        first.get_or_insert(why);
    }
    first
}

/// Closes, unanswered, each newest confirmation whose connection has changed: whether any closed.
pub(in crate::ui) fn close_stale(window: &mut Window, cx: &mut App) -> bool {
    let mut closed = false;
    while newest_is_stale(window, cx) {
        close_newest(window, cx);
        closed = true;
    }
    closed
}

/// Closes the newest dialog and returns focus to what held it before the dialog, in this update.
/// If its OK handler moved focus away from `before`, focus stays on that new target.
fn close_returning_focus(before: Option<FocusHandle>, window: &mut Window, cx: &mut App) {
    let moved = window
        .focused(cx)
        .filter(|now| Some(now) != before.as_ref());
    window.close_dialog(cx);
    if let Some(moved) = moved {
        window.focus(&moved, cx);
    }
}

/// `text` split into what scrolls and its last `pinned` lines, without the blank line between.
fn split_pinned(text: &str, pinned: usize) -> (String, String) {
    if pinned == 0 {
        return (text.to_owned(), String::new());
    }
    let lines: Vec<&str> = text.lines().collect();
    let at = lines.len().saturating_sub(pinned);
    (
        lines[..at].join("\n").trim_end().to_owned(),
        lines[at..].join("\n"),
    )
}

/// The pinned lines as drawn: each on one line, shortened to the width they were last fitted to.
struct FittedTarget {
    full: String,
    lines: Vec<SharedString>,
    fitted_to: Option<(Pixels, Pixels)>,
}

impl FittedTarget {
    fn new(full: String) -> Self {
        let lines = fit_lines(&full, |line| line.chars().count() <= FIRST_FRAME_CHARS);
        Self {
            full,
            lines,
            fitted_to: None,
        }
    }

    fn is_cut(&self) -> bool {
        self.full.lines().ne(self.lines.iter().map(|l| l.as_ref()))
    }

    /// `text`, followed by the pinned lines in full when one of them is shortened.
    fn scrolling_text(&self, text: &SharedString) -> SharedString {
        match (self.is_cut(), text.is_empty()) {
            (false, _) => text.clone(),
            (true, true) => format!("Full target:\n{}", self.full).into(),
            (true, false) => format!("{text}\n\nFull target:\n{}", self.full).into(),
        }
    }

    /// Fits the lines to `width` in the text style being drawn; whether they changed.
    fn fit(&mut self, width: Pixels, window: &Window) -> bool {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        if self.fitted_to == Some((width, font_size)) {
            return false;
        }
        self.fitted_to = Some((width, font_size));
        let lines = fit_lines(&self.full, |line| {
            let run = style.to_run(line.len());
            let shaped =
                window
                    .text_system()
                    .shape_line(line.to_owned().into(), font_size, &[run], None);
            shaped.width <= width
        });
        let changed = lines != self.lines;
        self.lines = lines;
        changed
    }
}

fn fit_lines(text: &str, fits: impl Fn(&str) -> bool) -> Vec<SharedString> {
    text.lines().map(|l| fit_line(l, &fits).into()).collect()
}

/// `line` (`Label: value`) as it fits. Only the part that can lose its middle does: an object's
/// name, under its whole kind, and a server's host (or path), between its scheme and port.
fn fit_line(line: &str, fits: &impl Fn(&str) -> bool) -> String {
    let Some((label, value)) = line.split_once(": ") else {
        return fit_value("", line, fits);
    };
    let prefix = format!("{label}: ");
    let kept = match label {
        "Object" => fit_object(&prefix, value, fits),
        "Server" => fit_server(&prefix, value, fits),
        _ => None,
    };
    // Only when what must stay whole is itself wider than the line.
    kept.unwrap_or_else(|| fit_value(&prefix, value, fits))
}

/// `Object: kind name`, with the whole kind. With no room for a cut name, the name is only its
/// code. `None` if the kind and that code do not fit: the code is what tells two objects apart,
/// so it stays even at the cost of the kind's middle.
fn fit_object(prefix: &str, value: &str, fits: &impl Fn(&str) -> bool) -> Option<String> {
    let (kind, name) = value.split_once(' ')?;
    let start = format!("{prefix}{kind} ");
    fit_around(&start, name, "", 0, value, fits).or_else(|| {
        let code_only = format!("{start}#{}", value_code(value));
        fits(&code_only).then_some(code_only)
    })
}

/// `Server: scheme://host:port/path`, with the whole scheme and port. A long path loses its middle
/// while the host fits; after that the path is left out and the host loses its middle, keeping
/// its last two labels as far as they fit. A path left out always leaves the code of the whole
/// URL, since paths can route to different clusters. `None` for anything that is not such a URL.
fn fit_server(prefix: &str, value: &str, fits: &impl Fn(&str) -> bool) -> Option<String> {
    let (scheme, rest) = value.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    let port_at = authority
        .rfind(':')
        .filter(|&at| {
            let port = &authority[at + 1..];
            !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
        })
        .unwrap_or(authority.len());
    let (host, port) = authority.split_at(port_at);
    let start = format!("{prefix}{scheme}://");
    if !path.is_empty()
        && let Some(line) = fit_around(&format!("{start}{authority}"), path, "", 0, value, fits)
    {
        return Some(line);
    }
    let last_labels = host
        .rmatch_indices('.')
        .nth(1)
        .map_or(host, |(at, _)| &host[at..]);
    let min_tail = escaped_parts(last_labels).len();
    if path.is_empty() {
        fit_around(&start, host, port, min_tail, value, fits)
    } else {
        fit_cut(&start, host, &format!("{port}/…"), min_tail, value, fits)
    }
}

/// `prefix` and the most of `value` that `fits`, keeping its start and end around a short code of
/// the whole value: two long values that differ only in the part left out still read differently.
/// `value` is already escaped ([`flint_core::display::single_line`]); no escape is cut.
fn fit_value(prefix: &str, value: &str, fits: &impl Fn(&str) -> bool) -> String {
    fit_around(prefix, value, "", 0, value, fits)
        .unwrap_or_else(|| format!("{prefix}…#{}…", value_code(value)))
}

/// `prefix`, `middle` and `suffix` if they fit, or else [`fit_cut`].
fn fit_around(
    prefix: &str,
    middle: &str,
    suffix: &str,
    min_tail: usize,
    whole: &str,
    fits: &impl Fn(&str) -> bool,
) -> Option<String> {
    let uncut = format!("{prefix}{middle}{suffix}");
    if escaped_parts(middle).len() <= MAX_LINE_CHARS && fits(&uncut) {
        return Some(uncut);
    }
    fit_cut(prefix, middle, suffix, min_tail, whole, fits)
}

/// `prefix`, the most of `middle` that fits, and `suffix`, with a code of `whole` where `middle`
/// is cut. The end of `middle` keeps at least `min_tail` parts when that many fit. `None` if
/// `prefix`, the code and `suffix` alone do not fit.
fn fit_cut(
    prefix: &str,
    middle: &str,
    suffix: &str,
    min_tail: usize,
    whole: &str,
    fits: &impl Fn(&str) -> bool,
) -> Option<String> {
    let parts = escaped_parts(middle);
    let code = value_code(whole);
    let cut = |kept: usize| {
        let tail = (kept / 2).max(min_tail).min(kept.saturating_sub(1));
        format!(
            "{prefix}{}…#{code}…{}{suffix}",
            parts[..kept - tail].concat(),
            parts[parts.len() - tail..].concat()
        )
    };
    if !fits(&cut(0)) {
        return None;
    }
    // The most parts that fit, by bisection: at most `high` are kept.
    let (mut low, mut high) = (0, parts.len().saturating_sub(1).min(MAX_LINE_CHARS));
    while low < high {
        let mid = (low + high).div_ceil(2);
        if fits(&cut(mid)) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Some(cut(low))
}

/// `value` in the pieces that may be cut between: a character, or a whole escape.
fn escaped_parts(value: &str) -> Vec<&str> {
    let mut parts = Vec::with_capacity(value.len());
    let mut rest = value;
    while let Some(first) = rest.chars().next() {
        let len = if rest.starts_with("\\\\") {
            2
        } else if rest.starts_with("\\u{") {
            rest.find('}').map_or(first.len_utf8(), |end| end + 1)
        } else {
            first.len_utf8()
        };
        let (part, after) = rest.split_at(len);
        parts.push(part);
        rest = after;
    }
    parts
}

/// A short code of `value`, the same for the life of this process. Keyed per process, so a name
/// cannot be crafted ahead of time to show another name's code.
fn value_code(value: &str) -> String {
    static KEY: OnceLock<RandomState> = OnceLock::new();
    format!(
        "{:08x}",
        KEY.get_or_init(RandomState::new).hash_one(value) as u32
    )
}

/// An already escaped name as a dialog title quotes it: bounded, so the title leaves room for
/// the pinned lines, with its start and end kept.
pub fn title_name(name: &str) -> String {
    fit_value("", name, &|shown: &str| {
        shown.chars().count() <= TITLE_NAME_CHARS
    })
}

/// The lines that stay on show, each on one line, measured so the text under them takes only the
/// height left.
fn pinned_target(fitted: Rc<RefCell<FittedTarget>>, height: Rc<Cell<Pixels>>) -> impl IntoElement {
    let lines = fitted.borrow().lines.clone();
    div()
        .id("confirm-target")
        .relative()
        .flex_none()
        .w_full()
        .mb_2()
        .children(lines.into_iter().enumerate().map(|(i, line)| {
            div()
                .id(("confirm-target-line", i))
                .flex()
                .w_full()
                .overflow_hidden()
                .child(
                    div()
                        .id(("confirm-target-text", i))
                        .flex_none()
                        .whitespace_nowrap()
                        .aria_label(line.clone())
                        .child(line)
                        .test_support(),
                )
                .test_support()
        }))
        .child(
            canvas(
                move |bounds, window, _| {
                    let refit = fitted.borrow_mut().fit(bounds.size.width, window);
                    if height.replace(bounds.size.height) != bounds.size.height || refit {
                        window.on_next_frame(|window, _| window.refresh());
                    }
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        )
        .test_support()
}

/// Where `key` moves a text scrolled to `y` (zero or negative) of at most `max`, showing `page`
/// at a time; `None` for any other key.
fn scrolled_to(key: &Keystroke, y: f32, max: f32, page: f32) -> Option<f32> {
    if key.modifiers.modified() {
        return None;
    }
    let to = match key.key.as_str() {
        "up" => y + SCROLL_LINE,
        "down" => y - SCROLL_LINE,
        "pageup" => y + page,
        "pagedown" => y - page,
        "home" => 0.,
        "end" => -max,
        _ => return None,
    };
    Some(to.clamp(-max, 0.))
}

pub(super) fn scroll_by_key(
    scroll: &ScrollHandle,
    key: &Keystroke,
    window: &mut Window,
    cx: &mut App,
) {
    let offset = scroll.offset();
    let (max, page) = (scroll.max_offset().y, scroll.bounds().size.height);
    if let Some(y) = scrolled_to(key, offset.y.into(), max.into(), page.into()) {
        scroll.set_offset(point(offset.x, px(y)));
        window.refresh();
        cx.stop_propagation();
    }
}

/// The descriptions of the [`DialogText::default_cancel`] confirmations, while they are open
/// (each handle dies with its dialog).
#[derive(Default)]
struct DefaultCancels(Vec<WeakFocusHandle>);

impl Global for DefaultCancels {}

/// Makes Enter and Escape cancel a [`DialogText::default_cancel`] confirmation also while focus
/// is on the dialog's own node, outside [`DEFAULT_CANCEL`] (a click on the title-bar strip above
/// it puts it there), where its keys are off and they would do nothing. Call once at startup.
pub fn init(cx: &mut App) {
    cx.intercept_keystrokes(|event, window, cx| {
        let key = &event.keystroke;
        if !matches!(key.key.as_str(), "enter" | "escape") || key.modifiers.modified() {
            return;
        }
        let Some(cancels) = cx.try_global::<DefaultCancels>() else {
            return;
        };
        let open: Vec<FocusHandle> = cancels.0.iter().filter_map(|d| d.upgrade()).collect();
        // The dialog's node holds focus: an ancestor of its description, not the description.
        let description = open
            .into_iter()
            .find(|d| d.within_focused(window, cx) && !d.is_focused(window));
        cx.global_mut::<DefaultCancels>()
            .0
            .retain(|d| d.upgrade().is_some());
        if let Some(description) = description {
            cx.stop_propagation();
            description.dispatch_action(&gpui_kit::base::actions::Cancel, window, cx);
        }
    })
    .detach();
}

/// The buttons of a [`DialogText::default_cancel`] confirmation, as the dialog's own footer draws
/// them but under [`DEFAULT_CANCEL`], so Enter on a button Tab reached cancels too. They answer
/// from the description's `focus`, inside the dialog, whatever holds focus.
fn default_cancel_footer(dialog: &DialogText, focus: &FocusHandle) -> impl IntoElement {
    use gpui_kit::base::actions::{Cancel, Confirm};
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;

    use super::registry;

    div().w_full().key_context(DEFAULT_CANCEL).child(
        DialogFooter::new()
            .child(
                Button::new("cancel")
                    .label(dialog.cancel.clone())
                    .on_click(registry::on_click_in(focus.clone(), Cancel)),
            )
            .child(
                Button::new("ok")
                    .label(dialog.ok.clone())
                    .primary()
                    .on_click(registry::on_click_in(
                        focus.clone(),
                        Confirm { secondary: false },
                    )),
            ),
    )
}

/// The buttons of a gated confirmation with its note beside them. They answer from `focus`,
/// inside the dialog, whatever holds focus.
fn gated_footer(
    dialog: &DialogText,
    state: &ConfirmState,
    focus: &FocusHandle,
    cx: &App,
) -> impl IntoElement {
    use gpui_kit::base::actions::{Cancel, Confirm};
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::DialogFooter;
    use gpui_kit::component::{Disableable as _, h_flex};

    use super::registry;

    let color = if state.blocked {
        cx.theme().danger
    } else {
        cx.theme().muted_foreground
    };
    h_flex()
        .w_full()
        .items_center()
        .gap_3()
        .child(
            div()
                .id("confirm-footer-note")
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(color)
                .aria_label(state.note.clone())
                .child(state.note.clone())
                .test_support(),
        )
        .child(
            div().flex_none().child(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label(dialog.cancel.clone())
                            .on_click(registry::on_click_in(focus.clone(), Cancel)),
                    )
                    .child(
                        Button::new("ok")
                            .label(dialog.ok.clone())
                            .primary()
                            .disabled(state.blocked)
                            .on_click(registry::on_click_in(
                                focus.clone(),
                                Confirm { secondary: false },
                            )),
                    ),
            ),
        )
}

/// The confirmations opened per window, oldest first, as long as they are drawn. GPUI stacks
/// dialogs (a confirmation opened over another one), so this is a stack too.
#[cfg(any(test, feature = "automation"))]
#[derive(Default)]
struct Opened(std::collections::HashMap<WindowId, Vec<DialogText>>);

#[cfg(any(test, feature = "automation"))]
impl Global for Opened {}

#[cfg(any(test, feature = "automation"))]
fn remember(window: &Window, dialog: DialogText, cx: &mut App) {
    let id = window.window_handle().window_id();
    cx.default_global::<Opened>()
        .0
        .entry(id)
        .or_default()
        .push(dialog);
}

/// What the dialog open over `window` says, while one is open: the newest confirmation still
/// drawn in the last frame (its description, under its id, says its text). Those no longer
/// drawn were closed, and are forgotten.
#[cfg(any(test, feature = "automation"))]
pub fn shown(window: &mut Window, cx: &mut App) -> Option<DialogText> {
    use gpui_kit::test::TestWindowExt as _;

    let id = window.window_handle().window_id();
    if !window.has_active_dialog(cx) {
        if let Some(opened) = cx.try_global::<Opened>()
            && opened.0.contains_key(&id)
        {
            cx.global_mut::<Opened>().0.remove(&id);
        }
        return None;
    }
    let drawn = |dialog: &DialogText| {
        window
            .find_all(dialog.id)
            .iter()
            .any(|e| e.label() == Some(dialog.text.as_str()))
    };
    let stack = cx.try_global::<Opened>()?.0.get(&id)?;
    // Drawn now: those opened before the newest drawn one may still be open under it.
    let top = stack.iter().rposition(drawn)?;
    let shown = stack[top].clone();
    cx.global_mut::<Opened>()
        .0
        .entry(id)
        .or_default()
        .truncate(top + 1);
    Some(shown)
}

/// Asserts each of the `count` pinned lines is whole, inside the open dialog and above its
/// buttons, with the buttons on screen, and returns the lines as drawn.
#[cfg(test)]
pub(in crate::ui) fn assert_pinned_lines_fit(
    window: &mut Window,
    count: usize,
    height: Pixels,
) -> Vec<String> {
    use gpui_kit::test::TestWindowExt as _;

    let surface = window.find("surface").bounds();
    let (ok, cancel) = (window.find("ok").bounds(), window.find("cancel").bounds());
    for button in [ok, cancel] {
        assert!(
            button.origin.y >= surface.origin.y && button.bottom() <= surface.bottom().min(height),
            "{button:?} inside {surface:?}"
        );
    }
    assert!(window.try_find(("confirm-target-line", count)).is_none());
    (0..count)
        .map(|i| {
            let line = window.find(("confirm-target-line", i)).bounds();
            let text = window.find(("confirm-target-text", i));
            let drawn = text.bounds();
            assert!(line.size.height > px(0.), "line {i}: {line:?}");
            assert!(line.size.height < px(32.), "line {i} is one line: {line:?}");
            assert!(line.origin.y >= surface.origin.y, "{line:?} in {surface:?}");
            assert!(line.bottom() <= ok.origin.y, "{line:?} above {ok:?}");
            assert!(line.origin.x >= surface.origin.x, "{line:?} in {surface:?}");
            assert!(line.right() <= surface.right(), "{line:?} in {surface:?}");
            assert!(
                drawn.right() <= line.right(),
                "{drawn:?} clipped by {line:?}"
            );
            text.label()
                .expect("a pinned line says its text")
                .to_owned()
        })
        .collect()
}

/// Asserts Tab reaches the open dialog's scrolling text and End shows the last of it, where the
/// full target lines are.
#[cfg(test)]
pub(in crate::ui) fn assert_full_text_reachable(window: &mut Window, cx: &mut App) {
    use gpui_kit::test::TestWindowExt as _;

    for _ in 0..8 {
        window.render_frame(cx);
        if window.find("confirm-text").focused() == Some(true) {
            break;
        }
        window.press("tab", cx);
    }
    assert_eq!(window.find("confirm-text").focused(), Some(true));
    window.press("end", cx);
    window.render_frame(cx);
    window.render_frame(cx);
    let view = window.find("confirm-text").bounds();
    let body = window.find("confirm-text-body").bounds();
    assert!(
        body.origin.y < view.origin.y,
        "{body:?} scrolled in {view:?}"
    );
    assert!(
        body.bottom() <= view.bottom() + px(2.),
        "{body:?} ends in {view:?}"
    );
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, FocusHandle, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use super::{
        DialogText, escaped_parts, fit_line, open, scrolled_to, shown, split_pinned, title_name,
    };
    use crate::ui::registry;

    fn text(title: &str) -> DialogText {
        DialogText {
            title: title.into(),
            text: format!("{title} says this."),
            ok: "Go".into(),
            cancel: "Cancel".into(),
            id: "test-description",
            default_cancel: false,
        }
    }

    fn window(cx: &mut TestAppContext) -> AnyWindowHandle {
        cx.update(|cx| {
            gpui_kit::init(cx);
            registry::init(cx);
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(600.), px(400.)),
                    })),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| gpui_kit::Empty),
            )
            .unwrap()
            .0
        })
    }

    /// The shown dialog's title, in a freshly drawn frame.
    fn shown_title(cx: &mut TestAppContext, window: AnyWindowHandle) -> Option<String> {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            shown(window, cx).map(|d| d.title)
        })
        .unwrap()
    }

    fn open_text(cx: &mut TestAppContext, window: AnyWindowHandle, title: &str) {
        let dialog = text(title);
        cx.update_window(window, |_, window, cx| open(window, cx, dialog, |_, _| {}))
            .unwrap();
    }

    // The pinned lines are the last ones; what scrolls keeps the rest, without the blank line.
    #[test]
    fn the_last_lines_are_pinned() {
        let split = |pinned| split_pinned("warning\n\nContext: a\nObject: b", pinned);
        assert_eq!(
            split(2),
            ("warning".to_owned(), "Context: a\nObject: b".to_owned())
        );
        assert_eq!(split(0).1, "");
        assert_eq!(split(9).0, "");
    }

    // A shortened value keeps both ends and a code of the whole, and never halves an escape.
    #[test]
    fn a_long_value_keeps_both_ends_and_whole_escapes() {
        let fits = |line: &str| line.chars().count() <= 40;
        let long = |middle: &str| format!("start-{}{middle}{}-end", "x".repeat(99), "y".repeat(99));
        let one = fit_line(&format!("Context: {}", long("a")), &fits);
        let two = fit_line(&format!("Context: {}", long("b")), &fits);
        assert!(
            one.starts_with("Context: start") && one.ends_with("-end"),
            "{one}"
        );
        assert!(one.chars().count() <= 40, "{one}");
        assert_ne!(one, two);
        assert_eq!(fit_line("Server: short", &fits), "Server: short");

        let escapes = "\\u{202e}\\\\".repeat(50);
        assert_eq!(escaped_parts("a\\u{a}\\\\b"), ["a", "\\u{a}", "\\\\", "b"]);
        let cut = fit_line(&format!("Object: {escapes}"), &fits);
        let (head, rest) = cut["Object: ".len()..].split_once('…').unwrap();
        let tail = rest.split_once('…').unwrap().1;
        for side in [head, tail] {
            assert!(escaped_parts(side).iter().all(|p| p.len() > 1), "{cut}");
        }
        assert!(title_name(&long("a")).chars().count() <= 48);
        assert_eq!(title_name("web"), "web");
    }

    // A server keeps its scheme, port and the end of its host; an object its whole kind.
    #[test]
    fn identifying_parts_of_a_line_stay_whole() {
        let fits = |line: &str| line.chars().count() <= 52;
        let host = "s".repeat(300);
        let server = fit_line(
            &format!("Server: https://{host}.example.invalid:6443"),
            &fits,
        );
        assert!(server.starts_with("Server: https://s"), "{server}");
        assert!(server.ends_with(".example.invalid:6443"), "{server}");
        let ipv6 = fit_line(
            &format!("Server: https://[{}::1]:6443", "a:".repeat(99)),
            &fits,
        );
        assert!(ipv6.ends_with("::1]:6443"), "{ipv6}");
        let path = fit_line(
            &format!(
                "Server: https://api.example.invalid:6443/k8s/{}/end",
                "p".repeat(300)
            ),
            // Room for the host, the code and both ends of the path.
            &|line: &str| line.chars().count() <= 64,
        );
        assert!(
            path.starts_with("Server: https://api.example.invalid:6443/"),
            "{path}"
        );
        assert!(path.ends_with("/end"), "{path}");
        let long_both = fit_line(
            &format!(
                "Server: https://{host}.example.invalid:6443/{}",
                "p".repeat(300)
            ),
            &fits,
        );
        assert!(long_both.ends_with(":6443/…"), "{long_both}");
        let object = fit_line(
            &format!("Object: deployments.apps {}", "o".repeat(300)),
            &fits,
        );
        assert!(object.starts_with("Object: deployments.apps o"), "{object}");
        assert!(object.ends_with('o'), "{object}");
    }

    /// The width of a code with its two marks, and so the least a shortened line needs after its
    /// label.
    const CUT_CODE_CHARS: usize = 11;

    fn fit_to(line: &str, width: usize) -> String {
        fit_line(line, &|shown: &str| shown.chars().count() <= width)
    }

    /// Asserts every pair of `values` reads differently under `label`, at each width from the
    /// tightest a code fits in, and that each line stays within the width.
    fn assert_all_render_differently(label: &str, values: &[String]) {
        let tightest = label.chars().count() + 2 + CUT_CODE_CHARS;
        for width in tightest..=140 {
            let lines: Vec<String> = values
                .iter()
                .map(|value| fit_to(&format!("{label}: {value}"), width))
                .collect();
            for (i, line) in lines.iter().enumerate() {
                assert!(line.chars().count() <= width, "{width}: {line}");
                for (j, other) in lines.iter().enumerate().skip(i + 1) {
                    assert_ne!(
                        line, other,
                        "width {width}: {} and {}",
                        values[i], values[j]
                    );
                }
            }
        }
    }

    // Two clusters behind one gateway differ only in the path: with no room for the path's code,
    // the host gives up its middle so that the code stays.
    #[test]
    fn servers_differing_in_the_path_differ_when_the_authority_barely_fits() {
        let line = |path: &str| format!("Server: https://gw.example.invalid:6443{path}");
        let width = line("/…").chars().count();
        let one = fit_to(&line("/k8s/clusters/one"), width);
        let two = fit_to(&line("/k8s/clusters/two"), width);
        assert_ne!(one, two);
        for shown in [&one, &two] {
            assert!(shown.chars().count() <= width, "{shown}");
            assert!(shown.starts_with("Server: https://"), "{shown}");
            assert!(shown.contains("…#"), "{shown}");
            assert!(shown.ends_with(":6443/…"), "{shown}");
        }
    }

    // A kind that fits with room for the code alone stays whole, under a shorter placeholder.
    #[test]
    fn a_kind_that_fits_stays_whole_under_a_shorter_placeholder() {
        let start = "Object: widgets.storage.example.invalid ";
        // The code and its `#`, without the marks a cut name has around them.
        let width = start.chars().count() + CUT_CODE_CHARS - 2;
        let one = fit_to(&format!("{start}{}-one", "n".repeat(40)), width);
        let two = fit_to(&format!("{start}{}-two", "n".repeat(40)), width);
        assert_ne!(one, two);
        for shown in [&one, &two] {
            assert!(shown.starts_with(&format!("{start}#")), "{shown}");
            assert!(shown.chars().count() <= width, "{shown}");
        }
    }

    // Whatever the width, two different servers or objects never draw the same pinned line.
    #[test]
    fn distinct_targets_render_differently_at_every_width() {
        let long = "h".repeat(120);
        let servers: Vec<String> = [
            "https://gw.example.invalid:6443".to_owned(),
            "https://gw.example.invalid:6443/".to_owned(),
            "https://gw.example.invalid:6444".to_owned(),
            "http://gw.example.invalid:6443".to_owned(),
            "https://gw.example.invalid".to_owned(),
            "https://gw.example.invalid/k8s/clusters/one".to_owned(),
            "https://gw.example.invalid:6443/k8s/clusters/one".to_owned(),
            "https://gw.example.invalid:6443/k8s/clusters/two".to_owned(),
            "https://gw.example.invalid:6444/k8s/clusters/one".to_owned(),
            "https://gw.example.invalid:6443/k8s?cluster=one".to_owned(),
            "https://gw.example.invalid:6443/k8s?cluster=two".to_owned(),
            "https://[2001:db8::1]:6443/one".to_owned(),
            "https://[2001:db8::1]:6443/two".to_owned(),
            "https://a.io/x".to_owned(),
            "https://a.io/y".to_owned(),
            format!("https://{long}a{long}.example.invalid:6443/one"),
            format!("https://{long}b{long}.example.invalid:6443/one"),
            format!("https://{long}a{long}.example.invalid:6443/two"),
            format!("https://gw.example.invalid:6443/{long}a{long}"),
            format!("https://gw.example.invalid:6443/{long}b{long}"),
        ]
        .into();
        assert_all_render_differently("Server", &servers);

        let objects: Vec<String> = [
            "deployments.apps web-one".to_owned(),
            "deployments.apps web-two".to_owned(),
            "statefulsets.apps web-one".to_owned(),
            "widgets.one.example.invalid web".to_owned(),
            "widgets.two.example.invalid web".to_owned(),
            format!("widgets.storage.example.invalid {long}a{long}"),
            format!("widgets.storage.example.invalid {long}b{long}"),
            format!("widgets.{long}a.example.invalid web"),
            format!("widgets.{long}b.example.invalid web"),
        ]
        .into();
        assert_all_render_differently("Object", &objects);
    }

    // The keys move a long text within its range, and leave every other key alone.
    #[test]
    fn keys_scroll_a_long_text() {
        let to = |key: &'static str, y| {
            scrolled_to(&gpui_kit::Keystroke::parse(key).unwrap(), y, 100., 40.)
        };
        assert_eq!(to("down", 0.), Some(-20.));
        assert_eq!(to("up", 0.), Some(0.));
        assert_eq!(to("pagedown", -80.), Some(-100.));
        assert_eq!(to("pageup", -50.), Some(-10.));
        assert_eq!(to("home", -50.), Some(0.));
        assert_eq!(to("end", 0.), Some(-100.));
        assert_eq!(to("enter", 0.), None);
        assert_eq!(to("shift-down", 0.), None);
    }

    // The open confirmation says what it was opened with, and nothing once it closed.
    #[gpui_kit::test]
    async fn the_open_dialog_says_its_title_and_text(cx: &mut TestAppContext) {
        let window = window(cx);
        assert_eq!(shown_title(cx, window), None);
        let confirmed = Rc::new(Cell::new(0));
        let count = confirmed.clone();
        cx.update_window(window, |_, window, cx| {
            open(window, cx, text("First"), move |_, _| {
                count.set(count.get() + 1)
            })
        })
        .unwrap();
        assert_eq!(shown_title(cx, window).as_deref(), Some("First"));
        let dialog = cx
            .update_window(window, |_, window, cx| shown(window, cx))
            .unwrap()
            .unwrap();
        assert_eq!(dialog, text("First"));
        // The description is drawn under its id, labelled with its text.
        let label = cx
            .update_window(window, |_, window, _| {
                window
                    .try_find("test-description")
                    .and_then(|e| e.label().map(str::to_owned))
            })
            .unwrap();
        assert_eq!(label.as_deref(), Some("First says this."));

        cx.update_window(window, |_, window, cx| {
            registry::dispatch("ui::Confirm", serde_json::Value::Null, window, cx)
        })
        .unwrap()
        .unwrap();
        cx.run_until_parked();
        assert_eq!(confirmed.get(), 1);
        assert_eq!(shown_title(cx, window), None);

        open_text(cx, window, "Second");
        assert_eq!(shown_title(cx, window).as_deref(), Some("Second"));
    }

    // Enter confirms a confirmation, unless it is a `default_cancel` one: Enter cancels that,
    // and only its OK confirms it. Escape cancels both.
    #[gpui_kit::test]
    async fn enter_cancels_only_a_default_cancel_dialog(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;

        let window = window(cx);
        for (default_cancel, keys, confirms) in [
            (false, &["enter"][..], true),
            (false, &["escape"], false),
            (true, &["enter"], false),
            (true, &["escape"], false),
            (true, &["tab", "enter"], false),
            (true, &["shift-tab", "enter"], false),
        ] {
            let confirmed = Rc::new(Cell::new(false));
            let set = confirmed.clone();
            let dialog = DialogText {
                default_cancel,
                ..text("Asked")
            };
            cx.update_window(window, |_, window, cx| {
                open(window, cx, dialog, move |_, _| set.set(true))
            })
            .unwrap();
            assert_eq!(shown_title(cx, window).as_deref(), Some("Asked"));
            for key in keys {
                cx.update_window(window, |_, window, cx| window.press(key, cx))
                    .unwrap();
                // Tab and Shift-Tab reach Cancel and OK: the key that follows lands on a button.
                let expected = match *key {
                    "tab" => Some("cancel"),
                    "shift-tab" => Some("ok"),
                    _ => continue,
                };
                let focused = cx
                    .update_window(window, |_, window, cx| {
                        window.render_frame(cx);
                        ["ok", "cancel"]
                            .into_iter()
                            .find(|id| window.find(*id).focused() == Some(true))
                    })
                    .unwrap();
                assert_eq!(focused, expected, "{keys:?}: after {key}");
            }
            assert_eq!(shown_title(cx, window), None, "{keys:?} closes it");
            assert_eq!(
                confirmed.get(),
                confirms,
                "default_cancel {default_cancel}, {keys:?}"
            );
        }
        // Clicking its OK button confirms a default_cancel dialog.
        let confirmed = Rc::new(Cell::new(false));
        let set = confirmed.clone();
        let dialog = DialogText {
            default_cancel: true,
            ..text("Asked")
        };
        cx.update_window(window, |_, window, cx| {
            open(window, cx, dialog, move |_, _| set.set(true))
        })
        .unwrap();
        shown_title(cx, window);
        cx.update_window(window, |_, window, cx| window.click("ok", cx))
            .unwrap();
        assert_eq!(shown_title(cx, window), None);
        assert!(confirmed.get());
        // So does automation's Confirm.
        let confirmed = Rc::new(Cell::new(false));
        let set = confirmed.clone();
        let dialog = DialogText {
            default_cancel: true,
            ..text("Asked")
        };
        cx.update_window(window, |_, window, cx| {
            open(window, cx, dialog, move |_, _| set.set(true))
        })
        .unwrap();
        shown_title(cx, window);
        cx.update_window(window, |_, window, cx| {
            registry::dispatch("ui::Confirm", serde_json::Value::Null, window, cx)
        })
        .unwrap()
        .unwrap();
        cx.run_until_parked();
        assert!(confirmed.get());
    }

    fn focused(cx: &mut TestAppContext, window: AnyWindowHandle) -> Option<FocusHandle> {
        cx.update_window(window, |_, window, cx| window.focused(cx))
            .unwrap()
    }

    fn confirm_dialog(cx: &mut TestAppContext, window: AnyWindowHandle) {
        cx.update_window(window, |_, window, cx| {
            registry::dispatch("ui::Confirm", serde_json::Value::Null, window, cx)
        })
        .unwrap()
        .unwrap();
    }

    // Confirming returns focus at once, and nothing puts it back later over a newer target.
    #[gpui_kit::test]
    async fn focus_moved_after_a_confirmation_stays(cx: &mut TestAppContext) {
        let window = window(cx);
        let (earlier, newer) = cx.update(|cx| (cx.focus_handle(), cx.focus_handle()));
        cx.update_window(window, |_, window, cx| window.focus(&earlier, cx))
            .unwrap();
        open_text(cx, window, "Question");
        confirm_dialog(cx, window);
        cx.run_until_parked();
        assert_eq!(focused(cx, window), Some(earlier));

        cx.update_window(window, |_, window, cx| window.focus(&newer, cx))
            .unwrap();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(focused(cx, window), Some(newer));
    }

    // With nothing newly focused, focus is still where it was before the dialog later on.
    #[gpui_kit::test]
    async fn a_confirmation_returns_focus_when_nothing_took_it(cx: &mut TestAppContext) {
        let window = window(cx);
        let earlier = cx.update(|cx| cx.focus_handle());
        cx.update_window(window, |_, window, cx| window.focus(&earlier, cx))
            .unwrap();
        open_text(cx, window, "Question");
        confirm_dialog(cx, window);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(focused(cx, window), Some(earlier));
    }

    // An OK handler that moves focus keeps it there as the dialog closes.
    #[gpui_kit::test]
    async fn focus_moved_by_the_ok_handler_stays(cx: &mut TestAppContext) {
        let window = window(cx);
        let (earlier, target) = cx.update(|cx| (cx.focus_handle(), cx.focus_handle()));
        let moved_to = target.clone();
        cx.update_window(window, |_, window, cx| {
            window.focus(&earlier, cx);
            open(window, cx, text("Question"), move |window, cx| {
                window.focus(&moved_to, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();
        confirm_dialog(cx, window);
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(focused(cx, window), Some(target));
    }

    /// Clicks the newest dialog's `button` twice, with no frame drawn between the clicks.
    fn click_twice_before_repaint(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        button: &'static str,
    ) {
        use gpui_kit::{
            Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
        };

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let bounds = window
                .find_all(button)
                .last()
                .expect("the button is drawn")
                .bounds();
            let position = bounds.center();
            let modifiers = Modifiers::default();
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                }),
                cx,
            );
            window.render_frame(cx);
            for _ in 0..2 {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers,
                        click_count: 1,
                    }),
                    cx,
                );
            }
        })
        .unwrap();
        cx.run_until_parked();
    }

    // Two clicks on OK before the next frame run its handler once.
    #[gpui_kit::test]
    async fn a_second_ok_before_repaint_does_nothing(cx: &mut TestAppContext) {
        let window = window(cx);
        let runs = Rc::new(Cell::new(0));
        let count = runs.clone();
        cx.update_window(window, |_, window, cx| {
            open(window, cx, text("Once"), move |_, _| {
                count.set(count.get() + 1)
            })
        })
        .unwrap();
        click_twice_before_repaint(cx, window, "ok");
        assert_eq!(runs.get(), 1);
    }

    // An OK still on screen after the dialog was closed from code does nothing.
    #[gpui_kit::test]
    async fn an_ok_after_a_close_from_code_does_nothing(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::{
            Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput,
        };

        let window = window(cx);
        let runs = Rc::new(Cell::new(0));
        let count = runs.clone();
        cx.update_window(window, |_, window, cx| {
            open(window, cx, text("Once"), move |_, _| {
                count.set(count.get() + 1)
            });
            window.render_frame(cx);
            window.render_frame(cx);
            let position = window.find("ok").bounds().center();
            let modifiers = Modifiers::default();
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                }),
                cx,
            );
            window.render_frame(cx);
            window.close_dialog(cx);
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers,
                    click_count: 1,
                }),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(runs.get(), 0);
    }

    // Once its connection changed, a guarded dialog runs nothing, by OK or by Enter, and closes.
    #[gpui_kit::test]
    async fn a_guarded_dialog_cannot_be_confirmed_once_stale(cx: &mut TestAppContext) {
        use gpui_kit::test::TestWindowExt as _;

        use super::{close_stale, newest_is_stale, open_pinned_guarded};
        use crate::ui::write_binding::ConnectionGuard;

        let window = window(cx);
        for answer in ["ok", "enter", "close"] {
            let holds = Rc::new(Cell::new(true));
            let runs = Rc::new(Cell::new(0));
            let (check, count) = (holds.clone(), runs.clone());
            let guard = ConnectionGuard::from_check(move || check.get());
            cx.update_window(window, |_, window, cx| {
                open_pinned_guarded(window, cx, text("Bound"), 0, None, guard, move |_, _| {
                    count.set(count.get() + 1);
                    true
                })
            })
            .unwrap();
            assert_eq!(shown_title(cx, window).as_deref(), Some("Bound"));
            holds.set(false);
            cx.update_window(window, |_, window, cx| {
                assert!(newest_is_stale(window, cx));
                match answer {
                    "ok" => window.click("ok", cx),
                    "enter" => {
                        window.press("enter", cx);
                    }
                    _ => assert!(close_stale(window, cx)),
                }
            })
            .unwrap();
            assert_eq!(shown_title(cx, window), None, "{answer}");
            assert_eq!(runs.get(), 0, "{answer}");
        }
    }

    // A stale click on a dialog already answered leaves the dialog under it open.
    #[gpui_kit::test]
    async fn a_stale_click_keeps_the_dialog_underneath(cx: &mut TestAppContext) {
        for button in ["ok", "cancel"] {
            let window = window(cx);
            open_text(cx, window, "Under");
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            open_text(cx, window, "Over");
            click_twice_before_repaint(cx, window, button);
            assert_eq!(
                shown_title(cx, window).as_deref(),
                Some("Under"),
                "{button}"
            );
        }
    }

    // GPUI stacks dialogs: one opened over another is shown, and closing it shows the one
    // under it again.
    #[gpui_kit::test]
    async fn stacked_dialogs_are_shown_top_first(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;

        let window = window(cx);
        open_text(cx, window, "Under");
        open_text(cx, window, "Over");
        assert_eq!(shown_title(cx, window).as_deref(), Some("Over"));
        cx.update_window(window, |_, window, cx| window.close_dialog(cx))
            .unwrap();
        assert_eq!(shown_title(cx, window).as_deref(), Some("Under"));
        cx.update_window(window, |_, window, cx| window.close_dialog(cx))
            .unwrap();
        assert_eq!(shown_title(cx, window), None);
    }
}
