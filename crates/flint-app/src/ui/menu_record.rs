//! What a menu was built with, for the automation state. GPUI keeps a popup menu's items to
//! itself, and a drawn menu cannot tell a note from a disabled entry, so the code that builds a
//! menu records its entries here, by the menu's entity: what was built, and stays on screen
//! until the menu is dismissed or built again. Release builds record nothing.

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::*;

/// An entry a menu offers (not its notes or separators).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(any(test, feature = "automation")), allow(dead_code))]
pub struct Entry {
    pub label: String,
    pub enabled: bool,
    /// A toggle's state; `None` for an entry that is not one.
    pub checked: Option<bool>,
}

impl Entry {
    pub fn new(label: impl Into<String>, enabled: bool) -> Self {
        Entry {
            label: label.into(),
            enabled,
            checked: None,
        }
    }

    pub fn toggle(label: impl Into<String>, on: bool) -> Self {
        Entry {
            label: label.into(),
            enabled: true,
            checked: Some(on),
        }
    }
}

/// Records that the menu `cx` builds, opened from `owner` (the control's element id), offers
/// `entries`. `entries` is only called in test builds.
#[cfg_attr(not(any(test, feature = "automation")), allow(unused_variables))]
pub fn record(
    cx: &mut Context<PopupMenu>,
    owner: &'static str,
    entries: impl FnOnce() -> Vec<Entry>,
) {
    #[cfg(any(test, feature = "automation"))]
    {
        let menu = cx.weak_entity();
        let built = cx.default_global::<Built>();
        // Forget the menus that are gone.
        built.0.retain(|(menu, _, _)| menu.upgrade().is_some());
        built
            .0
            .retain(|(other, _, _)| other.entity_id() != menu.entity_id());
        built.0.push((menu, owner, entries()));
    }
}

/// The menus built, while they live.
#[cfg(any(test, feature = "automation"))]
#[derive(Default)]
struct Built(Vec<(WeakEntity<PopupMenu>, &'static str, Vec<Entry>)>);

#[cfg(any(test, feature = "automation"))]
impl Global for Built {}

/// What the menu drawn as entity `menu` was last built with: its owner and entries.
#[cfg(any(test, feature = "automation"))]
pub fn built(menu: EntityId, cx: &App) -> Option<(&'static str, Vec<Entry>)> {
    cx.try_global::<Built>()?
        .0
        .iter()
        .rev()
        .find(|(m, _, _)| m.entity_id() == menu && m.upgrade().is_some())
        .map(|(_, owner, entries)| (*owner, entries.clone()))
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::component::menu::PopupMenu;

    use super::{Entry, built, record};

    // A menu's entries are kept while it lives, the last build replacing the one before.
    #[gpui_kit::test]
    fn a_menu_is_known_by_what_it_was_built_with(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let window = cx.add_empty_window();
        let menu = window.update(|window, cx| {
            PopupMenu::build(window, cx, |menu, _, cx| {
                record(cx, "test-menu", || vec![Entry::new("One", true)]);
                menu
            })
        });
        let id = menu.entity_id();
        assert_eq!(
            cx.update(|cx| built(id, cx)),
            Some(("test-menu", vec![Entry::new("One", true)]))
        );
        menu.update(cx, |_, cx| {
            record(cx, "test-menu", || vec![Entry::toggle("Two", false)])
        });
        assert_eq!(
            cx.update(|cx| built(id, cx)),
            Some(("test-menu", vec![Entry::toggle("Two", false)]))
        );
        drop(menu);
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| built(id, cx)), None);
    }
}
