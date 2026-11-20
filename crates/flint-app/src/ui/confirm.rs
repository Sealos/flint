//! Confirmation dialogs: a title, a sentence or two, and two buttons (Expose to AI, saving logs,
//! a multi-line paste). Every one opens through [`open`], so test builds can say what the open
//! dialog says (`shown`): GPUI keeps only a builder for a dialog, and nothing can read its text
//! back.

use std::rc::Rc;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;

/// What a confirmation says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogText {
    pub title: String,
    pub text: String,
    pub ok: String,
    pub cancel: String,
    /// The element id of the description, for view tests.
    pub id: &'static str,
}

/// Opens a confirmation saying `dialog`; confirming runs `on_ok` and closes it. Test builds open
/// it at once, without the entrance animation, so automation sees it without waiting.
pub fn open(
    window: &mut Window,
    cx: &mut App,
    dialog: DialogText,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    #[cfg(any(test, feature = "automation"))]
    remember(window, dialog.clone(), cx);
    let on_ok = Rc::new(on_ok);
    window.open_alert_dialog(cx, move |alert, _, _| {
        let on_ok = on_ok.clone();
        #[cfg(feature = "automation")]
        let alert = alert.entrance(gpui_kit::component::dialog::DialogEntrance::None);
        alert
            .confirm()
            .title(dialog.title.clone())
            .description(
                div()
                    .id(dialog.id)
                    .aria_label(dialog.text.clone())
                    .child(dialog.text.clone())
                    .test_support(),
            )
            .ok_text(dialog.ok.clone())
            .cancel_text(dialog.cancel.clone())
            .on_ok(move |_, window, cx| {
                on_ok(window, cx);
                true
            })
    });
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

    use super::{DialogText, open, shown};
    use crate::ui::registry;

    fn text(title: &str) -> DialogText {
        DialogText {
            title: title.into(),
            text: format!("{title} says this."),
            ok: "Go".into(),
            cancel: "Cancel".into(),
            id: "test-description",
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
