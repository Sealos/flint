//! Where the arrow keys have put the highlight in a row's open menu.
//!
//! gpui-component's popup menu keeps its highlighted entry private and forgets it when the menu
//! is rebuilt, which a live update does while the menu is open (the object loaded, the pods
//! arrived). This mirrors the menu's own up and down rules over the entries the menu shows, so
//! after a rebuild the highlight can be put back on the same entry: by its label, not its place,
//! since entries come and go above it.

/// An entry as the cursor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) struct CursorEntry {
    pub label: String,
    /// A disabled entry is skipped by the arrow keys.
    pub enabled: bool,
}

#[derive(Debug, Default)]
pub(in crate::ui) struct MenuCursor {
    entries: Vec<CursorEntry>,
    /// Index into `entries` of the highlighted entry; always an enabled one.
    at: Option<usize>,
    /// The pointer has moved over the menu, which highlights what it passes over without the
    /// cursor seeing it: the cursor cannot tell where the highlight is.
    lost: bool,
}

impl MenuCursor {
    pub fn new(entries: Vec<CursorEntry>) -> Self {
        MenuCursor {
            entries,
            at: None,
            lost: false,
        }
    }

    pub fn label(&self) -> Option<&str> {
        self.at.map(|ix| self.entries[ix].label.as_str())
    }

    fn enabled(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        (0..self.entries.len()).filter(|&ix| self.entries[ix].enabled)
    }

    /// Gives up following the highlight for this menu.
    pub fn lose(&mut self) {
        self.at = None;
        self.lost = true;
    }

    /// The Down key: the next enabled entry, the first from none or after the last.
    pub fn down(&mut self) {
        if self.lost {
            return;
        }
        let next = self.at.and_then(|at| self.enabled().find(|&ix| ix > at));
        self.at = next.or_else(|| self.enabled().next());
    }

    /// The Up key: the previous enabled entry, the last from none or before the first.
    pub fn up(&mut self) {
        if self.lost {
            return;
        }
        let prev = self
            .at
            .and_then(|at| self.enabled().rev().find(|&ix| ix < at));
        self.at = prev.or_else(|| self.enabled().next_back());
    }

    /// The menu was rebuilt with `entries`. Returns how many Down presses from a fresh menu
    /// highlight the entry that was highlighted, or none when there was none or it is gone or
    /// disabled now.
    pub fn rebuilt(&mut self, entries: Vec<CursorEntry>) -> Option<usize> {
        let label = self.label().filter(|_| !self.lost).map(str::to_owned);
        self.entries = entries;
        self.at =
            label.and_then(|label| self.enabled().find(|&ix| self.entries[ix].label == label));
        let at = self.at?;
        Some(self.enabled().take_while(|&ix| ix <= at).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(spec: &[&str]) -> Vec<CursorEntry> {
        // A leading `-` marks a disabled entry.
        spec.iter()
            .map(|s| CursorEntry {
                label: s.trim_start_matches('-').to_owned(),
                enabled: !s.starts_with('-'),
            })
            .collect()
    }

    #[test]
    fn down_starts_at_the_first_enabled_entry_and_wraps() {
        let mut c = MenuCursor::new(entries(&["-Logs", "Shell", "Scale", "Delete"]));
        assert_eq!(c.label(), None);
        c.down();
        assert_eq!(c.label(), Some("Shell"));
        c.down();
        c.down();
        assert_eq!(c.label(), Some("Delete"));
        c.down();
        assert_eq!(c.label(), Some("Shell"), "wraps past the last");
    }

    #[test]
    fn up_starts_at_the_last_enabled_entry_and_wraps() {
        let mut c = MenuCursor::new(entries(&["Logs", "Shell", "-Delete"]));
        c.up();
        assert_eq!(c.label(), Some("Shell"));
        c.up();
        assert_eq!(c.label(), Some("Logs"));
        c.up();
        assert_eq!(c.label(), Some("Shell"), "wraps past the first");
    }

    #[test]
    fn a_menu_with_nothing_enabled_has_no_cursor() {
        let mut c = MenuCursor::new(entries(&["-Logs"]));
        c.down();
        assert_eq!(c.label(), None);
        c.up();
        assert_eq!(c.label(), None);
    }

    #[test]
    fn a_rebuilt_menu_keeps_the_entry_by_label_when_entries_move() {
        let mut c = MenuCursor::new(entries(&["Logs", "Shell", "Delete"]));
        c.down();
        c.down();
        assert_eq!(c.label(), Some("Shell"));
        // Logs is disabled now and Scale arrived above Delete: Shell is the first enabled.
        let presses = c.rebuilt(entries(&["-Logs", "Shell", "Scale", "Delete"]));
        assert_eq!(presses, Some(1));
        assert_eq!(c.label(), Some("Shell"));
        c.down();
        assert_eq!(c.label(), Some("Scale"), "the arrows go on from it");
    }

    #[test]
    fn the_presses_to_replay_count_enabled_entries_up_to_it() {
        let mut c = MenuCursor::new(entries(&["Logs", "Shell", "Delete"]));
        c.up();
        assert_eq!(c.label(), Some("Delete"));
        let presses = c.rebuilt(entries(&["Logs", "-Shell", "Scale", "Delete"]));
        assert_eq!(presses, Some(3));
    }

    #[test]
    fn a_rebuilt_menu_without_the_entry_or_with_it_disabled_clears_the_cursor() {
        let mut c = MenuCursor::new(entries(&["Logs", "Shell"]));
        c.down();
        c.down();
        assert_eq!(c.rebuilt(entries(&["Logs"])), None);
        assert_eq!(c.label(), None);

        let mut c = MenuCursor::new(entries(&["Logs", "Shell"]));
        c.down();
        assert_eq!(c.rebuilt(entries(&["-Logs", "Shell"])), None);
        c.down();
        assert_eq!(c.label(), Some("Shell"), "Down starts from the top again");
    }

    #[test]
    fn a_menu_the_pointer_moved_over_is_not_followed() {
        let mut c = MenuCursor::new(entries(&["Logs", "Shell"]));
        c.down();
        c.lose();
        assert_eq!(c.label(), None);
        c.down();
        assert_eq!(c.label(), None, "the keys are not followed any more");
        assert_eq!(c.rebuilt(entries(&["Logs", "Shell"])), None);
    }

    #[test]
    fn nothing_highlighted_stays_that_way_through_a_rebuild() {
        let mut c = MenuCursor::new(entries(&["Logs"]));
        assert_eq!(c.rebuilt(entries(&["Logs", "Shell"])), None);
        assert_eq!(c.label(), None);
    }
}
