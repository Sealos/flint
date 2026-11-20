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

/// The confirmations opened, the newest per window.
#[cfg(any(test, feature = "automation"))]
#[derive(Default)]
struct Opened(std::collections::HashMap<WindowId, DialogText>);

#[cfg(any(test, feature = "automation"))]
impl Global for Opened {}

#[cfg(any(test, feature = "automation"))]
fn remember(window: &Window, dialog: DialogText, cx: &mut App) {
    let id = window.window_handle().window_id();
    cx.default_global::<Opened>().0.insert(id, dialog);
}

/// What the dialog open over `window` says, while one is open. Confirmations do not stack (each
/// is modal), so the open one is the newest opened.
#[cfg(any(test, feature = "automation"))]
pub fn shown(window: &mut Window, cx: &mut App) -> Option<DialogText> {
    if !window.has_active_dialog(cx) {
        return None;
    }
    let id = window.window_handle().window_id();
    cx.try_global::<Opened>()
        .and_then(|opened| opened.0.get(&id).cloned())
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
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

    fn shown_title(cx: &mut TestAppContext, window: AnyWindowHandle) -> Option<String> {
        cx.update_window(window, |_, window, cx| shown(window, cx).map(|d| d.title))
            .unwrap()
    }

    // The open confirmation says what it was opened with, and nothing once it closed; the
    // newest one is the one shown.
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
        cx.run_until_parked();
        let dialog = cx
            .update_window(window, |_, window, cx| shown(window, cx))
            .unwrap()
            .unwrap();
        assert_eq!(dialog, text("First"));
        // The description is drawn under its id, labelled with its text.
        let label = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
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
        cx.wait_for(window, super::super::support_tests::WAIT, |window, cx| {
            shown(window, cx).is_none()
        })
        .await;

        cx.update_window(window, |_, window, cx| {
            open(window, cx, text("Second"), |_, _| {})
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(shown_title(cx, window).as_deref(), Some("Second"));
    }
}
