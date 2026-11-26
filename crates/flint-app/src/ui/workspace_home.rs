//! The workspace's side of the fleet home screen ([`home`]): when it shows, what it is read
//! from, and the sidebar's Home entry.
//!
//! The home screen shows in place of an empty main area (on launch), or over the active
//! cluster's view from the Home entry, which keeps that cluster connected. Selecting any cluster
//! (its card, Open, the sidebar) leaves it, back to that cluster's view. Its cards are read only
//! from feeds the core already runs for the clusters kept watching: nothing here connects.

// module-tests: workspace_home_tests.rs

use std::rc::Rc;

use flint_core::keep_watching::KeepWatching;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Selectable as _, Sizable as _, Theme};
use gpui_kit::*;
use jiff::Timestamp;

use super::super::dashboard::FeedId;
use super::super::dashboard::cluster_card::{CardActions, CardTarget, ClusterRead};
use super::super::dashboard::set::AGE_OUT_INTERVAL;
use super::super::home::{
    self, HomeCard, HomePlan, HomeRead, HomeRow, HomeView, OnSignIn, WatchedCard,
};
use super::super::registry::{self, OpenHome, SelectCluster};
use super::Workspace;

/// What the main area shows, as far as the home screen gives way to a change of it: another
/// cluster or a set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) struct MainKey {
    cluster: Option<flint_core::ClusterId>,
    /// The running set's list.
    set: Option<EntityId>,
}

/// The home screen's state in the workspace.
#[derive(Default)]
pub(in crate::ui) struct HomePage {
    /// Opened from the Home entry, over what the main area showed.
    pub open: bool,
    /// What the main area showed when it opened: another cluster or set opened meanwhile
    /// leaves it.
    opened_over: Option<MainKey>,
    /// The cards last read, in sidebar order.
    cards: Vec<HomeCard>,
    /// The watched clusters and states those cards were read for.
    built_for: Vec<(String, KeepWatching)>,
    /// No feed changed since the cards were read.
    current: bool,
    reading: bool,
    /// A feed changed while the cards were being read.
    dirty: bool,
    /// Why the last card's Sign in was refused, until the next one.
    pub(in crate::ui) sign_in_error: Option<String>,
    _read: Option<Task<()>>,
    _watch: Vec<Task<()>>,
}

impl Workspace {
    /// Reads the cards again whenever a feed changes, and as their values age.
    pub(super) fn watch_home_feeds(&mut self, cx: &mut Context<Self>) {
        let mut updates = self.model.read(cx).core().telemetry_updates();
        let on_update = cx.spawn(async move |this, cx| {
            while updates.changed().await {
                if this.update(cx, |ws, cx| ws.refresh_home(cx)).is_err() {
                    break;
                }
            }
        });
        let age_out = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AGE_OUT_INTERVAL).await;
                if this.update(cx, |ws, cx| ws.refresh_home(cx)).is_err() {
                    break;
                }
            }
        });
        self.home._watch = vec![on_update, age_out];
    }

    /// What the main area shows under the home screen, as far as it gives way to it: another
    /// cluster or a set opened meanwhile. Views that open by themselves (a list restored, the
    /// overview once discovery ends) do not count; the navigation and Open Object leave the
    /// home screen on their own.
    fn main_key(&self) -> MainKey {
        MainKey {
            cluster: self.cluster.clone(),
            set: self.sets.running.as_ref().map(|r| r.list.entity_id()),
        }
    }

    /// Shows the home screen over the main area. The active cluster stays connected.
    pub(super) fn open_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.home.open = true;
        self.home.opened_over = Some(self.main_key());
        // Out of a list or detail now hidden, onto the home screen, so keys reach the workspace
        // and Up and Down move between the cards' links.
        window.focus(&self.home_focus, cx);
        self.refresh_home(cx);
        cx.notify();
    }

    /// Leaves the home screen for the view under it.
    pub(super) fn leave_home(&mut self) {
        self.home.open = false;
        self.home.opened_over = None;
    }

    /// Leaves the home screen when another cluster or a set was opened meanwhile, and reads the
    /// cards when what they show changed. Called as the workspace draws.
    pub(super) fn home_follow_main(&mut self, cx: &mut Context<Self>) {
        if self.home.open && self.home.opened_over.as_ref() != Some(&self.main_key()) {
            self.leave_home();
        }
        if self.home_shown(cx)
            && !self.home.reading
            && (!self.home.current || self.home.built_for != self.home_plan(cx).watched)
        {
            self.refresh_home(cx);
        }
    }

    /// Whether the main area shows the home screen: opened from the Home entry, or in place of
    /// an empty main area.
    pub(in crate::ui) fn home_shown(&self, cx: &App) -> bool {
        let model = self.model.read(cx);
        // With no context at all, the main area keeps saying where Flint looked.
        if !model.loaded || model.clusters.is_empty() {
            return false;
        }
        self.home.open
            || (model.loaded
                && !model.clusters.is_empty()
                && model.active_cluster().is_none()
                && !self.sets.is_open()
                && self.edit_shown_alone(cx).is_none())
    }

    /// Every cluster in sidebar order, split into cards and rows.
    fn home_plan(&self, cx: &App) -> HomePlan {
        let model = self.model.read(cx);
        home::plan(model.clusters.iter().map(|cluster| {
            let name = cluster.info.name.as_str();
            let status = model
                .keep_watching(name)
                .map_or(KeepWatching::Off, |view| view.status);
            (name, status)
        }))
    }

    /// Each watched cluster as it stands now. Reads only what the core already holds, and no
    /// feed of a cluster whose keep-watching state says it may not run.
    fn home_reads(&self, watched: &[(String, KeepWatching)], cx: &App) -> Vec<HomeRead> {
        let model = self.model.read(cx);
        let names = model.names();
        let alerts = cx
            .try_global::<super::super::alert_center::Alerts>()
            .map(|alerts| alerts.0.read(cx));
        watched
            .iter()
            .filter_map(|(cluster, status)| {
                let summary = self.cluster_summary(cluster, cx)?;
                let blocked = home::blocked_reason(*status);
                let server = summary.info.server.clone();
                Some(HomeRead {
                    read: ClusterRead {
                        cluster: cluster.clone(),
                        shown: names.exact(cluster),
                        tag: model.cluster_tag(summary),
                        telemetry: blocked
                            .is_none()
                            .then(|| model.core().telemetry(cluster))
                            .flatten(),
                        blocked,
                        thresholds: model.settings.thresholds_for(cluster, server.as_deref()),
                        server: server.unwrap_or_default(),
                    },
                    status: *status,
                    latest_alert: alerts.and_then(|alerts| alerts.latest_alert(summary)),
                })
            })
            .collect()
    }

    /// Reads the cards again off the UI thread, one read at a time, while the home screen shows.
    pub(super) fn refresh_home(&mut self, cx: &mut Context<Self>) {
        if !self.home_shown(cx) {
            self.home.current = false;
            return;
        }
        if self.home.reading {
            self.home.dirty = true;
            return;
        }
        let watched = self.home_plan(cx).watched;
        let reads = self.home_reads(&watched, cx);
        self.home.reading = true;
        self.home.dirty = false;
        self.home.current = true;
        self.home.built_for = watched;
        let job = cx.background_spawn(async move { home::build(reads, Timestamp::now()) });
        self.home._read = Some(cx.spawn(async move |this, cx| {
            let cards = job.await;
            let _ = this.update(cx, |ws, cx| {
                ws.home.reading = false;
                // Read from a feed that has since been replaced or stopped: read again.
                let stale = cards
                    .iter()
                    .any(|card| !ws.feed_still_runs(card.cluster(), card.card.feed.as_ref(), cx));
                if !stale {
                    ws.home.cards = cards;
                }
                if std::mem::take(&mut ws.home.dirty) || stale {
                    ws.refresh_home(cx);
                }
                cx.notify();
            });
        }));
    }

    /// Whether `cluster`'s feed is still `feed`, the one a card was read from (`None`: the card
    /// read no feed, so nothing can have gone stale).
    fn feed_still_runs(&self, cluster: &str, feed: Option<&FeedId>, cx: &App) -> bool {
        feed.is_none_or(|feed| {
            self.model
                .read(cx)
                .core()
                .telemetry(cluster)
                .is_some_and(|telemetry| FeedId::of(&telemetry) == *feed)
        })
    }

    /// The card read for `cluster` in its present state and from the feed it runs now, if any.
    fn home_card(&self, cluster: &str, status: KeepWatching, cx: &App) -> Option<&HomeCard> {
        self.home.cards.iter().find(|card| {
            card.cluster() == cluster
                && card.status == status
                && self.feed_still_runs(cluster, card.card.feed.as_ref(), cx)
        })
    }

    fn home_view(&self, cx: &App) -> HomeView<'_> {
        let plan = self.home_plan(cx);
        let model = self.model.read(cx);
        let names = model.names();
        let settings_ok = model.settings_error.is_none();
        let active = model.active_cluster().map(|c| c.info.name.as_str());
        let in_set = |cluster: &str| self.sets.has_member(Some(cluster));
        HomeView {
            watched: plan
                .watched
                .into_iter()
                .map(|(cluster, status)| {
                    let server = self
                        .cluster_summary(&cluster, cx)
                        .and_then(|summary| summary.info.server.as_deref());
                    let sign_in = home::card_sign_in(
                        &cluster,
                        model.keep_watching(&cluster),
                        server,
                        active == Some(cluster.as_str()) || in_set(&cluster),
                    );
                    WatchedCard {
                        card: self.home_card(&cluster, status, cx),
                        shown: names.exact(&cluster),
                        cluster,
                        status,
                        sign_in,
                    }
                })
                .collect(),
            error: self.home.sign_in_error.clone(),
            unwatched: plan
                .unwatched
                .into_iter()
                .map(|cluster| HomeRow {
                    label: names.of(&cluster),
                    tag: self
                        .cluster_summary(&cluster, cx)
                        .and_then(|c| model.cluster_tag(c)),
                    keep: home::keep_button(model.keep_watching(&cluster), settings_ok),
                    cluster,
                })
                .collect(),
        }
    }

    /// For a click on a card's Sign in alone ([`home::CardSignIn`]): runs the cluster's sign-in
    /// once, now, for the server and definitions the card was drawn with. The core refuses it if
    /// any of them changed since, and nothing runs; why shows on the home screen. The card
    /// comes back with the cluster's next keep-watching state.
    pub(in crate::ui) fn sign_in_home_card(
        &mut self,
        target: home::CardSignIn,
        cx: &mut Context<Self>,
    ) {
        let asked = self.model.read(cx).core().sign_in_watched(
            &target.cluster,
            &target.server,
            target.definition,
            target.shown,
        );
        self.home.sign_in_error = None;
        cx.spawn(async move |ws, cx| {
            if let Err(why) = asked.await {
                let _ = ws.update(cx, |ws, cx| {
                    ws.home.sign_in_error = Some(why);
                    cx.notify();
                });
            }
        })
        .detach();
        cx.notify();
    }

    /// The home screen, when the main area shows it.
    pub(super) fn render_home(&self, theme: &Theme, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.home_shown(cx) {
            return None;
        }
        // A card's name selects its cluster, as a click on it in the sidebar does. Its counts
        // open nothing: their lists belong to the cluster's own overview.
        let actions: CardActions = Rc::new(|cluster, target| match target {
            CardTarget::Cluster => Some(Box::new(SelectCluster {
                cluster: cluster.to_owned(),
            })),
            CardTarget::Problem { .. } => None,
        });
        let workspace = cx.entity().downgrade();
        let on_sign_in: OnSignIn = Rc::new(move |target, cx| {
            let _ = workspace.update(cx, |ws, cx| ws.sign_in_home_card(target, cx));
        });
        Some(home::render(
            &self.home_view(cx),
            &actions,
            &on_sign_in,
            &self.focus,
            &self.home_focus,
            theme,
        ))
    }

    /// The sidebar's Home entry, at its top.
    pub(super) fn render_home_entry(&self, theme: &Theme, cx: &App) -> impl IntoElement {
        div()
            .id("home-entry")
            .flex_none()
            .w_full()
            .px_2()
            .pb_1()
            .border_b_1()
            .border_color(theme.sidebar_border)
            .child(
                Button::new("open-home")
                    .xsmall()
                    .ghost()
                    .w_full()
                    .label("Home")
                    .selected(self.home_shown(cx))
                    .tooltip("Every cluster at a glance. The active cluster stays connected.")
                    .on_click(registry::on_click(OpenHome)),
            )
            .test_support()
    }

    #[cfg(any(test, feature = "automation"))]
    pub(super) fn home_state(&self, cx: &App) -> super::super::view_state::Main {
        use super::super::view_state::{HomeCardState, HomeRowState, Main};
        let view = self.home_view(cx);
        Main::Home {
            watched: view
                .watched
                .iter()
                .map(|watched| HomeCardState {
                    cluster: watched.cluster.clone(),
                    status: home::status_word(watched.status),
                    card: watched.card.map(|card| card.card.description()),
                    version: watched.card.and_then(|card| card.version.clone()),
                    sign_in: watched.sign_in.is_some(),
                })
                .collect(),
            unwatched: view
                .unwatched
                .iter()
                .map(|row| HomeRowState {
                    cluster: row.cluster.clone(),
                    label: row.label.clone(),
                    keep_watching: row.keep.enabled,
                })
                .collect(),
        }
    }
}
