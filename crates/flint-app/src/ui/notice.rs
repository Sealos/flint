//! A strip that states something about the content below it (stale rows, a
//! limited list, a deleted object), used by the list and the detail view alike.

// module-tests: list_tests.rs (the Reconnecting and Failed view tests render these banners)

use gpui_kit::base::TestTextExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;

/// The most lines a notice or a caption wraps to: enough for a server's sentence in a narrow
/// pane. A longer message ends in an ellipsis, and its tooltip still has all of it.
const MAX_NOTICE_LINES: usize = 6;

/// The most of its pane's height a notice takes, so that the content under it stays visible
/// however short the pane gets. A taller message scrolls inside the strip.
const MAX_PANE_SHARE: f32 = 0.25;
/// A strip with one line of text or an extra-small control: the least a notice shrinks to.
const ONE_LINE_HEIGHT: Pixels = px(29.);
/// A [`notice_detail`] with its headline and one line of detail.
const TWO_LINE_HEIGHT: Pixels = px(50.);

/// `text` wrapped to at most [`MAX_NOTICE_LINES`] lines, with an ellipsis if that cuts it.
pub(in crate::ui) fn clamped(text: Div) -> Div {
    text.line_clamp(MAX_NOTICE_LINES).text_ellipsis()
}

/// The tinted strip every notice is drawn in, at least `least` tall and within its share of
/// the pane.
fn strip(id: impl Into<ElementId>, color: Hsla, least: Pixels) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .flex_none()
        .min_h(least)
        .max_h(relative(MAX_PANE_SHARE))
        .overflow_y_scroll()
        .text_xs()
        .bg(color.opacity(0.08))
        .border_b_1()
        .border_color(color.opacity(0.3))
}

/// A strip's text beside its control. Inside the strip rather than the strip itself, so that
/// centring cannot push the top of a tall message out of the scroll range.
fn with_control(text: impl IntoElement, control: impl IntoElement) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(text)
        .child(div().flex_none().child(control))
}

/// A tinted strip in `color` with a plain `headline` and, under it, `detail` (what a server
/// said) in full, wrapping as long as its share of the pane allows and scrolling past that: a
/// long RBAC message keeps its user, verb and namespace readable in a narrow pane. `control`
/// (a button) sits at its end.
pub fn notice_detail(
    id: impl Into<ElementId>,
    headline: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    color: Hsla,
    muted: Hsla,
    control: impl IntoElement,
) -> Stateful<Div> {
    let (headline, detail): (SharedString, SharedString) = (headline.into(), detail.into());
    strip(id, color, TWO_LINE_HEIGHT).child(with_control(
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
        control,
    ))
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
    strip(id, color, ONE_LINE_HEIGHT)
        .text_color(color)
        .child(clamped(div()).child(text))
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
    strip(id, color, ONE_LINE_HEIGHT)
        .text_color(color)
        .child(with_control(
            clamped(div().flex_1().min_w_0())
                .child(text)
                .id("notice-text")
                .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx)),
            control,
        ))
}

#[cfg(test)]
mod tests {
    // Named, not a glob: `gpui_kit::*` would make the `#[test]` the macro expands to its own.
    use super::{MAX_NOTICE_LINES, clamped, notice, notice_with};
    use gpui_kit::base::TestSupportExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AppContext as _, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
        Render, Styled as _, StyledText, TestAppContext, Window, div, px,
    };

    const BANNER: &str = "Reconnecting. These rows may be out of date: connection refused by \
                          https://203.0.113.10:6443; is the API server running, and is the port \
                          right?";
    const TOOLBAR: f32 = 36.;

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

    /// A pane as short as the dock may leave it, with a toolbar, `banners` long notices and a
    /// body that takes what is left.
    struct Pane {
        height: f32,
        banners: usize,
    }

    impl Render for Pane {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let long = BANNER.repeat(6);
            div()
                .w(px(220.))
                .h(px(self.height))
                .flex()
                .flex_col()
                .child(div().flex_none().h(px(TOOLBAR)))
                .children((0..self.banners).map(|ix| {
                    notice(("budget-notice", ix), long.clone(), Hsla::default()).test_support()
                }))
                .child(div().id("pane-body").test_support().flex_1().min_h_0())
        }
    }

    // #424: at the shortest pane the dock leaves (160 or 280), long notices leave the body a
    // header and a row.
    #[gpui_kit::test]
    fn long_notices_leave_the_body_room(cx: &mut TestAppContext) {
        for (height, banners) in [(160., 1), (280., 1), (280., 2)] {
            let window = cx.add_window(|_, _| Pane { height, banners });
            cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
                .unwrap();
            let body = cx
                .update_window(window.into(), |_, window, _| {
                    window.find("pane-body").bounds().size.height
                })
                .unwrap();
            assert!(
                body >= px(48.),
                "{banners} notices in {height}: body {body:?}"
            );
        }
    }

    struct Clamped(Option<StyledText>);

    impl Render for Clamped {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .w(px(120.))
                .child(clamped(div()).children(self.0.take()))
        }
    }

    // #424: text past the last line ends in a marker, so it does not read as complete.
    #[gpui_kit::test]
    fn clamped_text_marks_what_it_cuts(cx: &mut TestAppContext) {
        let text = StyledText::new(BANNER.repeat(4));
        let layout = text.layout().clone();
        let window = cx.add_window(|_, _| Clamped(Some(text)));
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        let lines: usize = layout
            .line_layouts()
            .iter()
            .map(|line| line.wrap_boundaries().len() + 1)
            .sum();
        assert!(lines <= MAX_NOTICE_LINES, "{lines} lines");
        assert!(layout.text().ends_with('…'), "{}", layout.text());
    }
}
