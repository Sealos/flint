//! Focus housekeeping for views that close or hide others.

use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, Window};

/// Lets the window drop the text input it last saw focused, once focus has moved on from it.
///
/// gpui-component's window state keeps a strong handle to the focused text field (a list's
/// filter, a log tab's search, a detail pane's YAML) and forgets it only when that field draws again without focus,
/// or when something asks which field has focus. A field closed with its view, or hidden
/// with its dock tab, never draws again, so without this it stays alive with its text until
/// another field takes focus. Call after moving focus away from a view that closed or went
/// out of sight; a field that still has focus is kept.
///
/// `window` must be one gpui-kit opened (`gpui_kit::open_window`), which installs the root that
/// holds that state; in a bare test window this panics.
pub fn release_unfocused_input(window: &mut Window, cx: &mut App) {
    let _ = window.has_focused_input(cx);
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::input::{Input, InputState};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Context, Entity, FocusHandle, Focusable as _, IntoElement,
        ParentElement as _, Render, TestAppContext, Window, WindowOptions, div,
    };

    use super::release_unfocused_input;

    /// A window that draws one text field until it is taken away.
    struct Holder {
        input: Option<Entity<InputState>>,
        focus: FocusHandle,
    }

    impl Render for Holder {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().children(self.input.as_ref().map(Input::new))
        }
    }

    // A field taken out of the window while focused is freed once focus moves on and the
    // window is asked; while it has focus it is kept.
    #[gpui_kit::test]
    fn a_closed_field_goes_once_focus_moved_on(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (window, holder) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let input = cx.new(|cx| InputState::new(window, cx));
                cx.new(|cx| Holder {
                    input: Some(input),
                    focus: cx.focus_handle(),
                })
            })
            .unwrap()
        });
        let draw = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        draw(cx);
        let field = holder.read_with(cx, |h, _| h.input.clone().unwrap());
        cx.update_window(window, |_, window, cx| {
            let focus = field.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
        })
        .unwrap();
        draw(cx);
        let weak = field.downgrade();
        drop(field);
        holder.update(cx, |h, cx| {
            h.input = None;
            cx.notify();
        });
        draw(cx);

        cx.update_window(window, |_, window, cx| release_unfocused_input(window, cx))
            .unwrap();
        assert!(weak.upgrade().is_some(), "kept while it has focus");

        let other = holder.read_with(cx, |h, _| h.focus.clone());
        cx.update_window(window, |_, window, cx| {
            window.focus(&other, cx);
            release_unfocused_input(window, cx);
        })
        .unwrap();
        cx.run_until_parked();
        weak.assert_released();
    }
}
