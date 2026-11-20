//! A one-line-high strip that states something about the content below it (stale rows, a
//! limited list, a deleted object), used by the list and the detail view alike.

// module-tests: list_tests.rs (the Reconnecting and Failed view tests render these banners)

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;

/// A tinted strip in `color` with `text` wrapped to at most two lines; the full text is in a
/// tooltip, so a long server message is readable without widening the pane.
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
        .child(div().line_clamp(2).child(text))
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
                .line_clamp(2)
                .child(text)
                .id("notice-text")
                .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx)),
        )
        .child(div().flex_none().child(control))
}
