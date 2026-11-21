//! Keeps context menus inside the window. gpui-component's menu items neither wrap nor shrink,
//! and a menu only scrolls when told to, so in a narrow or short window a long container name or
//! reason could pass the window's edge.

use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;

use super::registry::WORKSPACE;

/// The element holding a wrapped entry's text, inside its menu item. A custom item has no
/// accessible name of its own: it takes it from this label's value.
pub(in crate::ui) const MENU_NOTE: &str = "row-menu-note";

/// What a menu keeps clear of the window's edges, in all.
const WINDOW_MARGIN: Pixels = px(16.);
/// gpui-component's default menu width.
const MENU_WIDTH: Rems = rems(31.25);
/// How wide a container picker may grow, to show full names on one line when there is room.
const PICKER_WIDTH: Pixels = px(1000.);
/// A menu's own padding and its items', around their text.
const ROW_PADDING: Rems = rems(2.);
/// How far a picker's rows start in. gpui-component snaps a submenu to the window's edge, then
/// moves it 1 rem further left when it opens on that side, past the edge by this much at most:
/// its border and padding can go there, and the text must not.
const SUBMENU_OVERHANG: Rems = rems(0.75);

/// The widest a menu of at most `widest` can be in this window.
fn fitted(widest: Pixels, window: &Window) -> Pixels {
    widest.min(window.viewport_size().width - WINDOW_MARGIN)
}

fn menu_width(window: &Window) -> Pixels {
    fitted(MENU_WIDTH.to_pixels(window.rem_size()), window)
}

fn text_width(menu: Pixels, window: &Window) -> Pixels {
    (menu - ROW_PADDING.to_pixels(window.rem_size())).max(px(0.))
}

fn tallest(window: &Window) -> Pixels {
    window.viewport_size().height - WINDOW_MARGIN
}

/// A menu that stays inside the window, and scrolls when it is taller than it.
pub(in crate::ui) fn fit_menu(menu: PopupMenu, window: &Window) -> PopupMenu {
    menu.max_w(menu_width(window))
        .max_h(tallest(window))
        .scrollable(true)
}

/// A container picker: wide enough for full names when there is room (names that differ only in
/// the middle must stay apart), and inside the window when there is not, where its rows wrap.
pub(in crate::ui) fn fit_picker(menu: PopupMenu, window: &Window) -> PopupMenu {
    menu.max_w(fitted(PICKER_WIDTH, window))
        .max_h(tallest(window))
        .scrollable(true)
}

/// How wide a note in a [`fit_menu`] menu is before it wraps.
pub(in crate::ui) fn note_width(window: &Window) -> Pixels {
    text_width(menu_width(window), window)
}

/// `text` as a label that wraps, for a menu item.
pub(in crate::ui) fn wrapped_label(text: SharedString) -> impl IntoElement + Styled {
    div()
        .id(MENU_NOTE)
        .test_support()
        .role(Role::Label)
        .aria_value(text.clone())
        .min_w_0()
        .child(text)
}

/// A container picker's row: its text wraps when space runs out, and the key of `action`, if it
/// has one, stays whole beside it.
pub(in crate::ui) fn picker_item(text: String, action: Option<Box<dyn Action>>) -> PopupMenuItem {
    let text = SharedString::from(text);
    PopupMenuItem::element(move |window, cx| {
        let key = action
            .as_ref()
            .and_then(|a| Kbd::binding_for_action(a.as_ref(), Some(WORKSPACE), window))
            .map(|key| {
                key.p_0()
                    .flex_none()
                    .flex_nowrap()
                    .border_0()
                    .bg(cx.theme().transparent)
            });
        let inset = SUBMENU_OVERHANG.to_pixels(window.rem_size());
        h_flex()
            .pl(inset)
            .max_w(text_width(fitted(PICKER_WIDTH, window), window))
            .gap_3()
            .justify_between()
            .child(wrapped_label(text.clone()))
            .children(key)
    })
}

#[cfg(test)]
#[path = "menu_fit_tests.rs"]
mod tests;
