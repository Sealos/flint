//! Hosts the running set's dashboard: feeds it from telemetry the core already holds, and answers
//! its links.
//!
//! A link opens only while the member is admitted, can be read, and still runs the feed the card
//! was drawn from. Anything else opens nothing and redraws the dashboard.

// module-tests: set_dashboard_tests.rs

use std::sync::Arc;

use flint_core::KindId;
use flint_core::combined::CombinedObjects;
use flint_core::dashboard::ProblemCategory;
use flint_core::settings::WatchMode;
use gpui_kit::*;
use jiff::Timestamp;

use super::super::super::dashboard::set::{
    self, AGE_OUT_INTERVAL, MemberRead, OpenSetMember, OpenSetProblem, SetDashboardModel,
    SetDashboardView, category_named, kind_entry,
};
use super::super::super::dashboard::{FeedId, ProblemList, ProblemLists};
use super::super::super::focus::release_unfocused_input;
use super::super::super::registry::{Bind, OpenClusterSetDashboard, ShowClusterSetList};
use super::Workspace;

/// The list of `category` to open: the one of `kind`, or the only one.
fn pick_list<'a>(
    lists: &'a ProblemLists,
    category: ProblemCategory,
    kind: Option<&str>,
) -> Option<&'a ProblemList> {
    let lists = lists.get(&category)?;
    match kind {
        Some(kind) => lists.iter().find(|list| list.kind == kind),
        None => match lists.as_slice() {
            [list] => Some(list),
            _ => None,
        },
    }
}

impl Workspace {
    pub(super) fn bind_set_dashboard_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<OpenClusterSetDashboard>(Self::open_set_dashboard)
            .bind::<ShowClusterSetList>(Self::show_set_list)
            .bind::<OpenSetMember>(Self::open_set_member)
            .bind::<OpenSetProblem>(Self::open_set_problem)
    }

    /// The navigation's Overview: of the set while one is open, else of the active cluster.
    pub(in super::super) fn open_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sets.is_open() {
            self.open_set_dashboard(&OpenClusterSetDashboard, window, cx);
        } else {
            self.open_dashboard(window, cx);
        }
    }

    pub(in crate::ui) fn set_dashboard(&self) -> Option<Entity<SetDashboardView>> {
        self.sets.running.as_ref()?.dashboard.clone()
    }

    fn open_set_dashboard(
        &mut self,
        _: &OpenClusterSetDashboard,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.sets.is_open() {
            return;
        }
        if let Some(view) = self.set_dashboard() {
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        self.close_set_detail(cx);
        let mut updates = self.model.read(cx).core().telemetry_updates();
        let workspace = cx.weak_entity();
        let view = cx.new(|cx| {
            let mut view = SetDashboardView::new(cx);
            let on_update = workspace.clone();
            view._updates = cx.spawn(async move |_, cx| {
                while updates.changed().await {
                    if on_update
                        .update(cx, |ws, cx| ws.refresh_set_dashboard(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            view._age_out = cx.spawn(async move |_, cx| {
                loop {
                    cx.background_executor().timer(AGE_OUT_INTERVAL).await;
                    if workspace
                        .update(cx, |ws, cx| ws.refresh_set_dashboard(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            view
        });
        let focus = view.read(cx).focus_handle(cx);
        if let Some(run) = &mut self.sets.running {
            run.dashboard = Some(view);
        }
        window.focus(&focus, cx);
        release_unfocused_input(window, cx);
        self.nav.update(cx, |nav, cx| nav.set_overview(true, cx));
        self.refresh_set_dashboard(cx);
        cx.notify();
    }

    fn show_set_list(
        &mut self,
        _: &ShowClusterSetList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_set_dashboard(window, cx);
    }

    /// Back to the combined list, which was kept behind the dashboard.
    pub(super) fn close_set_dashboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = &mut self.sets.running else {
            return;
        };
        if run.dashboard.take().is_none() {
            return;
        }
        let (kind, focus) = (run.kind.clone(), run.list.read(cx).focus_handle().clone());
        self.nav
            .update(cx, |nav, cx| nav.set_selected(Some(kind), cx));
        window.focus(&focus, cx);
        release_unfocused_input(window, cx);
        cx.notify();
    }

    /// The feed `cluster` runs now, if it is an admitted member that may be read.
    fn running_feed_of(&self, cluster: &str, cx: &App) -> Option<FeedId> {
        #[cfg(test)]
        if let Some(view) = self.set_dashboard()
            && let Some(test) = &view.read(cx).test_feeds
        {
            return test.feeds.get(cluster).cloned();
        }
        if self.set_row_refusal(cluster, cx).is_some() {
            return None;
        }
        let member = self.sets.member(cluster)?;
        let telemetry = self.model.read(cx).core().member_telemetry(member)?;
        Some(FeedId::of(&telemetry))
    }

    /// The objects behind `cluster`'s counts, read from its feed now.
    fn member_lists(&self, cluster: &str, cx: &App) -> Option<ProblemLists> {
        #[cfg(test)]
        if let Some(view) = self.set_dashboard()
            && let Some(test) = &view.read(cx).test_feeds
        {
            return test.lists.get(cluster).cloned();
        }
        let member = self.sets.member(cluster)?;
        let telemetry = self.model.read(cx).core().member_telemetry(member)?;
        Some(set::lists_now(&telemetry))
    }

    /// Each member as it stands now, and those left out when the set opened. Reads only what
    /// the core already holds, and never a feed of a member that may not be read.
    fn member_reads(&self, cx: &App) -> Vec<MemberRead> {
        let Some(run) = &self.sets.running else {
            return Vec::new();
        };
        let names = self.cluster_names(cx);
        let model = self.model.read(cx);
        let core = model.core();
        let tag = |cluster: &str| {
            self.cluster_summary(cluster, cx)
                .and_then(|c| model.cluster_tag(c))
        };
        let admitted = run.members.iter().map(|member| {
            let cluster = member.context();
            let blocked = self.set_row_refusal(cluster, cx);
            MemberRead {
                cluster: cluster.to_owned(),
                shown: names.exact(cluster),
                tag: tag(cluster),
                telemetry: blocked
                    .is_none()
                    .then(|| core.member_telemetry(member))
                    .flatten(),
                blocked,
                server: member.server().to_owned(),
                thresholds: model
                    .settings
                    .thresholds_for(cluster, Some(member.server())),
                watched: model.settings.watch_mode_for(cluster) == WatchMode::All,
            }
        });
        let left_out = run.left_out.iter().map(|cluster| MemberRead {
            cluster: cluster.clone(),
            shown: names.exact(cluster),
            tag: tag(cluster),
            blocked: Some("Changed, left out of this set.".to_owned()),
            telemetry: None,
            server: String::new(),
            thresholds: model.settings.thresholds_for(cluster, None),
            watched: false,
        });
        admitted.chain(left_out).collect()
    }

    /// Rebuilds the open dashboard off the UI thread, one build at a time.
    pub(super) fn refresh_set_dashboard(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.set_dashboard() else {
            return;
        };
        #[cfg(test)]
        if view.read(cx).test_feeds.is_some() {
            return;
        }
        if view.read(cx).reading {
            view.update(cx, |view, _| view.dirty = true);
            return;
        }
        let reads = self.member_reads(cx);
        let job = cx.background_spawn(async move { set::build(reads, Timestamp::now()) });
        let workspace = cx.weak_entity();
        view.update(cx, |view, cx| {
            view.reading = true;
            view.dirty = false;
            view._read = cx.spawn(async move |this, cx| {
                let built = job.await;
                let Some(this) = this.upgrade() else {
                    return;
                };
                let _ = workspace.update(cx, |ws, cx| ws.finish_set_read(&this, built, cx));
            });
        });
    }

    /// Shows a finished build, unless a card's feed is no longer the one running.
    pub(in crate::ui) fn finish_set_read(
        &mut self,
        view: &Entity<SetDashboardView>,
        built: SetDashboardModel,
        cx: &mut Context<Self>,
    ) {
        let again = view.update(cx, |view, _| {
            view.reading = false;
            std::mem::take(&mut view.dirty)
        });
        if self.set_dashboard().as_ref() != Some(view) {
            return;
        }
        let stale = built.cards.iter().any(|card| {
            card.feed.is_some() && card.feed != self.running_feed_of(&card.cluster, cx)
        });
        if !stale {
            view.update(cx, |view, cx| view.set_model(built, cx));
        }
        if stale || again {
            self.refresh_set_dashboard(cx);
        }
    }

    /// Whether `cluster`'s card still shows the feed running now.
    fn card_is_current(&self, view: &Entity<SetDashboardView>, cluster: &str, cx: &App) -> bool {
        view.read(cx).model.card(cluster).is_some_and(|card| {
            card.state.counts()
                && card.feed.is_some()
                && card.feed == self.running_feed_of(cluster, cx)
        })
    }

    fn open_set_member(
        &mut self,
        action: &OpenSetMember,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.set_dashboard() else {
            return;
        };
        if !self.card_is_current(&view, &action.cluster, cx) {
            self.refresh_set_dashboard(cx);
            return;
        }
        self.close_set_dashboard(window, cx);
        if let Some(run) = &self.sets.running {
            run.list
                .update(cx, |list, cx| list.show_cluster(&action.cluster, cx));
        }
    }

    fn open_set_problem(
        &mut self,
        action: &OpenSetProblem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.set_dashboard() else {
            return;
        };
        let Some(category) = category_named(&action.category) else {
            return;
        };
        let (targets, members): (Vec<String>, usize) = {
            let model = &view.read(cx).model;
            let targets = model
                .cards
                .iter()
                .filter(|card| action.cluster.as_ref().is_none_or(|c| *c == card.cluster))
                .filter(|card| card.state.counts() && card.counts.get(category).is_some())
                .map(|card| card.cluster.clone())
                .collect();
            (targets, model.cards.len())
        };
        if targets.is_empty() {
            self.refresh_set_dashboard(cx);
            return;
        }
        let mut objects = CombinedObjects::default();
        let mut opened: Option<(&'static str, String)> = None;
        for cluster in &targets {
            if !self.card_is_current(&view, cluster, cx) {
                self.refresh_set_dashboard(cx);
                return;
            }
            let lists = self.member_lists(cluster, cx);
            let Some(list) = lists
                .as_ref()
                .and_then(|lists| pick_list(lists, category, action.kind.as_deref()))
            else {
                self.refresh_set_dashboard(cx);
                return;
            };
            list.filter.objects.add_to(cluster, &mut objects);
            // A member counting nothing has a placeholder kind, which must not set the list's.
            if opened.is_none() || list.filter.objects.len() > 0 {
                opened = Some((list.kind, list.filter.label.clone()));
            }
        }
        let Some((kind, label)) = opened else {
            return;
        };
        let Some((id, kind_label)) = kind_entry(kind) else {
            return;
        };
        let label = match action.cluster.as_deref() {
            Some(cluster) => format!("{label} on {}", self.cluster_names(cx).exact(cluster)),
            None if targets.len() < members => {
                format!("{label} on {} of {members} clusters", targets.len())
            }
            None => format!("{label} on every cluster"),
        };
        self.show_set_kind(id, kind_label.to_owned(), window, cx);
        if let Some(run) = &self.sets.running {
            run.list.update(cx, |list, cx| {
                list.show_objects(label, Arc::new(objects), cx)
            });
        }
    }

    /// Shows `kind` across the set, in place of the dashboard if it is open.
    pub(super) fn show_set_kind(
        &mut self,
        kind: KindId,
        kind_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = &self.sets.running else {
            return;
        };
        if run.kind == kind {
            let focus = run.list.read(cx).focus_handle().clone();
            self.close_set_dashboard(window, cx);
            window.focus(&focus, cx);
            return;
        }
        let (label, members, changed) =
            (run.label.clone(), run.members.clone(), run.changed.clone());
        let (set_id, left_out) = (run.set_id.clone(), run.left_out.clone());
        self.close_set_detail(cx);
        self.show_set(label, members, kind, kind_label, window, cx);
        if let Some(run) = &mut self.sets.running {
            run.changed = changed;
            run.set_id = set_id;
            run.left_out = left_out;
        }
        // Only now: a member that changed must not count as serving, even for a moment.
        self.refresh_set_kinds(cx);
    }
}

#[cfg(test)]
impl Workspace {
    /// Stops the live feeds and shows `snapshots` as the members' rows.
    pub(in crate::ui) fn feed_set_rows_for_test(
        &mut self,
        snapshots: Vec<Arc<flint_core::KindSnapshot>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = &mut self.sets.running {
            run._feeds.clear();
        }
        for snapshot in snapshots {
            self.feed_member(snapshot, window, cx);
        }
    }

    /// The running set's combined list.
    pub(in crate::ui) fn set_list_for_test(
        &self,
    ) -> Option<Entity<super::super::super::list::combined::CombinedResourceList>> {
        Some(self.sets.running.as_ref()?.list.clone())
    }

    pub(in crate::ui) fn set_is_open_for_test(&self) -> bool {
        self.sets.is_open()
    }
}
