//! A strip that states something about the content below it (stale rows, a
//! limited list, a deleted object), used by the list and the detail view alike.

// module-tests: list_tests.rs (the Reconnecting and Failed view tests render these banners)

use gpui_kit::base::TestTextExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;

/// The most lines a notice or a caption wraps to: enough for a server's sentence in a narrow
/// pane. Only a longer message is cut, and its tooltip still has all of it.
pub(in crate::ui) const MAX_NOTICE_LINES: usize = 6;

/// A tinted strip in `color` with a plain `headline` and, under it, `detail` (what a server
/// said) in full, wrapping as long as it needs: a long RBAC message keeps its user, verb and
/// namespace readable in a narrow pane. `control` (a button) sits at its end.
pub fn notice_detail(
    id: impl Into<ElementId>,
    headline: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    color: Hsla,
    muted: Hsla,
    control: impl IntoElement,
) -> Stateful<Div> {
    let (headline, detail): (SharedString, SharedString) = (headline.into(), detail.into());
    div()
        .id(id)
        .px_3()
        .py_1()
        .flex_none()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .bg(color.opacity(0.08))
        .border_b_1()
        .border_color(color.opacity(0.3))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .text_color(color)
                        .child(headline.clone())
                        .test_text(headline),
                )
                .child(
                    div()
                        .text_color(muted)
                        .child(detail.clone())
                        .test_text(detail),
                ),
        )
        .child(div().flex_none().child(control))
}

/// A tinted strip in `color` with `text` wrapped to at most [`MAX_NOTICE_LINES`] lines; the
/// full text is in a tooltip, so a long server message is readable without widening the pane.
pub fn notice(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    color: Hsla,
) -> Stateful<Div> {
    let text: SharedString = text.into();
    let full = text.clone();
    div()
        .id(id)
        .px_3()
        .py_1()
        .flex_none()
        .text_xs()
        .text_color(color)
        .bg(color.opacity(0.08))
        .border_b_1()
        .border_color(color.opacity(0.3))
        .child(div().line_clamp(MAX_NOTICE_LINES).child(text))
        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
}

/// A [`notice`] with a control at its end (a Reconnect or Restart button).
pub fn notice_with(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    color: Hsla,
    control: impl IntoElement,
) -> Stateful<Div> {
    let text: SharedString = text.into();
    let full = text.clone();
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .flex_none()
        .text_xs()
        .text_color(color)
        .bg(color.opacity(0.08))
        .border_b_1()
        .border_color(color.opacity(0.3))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .line_clamp(MAX_NOTICE_LINES)
                .child(text)
                .id("notice-text")
                .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx)),
        )
        .child(div().flex_none().child(control))
}

#[cfg(test)]
mod tests {
    // Named, not a glob: `gpui_kit::*` would make the `#[test]` the macro expands to its own.
    use super::{notice, notice_with};
    use gpui_kit::base::TestSupportExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
        Render, Styled as _, TestAppContext, Window, div, px,
    };

    const BANNER: &str = "Reconnecting. These rows may be out of date: connection refused by \
                          https://203.0.113.10:6443; is the API server running, and is the port \
                          right?";

    struct Narrow;

    impl Render for Narrow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let color = Hsla::default();
            div()
                .w(px(220.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .id("whole-text")
                        .test_support()
                        .text_xs()
                        .child(BANNER),
                )
                .child(notice("plain-notice", BANNER, color).test_support())
                .child(
                    notice_with("control-notice", BANNER, color, div().w(px(48.)).h(px(16.)))
                        .test_support(),
                )
        }
    }

    // #424: a reconnect banner in a narrowed list keeps its last words.
    #[gpui_kit::test]
    fn a_long_notice_grows_instead_of_ending_mid_word(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Narrow);
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        let height = |cx: &mut TestAppContext, id: &'static str| {
            cx.update_window(window.into(), |_, window, _| {
                window.find(id).bounds().size.height
            })
            .unwrap()
        };
        let whole = height(cx, "whole-text");
        assert!(whole > px(32.), "the text needs over two lines: {whole:?}");
        assert!(height(cx, "plain-notice") >= whole, "the strip fits it");
        // Beside a control the text has less room, so it needs at least as many lines.
        assert!(
            height(cx, "control-notice") >= whole,
            "and beside a control"
        );
    }
}
