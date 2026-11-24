//! The workspace's side of Settings' Thresholds and Cluster overrides
//! ([`settings_thresholds`]): their actions and guards, the text field a threshold is typed in,
//! and their controls under the keyboard cursor.
//!
//! Every edit goes into the Settings draft through [`Workspace::edit_settings`], as the core's
//! own write; a number its metric does not accept goes in as rejected and blocks Save.

// module-tests: settings_thresholds_tests.rs

use flint_core::settings::Settings;
use flint_core::thresholds::ThresholdMetric;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use super::super::registry::{ActionError, Bind, Guards};
use super::super::settings_draft::Change;
use super::super::settings_thresholds::{
    self, AddClusterOverride, CancelThresholdEntry, EditClusterAlerts, EditClusterWatchMode,
    EditThreshold, KnownCluster, OverrideRow, OverridesLayout, ResetClusterOverrides,
    ThresholdEntry, ThresholdStop, ThresholdTarget, TypeThreshold,
};
use super::super::settings_view::{Section, Stop};
use super::Workspace;

impl Workspace {
    pub(super) fn threshold_guards(guards: Guards<Self>) -> Guards<Self> {
        // Not while a Save is being written: an edit then would be made over the old file.
        fn editable(ws: &Workspace, _: &App) -> bool {
            ws.settings_page.open && ws.settings_page.saving.is_none()
        }
        fn metric(ws: &Workspace, key: &str) -> Result<(), String> {
            let _ = ws;
            settings_thresholds::metric_named(key)
                .map(drop)
                .ok_or_else(|| {
                    let keys: Vec<_> = ThresholdMetric::ALL.map(ThresholdMetric::key).to_vec();
                    format!("the metric is one of {}", keys.join(", "))
                })
        }
        guards
            .add::<EditThreshold>(editable, |ws, action, _| {
                metric(ws, &action.metric).map_err(ActionError::rejected::<EditThreshold>)
            })
            .add::<TypeThreshold>(editable, |ws, action, _| {
                metric(ws, &action.metric).map_err(ActionError::rejected::<TypeThreshold>)
            })
            .when::<EditClusterAlerts>(editable)
            .when::<EditClusterWatchMode>(editable)
            .when::<ResetClusterOverrides>(editable)
            .when::<AddClusterOverride>(editable)
            .when::<CancelThresholdEntry>(|ws, _| ws.settings_page.thresholds.entry.is_some())
    }

    pub(super) fn bind_threshold_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<EditThreshold>(|ws, action, _, cx| {
            ws.edit_threshold(
                action.cluster.as_deref(),
                &action.metric,
                action.value.as_deref(),
                cx,
            )
        })
        .bind::<TypeThreshold>(|ws, action, window, cx| {
            ws.type_threshold(action.cluster.as_deref(), &action.metric, window, cx)
        })
        .bind::<CancelThresholdEntry>(|ws, _, window, cx| {
            ws.finish_threshold_entry(false, window, cx)
        })
        .bind::<EditClusterAlerts>(|ws, action, _, cx| {
            ws.edit_cluster_alerts(&action.cluster, action.choice.enabled(), cx)
        })
        .bind::<EditClusterWatchMode>(|ws, action, _, cx| {
            let change = Change::ClusterWatchMode {
                context: action.cluster.clone(),
                mode: action.mode.mode(),
            };
            ws.edit_settings(change, cx)
        })
        .bind::<ResetClusterOverrides>(|ws, action, _, cx| {
            ws.reset_cluster_overrides(&action.cluster, cx)
        })
        .bind::<AddClusterOverride>(|ws, action, _, cx| {
            ws.add_cluster_override(action.cluster.as_deref(), cx)
        })
    }

    /// The kubeconfig's contexts and their servers.
    pub(super) fn settings_clusters(&self, cx: &App) -> Vec<KnownCluster> {
        self.model
            .read(cx)
            .clusters
            .iter()
            .map(|c| KnownCluster {
                context: c.info.name.clone(),
                server: c.info.server.clone(),
            })
            .collect()
    }

    /// The settings shown: the file with the draft on top.
    fn shown_settings(&self, cx: &App) -> Settings {
        self.settings_page
            .draft
            .shown(&self.model.read(cx).settings)
    }

    /// The Cluster overrides rows, as drawn.
    fn override_rows(&self, cx: &App) -> Vec<OverrideRow> {
        settings_thresholds::override_rows(
            &self.shown_settings(cx),
            &self.settings_clusters(cx),
            &self.settings_page.thresholds.added,
        )
    }

    /// What Cluster overrides draws, for the Tab cycle.
    pub(super) fn overrides_layout(&self, cx: &App) -> OverridesLayout {
        if self.settings_page.section != Section::ClusterOverrides {
            return OverridesLayout::default();
        }
        let rows = self.override_rows(cx);
        let candidates =
            self.settings_page.thresholds.picking.then(|| {
                settings_thresholds::add_candidates(&self.settings_clusters(cx), &rows).len()
            });
        OverridesLayout {
            rows: rows.len(),
            candidates,
        }
    }

    /// `context`'s row, given one (as Add override does) if the kubeconfig defines it with a
    /// server and it has none yet.
    fn row_of(&mut self, context: &str, cx: &App) -> Option<(usize, OverrideRow)> {
        let find = |rows: Vec<OverrideRow>| {
            rows.into_iter()
                .enumerate()
                .find(|(_, row)| row.context == context)
        };
        if let Some(found) = find(self.override_rows(cx)) {
            return Some(found);
        }
        let addable = self
            .settings_clusters(cx)
            .iter()
            .any(|c| c.context == context && c.server.is_some());
        if !addable {
            return None;
        }
        self.settings_page.thresholds.added.push(context.to_owned());
        find(self.override_rows(cx))
    }

    /// The threshold `cluster` (a context name, or the global value without one) has for
    /// `metric`, and its control.
    fn threshold_target(
        &mut self,
        cluster: Option<&str>,
        metric: ThresholdMetric,
        cx: &App,
    ) -> Option<(ThresholdTarget, ThresholdStop)> {
        match cluster {
            None => Some((
                ThresholdTarget {
                    cluster: None,
                    metric,
                },
                ThresholdStop::Global(metric),
            )),
            Some(context) => {
                let (index, row) = self.row_of(context, cx)?;
                Some((row.target(metric)?, ThresholdStop::Cell(index, metric)))
            }
        }
    }

    /// Enters `text`, typed for a threshold, into the draft: a value its metric accepts (empty
    /// clears it) as an edit, anything else as rejected.
    fn edit_threshold(
        &mut self,
        cluster: Option<&str>,
        metric: &str,
        text: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let Some(metric) = settings_thresholds::metric_named(metric) else {
            return;
        };
        if let Some((target, _)) = self.threshold_target(cluster, metric, cx) {
            self.enter_threshold(target, text.unwrap_or_default(), cx);
        }
    }

    fn enter_threshold(&mut self, target: ThresholdTarget, text: &str, cx: &mut Context<Self>) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        match settings_thresholds::parse_threshold(target.metric, text) {
            Ok(value) => self.edit_settings(
                Change::Threshold {
                    cluster: target.cluster,
                    metric: target.metric,
                    value,
                },
                cx,
            ),
            Err(why) => {
                let file = self.model.read(cx).settings.clone();
                self.settings_page
                    .draft
                    .reject(target.field(), text.trim().to_owned(), why, &file);
                self.settings_page.save_note = None;
                cx.notify();
            }
        }
    }

    /// Puts a text field in place of a threshold's value, holding the value set at that level
    /// (or the text rejected for it), with focus in it. Enter or leaving it enters the text;
    /// Esc drops it.
    fn type_threshold(
        &mut self,
        cluster: Option<&str>,
        metric: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        let Some(metric) = settings_thresholds::metric_named(metric) else {
            return;
        };
        let Some((target, stop)) = self.threshold_target(cluster, metric, cx) else {
            return;
        };
        // Another field still open is entered first, as leaving it would.
        self.finish_threshold_entry(true, window, cx);
        let shown = self.shown_settings(cx);
        let text = match self.settings_page.draft.rejected(&target.field()) {
            Some(rejected) => rejected.text.clone(),
            None => target
                .explicit(&shown)
                .map(|v| v.to_string())
                .unwrap_or_default(),
        };
        let placeholder = target.inherited(&shown).to_string();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        input.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.select_all(window, cx);
            state.focus(window, cx);
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            |ws: &mut Self, _: &Entity<InputState>, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    ws.finish_threshold_entry(true, window, cx)
                }
                _ => {}
            },
        );
        self.settings_page.thresholds.entry = Some(ThresholdEntry {
            target,
            stop,
            input,
            _subscription: subscription,
        });
        self.settings_page.cursor = Some(Stop::Threshold(stop));
        cx.notify();
    }

    /// Closes the open threshold field, entering its text into the draft when `enter`, and
    /// gives focus back to Settings.
    pub(super) fn finish_threshold_entry(
        &mut self,
        enter: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.settings_page.thresholds.entry.take() else {
            return;
        };
        if enter {
            let text = entry.input.read(cx).value().to_string();
            self.enter_threshold(entry.target, &text, cx);
        }
        if self.settings_page.open {
            window.focus(&self.settings_page.focus, cx);
        }
        cx.notify();
    }

    fn edit_cluster_alerts(
        &mut self,
        context: &str,
        enabled: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        let Some((_, row)) = self.row_of(context, cx) else {
            return;
        };
        let Some(cluster) = row.pin() else {
            return;
        };
        self.edit_settings(Change::ClusterAlerts { cluster, enabled }, cx);
    }

    /// Clears every override of `context` in the draft and drops its row.
    fn reset_cluster_overrides(&mut self, context: &str, cx: &mut Context<Self>) {
        let rows = self.override_rows(cx);
        let row = rows.iter().find(|row| row.context == context);
        let mut changes = vec![Change::ClusterWatchMode {
            context: context.to_owned(),
            mode: None,
        }];
        if let Some(cluster) = row.and_then(OverrideRow::pin) {
            changes.push(Change::ClusterAlerts {
                cluster: cluster.clone(),
                enabled: None,
            });
            changes.extend(ThresholdMetric::ALL.map(|metric| Change::Threshold {
                cluster: Some(cluster.clone()),
                metric,
                value: None,
            }));
        }
        for change in changes {
            self.edit_settings(change, cx);
        }
        self.settings_page.thresholds.added.retain(|c| c != context);
        cx.notify();
    }

    /// Without `context`, shows or hides the clusters Add override offers; with one, gives it a
    /// row and closes the list.
    fn add_cluster_override(&mut self, context: Option<&str>, cx: &mut Context<Self>) {
        let page = &mut self.settings_page;
        if !page.open || page.saving.is_some() {
            return;
        }
        let Some(context) = context else {
            page.thresholds.picking = !page.thresholds.picking;
            cx.notify();
            return;
        };
        if let Some((index, _)) = self.row_of(context, cx) {
            self.settings_page.thresholds.picking = false;
            self.settings_page.cursor = Some(Stop::Threshold(ThresholdStop::Cell(
                index,
                ThresholdMetric::ALL[0],
            )));
        }
        cx.notify();
    }

    /// Enter or Space on a control of the two sections: what clicking it does.
    pub(super) fn activate_threshold_stop(
        &mut self,
        stop: ThresholdStop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.override_rows(cx);
        let shown = self.shown_settings(cx);
        let row = |index: usize| rows.get(index).cloned();
        match stop {
            ThresholdStop::Global(metric) => {
                self.type_threshold(None, metric.key(), window, cx);
            }
            ThresholdStop::Cell(index, metric) => {
                if let Some(row) = row(index) {
                    self.type_threshold(Some(&row.context), metric.key(), window, cx);
                }
            }
            ThresholdStop::Alerts(index) => {
                if let Some(row) = row(index) {
                    let next = row.alerts(&shown).next();
                    self.edit_cluster_alerts(&row.context, next.enabled(), cx);
                }
            }
            ThresholdStop::Watch(index) => {
                if let Some(row) = row(index) {
                    let next = row.watch(&shown).next();
                    let change = Change::ClusterWatchMode {
                        context: row.context.clone(),
                        mode: next.mode(),
                    };
                    self.edit_settings(change, cx);
                }
            }
            ThresholdStop::Reset(index) => {
                if let Some(row) = row(index) {
                    self.reset_cluster_overrides(&row.context, cx);
                }
            }
            ThresholdStop::Add => self.add_cluster_override(None, cx),
            ThresholdStop::Pick(index) => {
                let candidates =
                    settings_thresholds::add_candidates(&self.settings_clusters(cx), &rows);
                if let Some(context) = candidates.get(index) {
                    self.add_cluster_override(Some(context), cx);
                }
            }
        }
    }

    /// Drops what the two sections hold besides the draft: Revert and Reload.
    pub(super) fn revert_thresholds_page(&mut self) {
        let page = &mut self.settings_page.thresholds;
        page.added.clear();
        page.picking = false;
        page.entry = None;
    }
}
