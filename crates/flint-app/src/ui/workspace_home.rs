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

use super::super::dashboard::cluster_card::{CardActions, ClusterRead};
use super::super::dashboard::set::AGE_OUT_INTERVAL;
use super::super::home::{self, HomeCard, HomePlan, HomeRead, HomeRow, HomeView};
use super::super::registry::{self, OpenHome, SelectCluster};
use super::super::settings_view::MainKey;
use super::Workspace;

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

    /// Shows the home screen over the main area. The active cluster stays connected.
    pub(super) fn open_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_settings_for_view();
        self.home.open = true;
        self.home.opened_over = Some(self.main_key());
        // Out of a list or detail now hidden, so keys reach the workspace.
        window.focus(&self.focus, cx);
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
        if self.settings_page.open {
            return false;
        }
        let model = self.model.read(cx);
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
                ws.home.cards = cards;
                ws.home.reading = false;
                if std::mem::take(&mut ws.home.dirty) {
                    ws.refresh_home(cx);
                }
                cx.notify();
            });
        }));
    }

    /// The card read for `cluster` in its present state, if any.
    fn home_card(&self, cluster: &str, status: KeepWatching) -> Option<&HomeCard> {
        self.home
            .cards
            .iter()
            .find(|card| card.cluster() == cluster && card.status == status)
    }

    fn home_view(&self, cx: &App) -> HomeView<'_> {
        let plan = self.home_plan(cx);
        let model = self.model.read(cx);
        let names = model.names();
        let settings_ok = model.settings_error.is_none();
        HomeView {
            watched: plan
                .watched
                .into_iter()
                .map(|(cluster, status)| {
                    let card = self.home_card(&cluster, status);
                    let shown = names.exact(&cluster);
                    (cluster, shown, status, card)
                })
                .collect(),
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

    /// The home screen, when the main area shows it.
    pub(super) fn render_home(&self, theme: &Theme, cx: &App) -> Option<AnyElement> {
        if !self.home_shown(cx) {
            return None;
        }
        // Every link on a card selects its cluster, as a click on it in the sidebar does.
        let actions: CardActions = Rc::new(|cluster, _| {
            Box::new(SelectCluster {
                cluster: cluster.to_owned(),
            })
        });
        Some(home::render(
            &self.home_view(cx),
            &actions,
            &self.focus,
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
                .map(|(cluster, _, status, card)| HomeCardState {
                    cluster: cluster.clone(),
                    status: home::status_word(*status),
                    card: card.map(|card| card.card.description()),
                    version: card.and_then(|card| card.version.clone()),
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
