//! The workspace's keyboard paths (UP-31, UP-33): the resource switcher (Cmd-P), going back to
//! the kind and object shown before (Cmd-[), moving focus between the panes (Tab and
//! Shift-Tab: clusters, navigation, list, detail, dock), and the cluster sidebar's keys (the
//! arrows move its cursor, Enter connects to the cluster under it).
// module-tests: keyboard_tests.rs

use std::sync::Arc;

use flint_core::{KindId, NavEntry, WatchMode, display};
use gpui_kit::*;

use super::super::focus::release_unfocused_input;
use super::super::palette::{CommandPalette, Mode, SwitcherScope};
use super::super::registry::{
    ActionError, ActivateSelectedCluster, Bind, GoBack, OpenObject, SelectCluster,
    SelectNextCluster, SelectPreviousCluster, ToggleResourceSwitcher,
};
use super::super::switcher_model::Index;
use super::Workspace;

/// How many places Go Back remembers.
const HISTORY: usize = 50;

/// A place Go Back returns to: a kind's list, and the object selected in it, if any.
#[derive(Debug, Clone, Eq)]
pub(in crate::ui) struct Place {
    pub kind: KindId,
    /// Namespace (`None`: cluster-scoped) and name.
    pub object: Option<(Option<String>, String)>,
    /// The revision of the rows the object was last seen in, if known
    /// (`ResourceList::selected_seen`): rows newer than that without it prove it gone on the
    /// way back, though the list opened again may start from them.
    pub seen: Option<u64>,
}

/// The same place whatever was known about its object.
impl PartialEq for Place {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.object == other.object
    }
}

/// The places shown before on the active cluster, oldest first, at most [`HISTORY`]. It
/// starts over when another cluster becomes active.
#[derive(Debug, Default, Clone)]
pub(in crate::ui) struct History(Vec<Place>);

impl History {
    /// Remembers `place`, unless it is the last one remembered.
    pub fn push(&mut self, place: Place) {
        if self.0.last() != Some(&place) {
            self.0.push(place);
        }
        if self.0.len() > HISTORY {
            self.0.remove(0);
        }
    }

    /// The place shown last, forgotten.
    pub fn pop(&mut self) -> Option<Place> {
        self.0.pop()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The places remembered, latest first.
    pub fn places(&self) -> impl Iterator<Item = &Place> {
        self.0.iter().rev()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// A pane Tab moves focus to, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Sidebar,
    Navigation,
    List,
    Detail,
    Dock,
}

impl Workspace {
    /// The sidebar's actions, bound on the sidebar's own element: they work only while it has
    /// focus, for [`Bind`].
    pub fn bind_sidebar_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<SelectPreviousCluster>(|this, _, _, cx| this.move_sidebar_cursor(-1, cx))
            .bind::<SelectNextCluster>(|this, _, _, cx| this.move_sidebar_cursor(1, cx))
            .bind::<ActivateSelectedCluster>(Self::activate_sidebar_cursor)
    }

    /// The cluster under the sidebar's cursor: where the arrows left it while that cluster is
    /// listed, else the active cluster, else the first.
    pub(in crate::ui) fn sidebar_cursor(&self, cx: &App) -> Option<String> {
        let model = self.model.read(cx);
        let listed = |name: &str| model.clusters.iter().any(|c| c.info.name == name);
        self.sidebar_cursor
            .clone()
            .filter(|c| listed(c))
            .or_else(|| model.active.clone().filter(|c| listed(c)))
            .or_else(|| model.clusters.first().map(|c| c.info.name.clone()))
    }

    /// Moves the sidebar's cursor `step` clusters, stopping at the first and the last.
    fn move_sidebar_cursor(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(current) = self.sidebar_cursor(cx) else {
            return;
        };
        let model = self.model.read(cx);
        let Some(at) = model.clusters.iter().position(|c| c.info.name == current) else {
            return;
        };
        let next = (at as isize + step).clamp(0, model.clusters.len() as isize - 1) as usize;
        self.sidebar_cursor = Some(model.clusters[next].info.name.clone());
        // Into view, on the next layout: the rows follow the settings error card, if shown.
        let before = usize::from(model.settings_error.is_some());
        self.sidebar_scroll.scroll_to_item(before + next);
        cx.notify();
    }

    /// Enter in the sidebar: the cluster under the cursor becomes the active one, as a click
    /// on it does. Focus stays in the sidebar.
    fn activate_sidebar_cursor(
        &mut self,
        _: &ActivateSelectedCluster,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(cluster) = self.sidebar_cursor(cx) {
            self.select_cluster(&SelectCluster { cluster }, window, cx);
        }
    }

    /// The panes shown now, in Tab order, with their focus.
    fn panes(&self, cx: &App) -> Vec<(Pane, FocusHandle)> {
        let mut panes = vec![(Pane::Sidebar, self.sidebar_focus.clone())];
        if self.model.read(cx).active_cluster().is_some() {
            panes.push((Pane::Navigation, self.nav.read(cx).focus_handle().clone()));
        }
        if let Some(list) = &self.list {
            panes.push((Pane::List, list.read(cx).focus_handle().clone()));
        }
        if let Some(detail) = &self.detail {
            panes.push((Pane::Detail, detail.read(cx).focus_handle().clone()));
        }
        if let Some(dock) = self.dock.read(cx).shown_tab_focus(cx) {
            panes.push((Pane::Dock, dock));
        }
        panes
    }

    /// Tab (`step` 1) or Shift-Tab (-1): focus moves to the next or previous pane shown,
    /// wrapping around. From outside every pane, Tab starts at the sidebar and Shift-Tab at
    /// the last pane.
    pub(super) fn focus_pane(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let panes = self.panes(cx);
        let at = panes
            .iter()
            .position(|(_, focus)| focus.contains_focused(window, cx));
        let next = match at {
            Some(at) => (at as isize + step).rem_euclid(panes.len() as isize) as usize,
            None if step > 0 => 0,
            None => panes.len() - 1,
        };
        window.focus(&panes[next].1, cx);
        // A field left behind (the list's filter, a log tab's search).
        release_unfocused_input(window, cx);
        cx.notify();
    }

    /// Where the main area is: the open kind and its selected object.
    fn place(&self, cx: &App) -> Option<Place> {
        let list = self.list.as_ref()?.read(cx);
        let object = self.detail.as_ref().map(|d| {
            let object = d.read(cx).object();
            (object.namespace.clone(), object.name.clone())
        });
        Some(Place {
            kind: list.kind().clone(),
            seen: object.as_ref().and(list.selected_seen()),
            object,
        })
    }

    /// Remembers where the main area is before it shows `next`, for Go Back.
    pub(super) fn remember_place(&mut self, next: &Place, cx: &App) {
        if let Some(here) = self.place(cx).filter(|here| here != next) {
            self.history.push(here);
        }
    }

    /// Forgets the places of the cluster that was active.
    pub(super) fn forget_places(&mut self) {
        self.history.clear();
    }

    /// The place shown, for Go Back to skip: none while an object opened by name waits for its
    /// row, since going back to the bare list is what cancels that pick.
    fn shown_place(&self, cx: &App) -> Option<Place> {
        let picking = self.list.as_ref().is_some_and(|l| l.read(cx).picking());
        self.place(cx).filter(|_| !picking)
    }

    /// Whether Go Back has somewhere to go: a place other than the one shown, of a kind the
    /// cluster still serves.
    pub(super) fn can_go_back(&self, cx: &App) -> bool {
        let here = self.shown_place(cx);
        self.cluster.is_some()
            && self
                .history
                .places()
                .any(|p| here.as_ref() != Some(p) && self.nav_entry(p.kind.as_str(), cx).is_some())
    }

    /// Go Back: shows the kind and object shown before.
    pub(super) fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        // Nothing to go to: the history stays as it is.
        if !self.can_go_back(cx) {
            return;
        }
        // Selecting a row or clearing the selection records nothing, so the last place
        // remembered can be the one shown; a press never goes there.
        let here = self.shown_place(cx);
        while let Some(place) = self.history.pop() {
            if here.as_ref() == Some(&place) {
                continue;
            }
            // A kind the cluster no longer serves is skipped.
            let Some(entry) = self.nav_entry(place.kind.as_str(), cx) else {
                continue;
            };
            self.show_kind(&entry, false, true, window, cx);
            if let Some(list) = &self.list {
                let focus = list.read(cx).focus_handle().clone();
                list.update(cx, |list, cx| match place.object {
                    Some((namespace, name)) => list.select_listed(namespace, name, place.seen, cx),
                    None => list.clear_selection(cx),
                });
                // Also when the kind was open already.
                window.focus(&focus, cx);
            }
            return;
        }
    }

    /// [`OpenObject`]: a kind the active cluster serves, and a name.
    pub(super) fn check_open_object(
        &self,
        action: &OpenObject,
        cx: &App,
    ) -> Result<(), ActionError> {
        if action.name.trim().is_empty() {
            return Err(ActionError::rejected::<OpenObject>("empty object name"));
        }
        self.check_listed_on::<OpenObject>(
            action.cluster.as_deref(),
            action.server.as_deref(),
            cx,
        )?;
        match self.nav_entry(&action.kind, cx) {
            Some(_) => Ok(()),
            None => Err(ActionError::rejected::<OpenObject>(format!(
                "the active cluster does not serve {} (or discovery has not finished)",
                action.kind
            ))),
        }
    }

    /// Opens the object's list with it selected, which shows its details. The namespace filter
    /// is left alone: when it hides the object, the list says so and the details still show.
    pub(super) fn open_object(
        &mut self,
        action: &OpenObject,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_open_object(action, cx) {
            tracing::warn!("{e}");
            return;
        }
        let Some(entry) = self.nav_entry(&action.kind, cx) else {
            return;
        };
        let namespace = action
            .namespace
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty() && entry.namespaced)
            .map(str::to_owned);
        let name = action.name.trim().to_owned();
        let target = Place {
            kind: entry.id.clone(),
            object: Some((namespace.clone(), name.clone())),
            seen: None,
        };
        self.remember_place(&target, cx);
        self.show_kind(&entry, false, true, window, cx);
        if let Some(list) = &self.list {
            let focus = list.read(cx).focus_handle().clone();
            list.update(cx, |list, cx| list.select_listed(namespace, name, None, cx));
            window.focus(&focus, cx);
        }
    }

    /// Opens the resource switcher, or closes it when it is open.
    pub(super) fn toggle_switcher(
        &mut self,
        _: &ToggleResourceSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette_in(Mode::Resources, window, cx);
    }

    /// What the resource switcher lists now: the active cluster and its listable kinds (the
    /// objects come later, [`Workspace::load_switcher`]), and what it was built from, to see
    /// when that changes.
    fn switcher_scope(&self, cx: &App) -> (SwitcherScope, Vec<NavEntry>, SwitcherKey) {
        let model = self.model.read(cx);
        let discovered = self.cluster.as_ref().and_then(|c| model.discovered(c));
        let entries: Vec<NavEntry> = discovered
            .and_then(|d| d.discovery.as_ref())
            .map(|d| {
                d.navigation
                    .sections
                    .iter()
                    .flat_map(|s| s.entries.iter().cloned())
                    .collect()
            })
            .unwrap_or_default();
        let discovery_failed =
            discovered.is_some_and(|d| d.discovery.is_none() && d.error.is_some());
        let on_view = self
            .cluster
            .as_deref()
            .is_some_and(|c| model.settings.watch_mode_for(c) == WatchMode::OnView);
        let summary = self
            .cluster
            .as_ref()
            .and_then(|c| model.clusters.iter().find(|s| s.info.name == *c));
        let cluster_id = self
            .cluster
            .as_ref()
            .map(|c| (c.to_string(), summary.and_then(|s| s.info.server.clone())));
        let key = SwitcherKey {
            cluster: cluster_id.clone(),
            generation: summary.map(|s| s.generation),
            kinds: entries
                .iter()
                .map(|e| (e.id.clone(), e.kind.clone(), e.label.clone(), e.listable))
                .collect(),
            discovery_failed,
            on_view,
        };
        let scope = SwitcherScope {
            cluster: self
                .cluster
                .as_deref()
                .map(|c| SharedString::from(display::short_name(c).to_string())),
            cluster_id,
            index: Index::new(&entries, &[]),
            loading: self.cluster.is_some() && !entries.is_empty(),
            discovery_failed,
            on_view,
        };
        (scope, entries, key)
    }

    /// Reads the objects in memory for `palette`, the switcher, off the UI thread. Nothing here
    /// reaches the API server: the core reads its stores without subscribing
    /// (`CoreHandle::stored_snapshots`).
    fn load_switcher(
        &mut self,
        palette: &Entity<CommandPalette>,
        entries: Vec<NavEntry>,
        cx: &mut Context<Self>,
    ) {
        let cluster = self.cluster.clone().filter(|_| !entries.is_empty());
        self._switcher = match cluster {
            Some(cluster) => {
                let stored = self.model.read(cx).core().stored_snapshots(&cluster);
                let scope = palette.read(cx).scope_generation();
                let palette = palette.downgrade();
                cx.spawn(async move |_, cx| {
                    let snapshots = stored.await;
                    let index = cx
                        .background_spawn(async move { Index::new(&entries, &snapshots) })
                        .await;
                    let _ = palette.update(cx, |p, cx| p.set_index(Arc::new(index), scope, cx));
                })
            }
            None => Task::ready(()),
        };
    }

    /// The resource switcher, from `previous`: the active cluster's kinds at once, and the
    /// objects its watches hold once read.
    pub(super) fn new_switcher(
        &mut self,
        previous: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<CommandPalette> {
        let (scope, entries, key) = self.switcher_scope(cx);
        let fallback = self.focus.clone();
        let palette = cx.new(|cx| CommandPalette::switcher(previous, fallback, scope, window, cx));
        self.load_switcher(&palette, entries, cx);
        self.switcher_key = Some(key);
        palette
    }

    /// Keeps an open switcher on what is active: when another cluster becomes active, or the
    /// active one's discovery arrives (or fails, or its watch mode changes), it lists anew,
    /// with what was typed kept. Rows of the cluster before never stay to be opened on the new
    /// one.
    pub(super) fn refresh_switcher(&mut self, cx: &mut Context<Self>) {
        let Some((palette, _)) = &self.palette else {
            return;
        };
        let palette = palette.clone();
        if palette.read(cx).mode() != Mode::Resources {
            return;
        }
        let (scope, entries, key) = self.switcher_scope(cx);
        if self.switcher_key.as_ref() == Some(&key) {
            return;
        }
        palette.update(cx, |p, cx| p.rescope(scope, cx));
        self.load_switcher(&palette, entries, cx);
        self.switcher_key = Some(key);
    }

    /// [`OpenView`](super::super::registry::OpenView) and [`OpenObject`] from the switcher name the cluster their row was listed
    /// on: refused once another cluster, or another API server, is active.
    pub(super) fn check_listed_on<A: Action>(
        &self,
        cluster: Option<&str>,
        server: Option<&str>,
        cx: &App,
    ) -> Result<(), ActionError> {
        let Some(cluster) = cluster else {
            return Ok(());
        };
        let active_server = self
            .cluster_summary(cluster, cx)
            .and_then(|c| c.info.server.as_deref());
        if self.cluster.as_deref() != Some(cluster) || active_server != server {
            return Err(ActionError::rejected::<A>(format!(
                "listed on {cluster}, which is not the active cluster any more"
            )));
        }
        Ok(())
    }
}

/// What an open switcher was built from: when it changes, the switcher lists anew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) struct SwitcherKey {
    cluster: Option<(String, Option<String>)>,
    /// The active context's slot: one that started over (re-pointed and put back between reads,
    /// so name, server and kinds match again) has other objects, and the index is rebuilt.
    generation: Option<u64>,
    /// Each kind's id, Kind, label as shown, and whether it can be listed: what its rows say.
    kinds: Vec<(KindId, String, String, bool)>,
    discovery_failed: bool,
    on_view: bool,
}

#[cfg(test)]
mod tests {
    use flint_core::KindId;

    use super::{HISTORY, History, Place};

    fn place(kind: &str, name: Option<&str>) -> Place {
        Place {
            kind: KindId::new("", kind),
            object: name.map(|n| (Some("demo".to_owned()), n.to_owned())),
            seen: None,
        }
    }

    // covers UP-33: Go Back returns to the places shown, latest first, each once.
    #[test]
    fn history_goes_back_latest_first_without_repeats() {
        let mut history = History::default();
        assert!(history.is_empty());
        history.push(place("pods", None));
        history.push(place("pods", Some("web")));
        assert_eq!(history.places().count(), 2);
        history.push(place("pods", Some("web")));
        history.push(place("services", None));
        assert_eq!(history.pop(), Some(place("services", None)));
        assert_eq!(history.pop(), Some(place("pods", Some("web"))));
        assert_eq!(history.pop(), Some(place("pods", None)));
        assert_eq!(history.pop(), None);
        history.push(place("pods", None));
        history.clear();
        assert!(history.is_empty());
    }

    #[test]
    fn history_keeps_the_latest_places() {
        let mut history = History::default();
        for i in 0..HISTORY + 5 {
            history.push(place(&format!("kind{i}"), None));
        }
        let mut count = 0;
        let mut last = None;
        while let Some(p) = history.pop() {
            count += 1;
            last = Some(p);
        }
        assert_eq!(count, HISTORY);
        assert_eq!(last, Some(place("kind5", None)));
    }
}
