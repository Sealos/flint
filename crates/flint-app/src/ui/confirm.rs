//! Confirmation dialogs: a title, a sentence or two, and two buttons (Expose to AI, saving logs,
//! a multi-line paste). Every one opens through [`open`], so test builds can say what the open
//! dialog says (`shown`): GPUI keeps only a builder for a dialog, and nothing can read its text
//! back.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

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

/// The share of the window's height a confirmation's text may take before it scrolls, leaving
/// room for a wrapped title and the buttons at the minimum window size.
const DESCRIPTION_MAX_SHARE: f32 = 0.45;
/// The same for a confirmation with controls under its text (the drain options).
const DESCRIPTION_MAX_SHARE_OVER_CONTROLS: f32 = 0.3;
/// The share of the window's height the pinned lines and the text under them may take together.
const PINNED_DESCRIPTION_MAX_SHARE: f32 = 0.6;
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
    open_dialog(window, cx, dialog, pinned, None, move |window, cx| {
        on_ok(window, cx);
        true
    });
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
    open_dialog(window, cx, dialog, 0, content, on_ok);
}

fn open_dialog(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    pinned: usize,
    content: Option<AnyView>,
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
    let (scrolling, target) = (SharedString::from(scrolling), SharedString::from(target));
    let scroll = ScrollHandle::new();
    let text_focus = cx.focus_handle();
    let target_height = Rc::new(Cell::new(px(0.)));
    window.open_alert_dialog(cx, move |alert, window, cx| {
        let on_ok = on_ok.clone();
        // A long text scrolls, so it cannot push the buttons or its last lines out of the dialog.
        let viewport = window.viewport_size().height;
        let text_height = if pinned > 0 {
            (viewport * PINNED_DESCRIPTION_MAX_SHARE - target_height.get()).max(px(MIN_TEXT_HEIGHT))
        } else if content.is_some() {
            viewport * DESCRIPTION_MAX_SHARE_OVER_CONTROLS
        } else {
            viewport * DESCRIPTION_MAX_SHARE
        };
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
                    .when(!target.is_empty(), |this| {
                        this.child(pinned_target(target.clone(), target_height.clone()))
                    })
                    // Only the text scrolls: controls under it stay in place and clickable.
                    .when(!scrolling.is_empty(), |this| {
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
                                        .child(scrolling.clone())
                                        .test_support(),
                                ),
                        )
                    })
                    .when_some(content.clone(), |this, content| this.child(content))
                    .when_some(default_cancel.as_ref(), |this, focus| {
                        this.track_focus(focus).key_context(DEFAULT_CANCEL)
                    })
                    .test_support(),
            )
            .ok_text(dialog.ok.clone())
            .cancel_text(dialog.cancel.clone())
            .when(dialog.cancel.is_empty(), |this| this.show_cancel(false))
            .when_some(default_cancel.as_ref(), |this, focus| {
                this.keyboard(false)
                    .footer(default_cancel_footer(&dialog, focus))
            })
            .on_ok(move |_, window, cx| on_ok(window, cx))
    });
    // The dialog took focus as it opened: hand it on to the description.
    if let Some(focus) = focus {
        focus.focus(window, cx);
    }
    // Whether the text overflows is known once it is laid out.
    window.on_next_frame(|window, _| window.refresh());
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

/// The lines that stay on show, measured so the text under them takes only the height left.
fn pinned_target(target: SharedString, height: Rc<Cell<Pixels>>) -> impl IntoElement {
    div()
        .id("confirm-target")
        .relative()
        .flex_none()
        .mb_2()
        .child(target)
        .child(
            canvas(
                move |bounds, window, _| {
                    if height.replace(bounds.size.height) != bounds.size.height {
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

fn scroll_by_key(scroll: &ScrollHandle, key: &Keystroke, window: &mut Window, cx: &mut App) {
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

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Point, TestAppContext, WindowBounds,
        WindowOptions, px, size,
    };
    use std::cell::Cell;
    use std::rc::Rc;

    use super::{DialogText, open, scrolled_to, shown, split_pinned};
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
