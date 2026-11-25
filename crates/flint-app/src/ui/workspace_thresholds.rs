//! The workspace's side of Settings' Thresholds and Cluster overrides
//! ([`settings_thresholds`]): their actions and guards, the text field a threshold is typed in,
//! and their controls under the keyboard cursor.
//!
//! Every edit goes into the Settings draft through [`Workspace::edit_settings`], as the core's
//! own write; a number its metric does not accept goes in as rejected and blocks Save.

// module-tests: settings_thresholds_tests.rs

use std::sync::Arc;

use flint_core::settings::{ClusterPin, Settings};
use flint_core::thresholds::ThresholdMetric;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;

use super::super::registry::{ActionError, Bind, Guards};
use super::super::settings_draft::{Change, Field};
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
        fn metric(key: &str) -> Result<(), String> {
            settings_thresholds::metric_named(key)
                .map(drop)
                .ok_or_else(|| {
                    let keys: Vec<_> = ThresholdMetric::ALL.map(ThresholdMetric::key).to_vec();
                    format!("the metric is one of {}", keys.join(", "))
                })
        }
        /// `context` names a cluster the action can apply to: one with a row, or one the
        /// kubeconfig defines (with a server, when `bound`).
        fn cluster(ws: &Workspace, context: &str, bound: bool, cx: &App) -> Result<(), String> {
            let row = ws
                .override_rows(cx)
                .into_iter()
                .find(|row| row.context == context);
            let known = ws
                .settings_clusters(cx)
                .into_iter()
                .find(|c| c.context == context);
            let usable = match (row, known) {
                (Some(row), _) => !bound || row.server.is_some(),
                (None, Some(known)) => !bound || known.server.is_some(),
                (None, None) => false,
            };
            if usable {
                Ok(())
            } else if bound {
                Err(format!(
                    "\"{context}\" is not a cluster with a server to bind an override to"
                ))
            } else {
                Err(format!("\"{context}\" is not a cluster"))
            }
        }
        fn threshold(
            ws: &Workspace,
            context: Option<&str>,
            key: &str,
            cx: &App,
        ) -> Result<(), String> {
            metric(key)?;
            context.map_or(Ok(()), |c| cluster(ws, c, true, cx))
        }
        guards
            .add::<EditThreshold>(editable, |ws, action, cx| {
                threshold(ws, action.cluster.as_deref(), &action.metric, cx)
                    .map_err(ActionError::rejected::<EditThreshold>)
            })
            .add::<TypeThreshold>(editable, |ws, action, cx| {
                threshold(ws, action.cluster.as_deref(), &action.metric, cx)
                    .map_err(ActionError::rejected::<TypeThreshold>)
            })
            .add::<EditClusterAlerts>(editable, |ws, action, cx| {
                cluster(ws, &action.cluster, true, cx)
                    .map_err(ActionError::rejected::<EditClusterAlerts>)
            })
            .add::<EditClusterWatchMode>(editable, |ws, action, cx| {
                cluster(ws, &action.cluster, false, cx)
                    .map_err(ActionError::rejected::<EditClusterWatchMode>)
            })
            .add::<ResetClusterOverrides>(editable, |ws, action, cx| {
                let has_row = ws
                    .override_rows(cx)
                    .iter()
                    .any(|r| r.context == action.cluster);
                if has_row {
                    Ok(())
                } else {
                    Err(ActionError::rejected::<ResetClusterOverrides>(format!(
                        "\"{}\" has no row in Cluster overrides",
                        action.cluster
                    )))
                }
            })
            .add::<AddClusterOverride>(editable, |ws, action, cx| match &action.cluster {
                None => Ok(()),
                Some(c) => {
                    cluster(ws, c, true, cx).map_err(ActionError::rejected::<AddClusterOverride>)
                }
            })
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
            ws.edit_cluster_watch(&action.cluster, action.mode.mode(), cx)
        })
        .bind::<ResetClusterOverrides>(|ws, action, window, cx| {
            ws.reset_cluster_overrides(&action.cluster, window, cx)
        })
        .bind::<AddClusterOverride>(|ws, action, window, cx| {
            ws.add_cluster_override(action.cluster.as_deref(), window, cx)
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
                // A blank server binds nothing: the setters refuse it.
                server: c.info.server.clone().filter(|s| !s.trim().is_empty()),
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
            self.enter_threshold(target, text.unwrap_or_default(), None, cx);
        }
    }

    /// Keeps `context`'s row until Save or Revert once the draft touches it, so a row whose
    /// last override is cleared does not vanish under the keyboard cursor.
    fn keep_row(&mut self, context: &str) {
        let added = &mut self.settings_page.thresholds.added;
        if !added.iter().any(|c| c == context) {
            added.push(context.to_owned());
        }
    }

    /// Enters `text` for `target` into the draft. Also used once Settings is left with a field
    /// open, so it does not ask for Settings to be shown; only a Save under way refuses it.
    /// `opened_over` is the file a field showed when it opened: a draft this starts is over it,
    /// so a change to the file since shows as one.
    fn enter_threshold(
        &mut self,
        target: ThresholdTarget,
        text: &str,
        opened_over: Option<&Arc<Settings>>,
        cx: &mut Context<Self>,
    ) {
        if self.settings_page.saving.is_some() {
            return;
        }
        if let Some(pin) = &target.cluster {
            self.keep_row(&pin.context);
        }
        let file = self.model.read(cx).settings.clone();
        let draft = &mut self.settings_page.draft;
        if let Some(opened_over) = opened_over {
            draft.begin(opened_over);
        }
        match settings_thresholds::parse_threshold(target.metric, text) {
            Ok(value) => draft.edit(
                Change::Threshold {
                    cluster: target.cluster,
                    metric: target.metric,
                    value,
                },
                &file,
            ),
            Err(why) => draft.reject(target.field(), text.trim().to_owned(), why, &file),
        }
        self.settings_page.save_note = None;
        cx.notify();
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
        // Another field still open is entered first, as leaving it would; before the new
        // field's row is found, since entering may change the rows.
        self.finish_threshold_entry(true, window, cx);
        let Some((target, stop)) = self.threshold_target(cluster, metric, cx) else {
            return;
        };
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
        let text_opened = text.clone();
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
            opened: text_opened,
            file: self.model.read(cx).settings.clone(),
            _subscription: subscription,
        });
        self.settings_page.cursor = Some(Stop::Threshold(stop));
        self.track_threshold_cursor(cx);
        cx.notify();
    }

    /// Closes the open threshold field, entering its text into the draft when `enter`. Focus
    /// goes back to Settings only if the field held it, so a dialog or the palette that took
    /// focus keeps it. While a Save is being written, a field to enter stays open: its text
    /// would be refused.
    pub(super) fn finish_threshold_entry(
        &mut self,
        enter: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if enter && self.settings_page.saving.is_some() {
            return;
        }
        let Some(entry) = self.settings_page.thresholds.entry.take() else {
            return;
        };
        let had_focus = entry.input.focus_handle(cx).is_focused(window);
        if enter && let Some(text) = entry.typed(cx) {
            self.enter_threshold(entry.target, &text, Some(&entry.file), cx);
        }
        if had_focus && self.settings_page.open {
            window.focus(&self.settings_page.focus, cx);
        }
        cx.notify();
    }

    /// Enters the text of a field left open when Settings was left with no window at hand
    /// (the navigation). Called as the workspace draws.
    pub(super) fn flush_threshold_entry(&mut self, cx: &mut Context<Self>) {
        if self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        if let Some(entry) = self.settings_page.thresholds.entry.take()
            && let Some(text) = entry.typed(cx)
        {
            self.enter_threshold(entry.target, &text, Some(&entry.file), cx);
        }
    }

    fn edit_cluster_watch(
        &mut self,
        context: &str,
        mode: Option<flint_core::WatchMode>,
        cx: &mut Context<Self>,
    ) {
        if self.settings_page.saving.is_some() {
            return;
        }
        self.keep_row(context);
        let change = Change::ClusterWatchMode {
            context: context.to_owned(),
            mode,
        };
        self.edit_settings(change, cx);
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
        if self.settings_page.saving.is_some() {
            return;
        }
        self.keep_row(context);
        self.edit_settings(Change::ClusterAlerts { cluster, enabled }, cx);
    }

    /// Clears every override of `context` in the draft and drops its row. Text typed in the
    /// open field is dropped too, and the keyboard cursor moves to Add override rather than onto
    /// the row that takes this one's place.
    fn reset_cluster_overrides(
        &mut self,
        context: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        // Text typed for this cluster goes with its overrides; for another, it is entered.
        let own = self
            .settings_page
            .thresholds
            .entry
            .as_ref()
            .and_then(|entry| entry.target.cluster.as_ref())
            .is_some_and(|pin| pin.context == context);
        self.finish_threshold_entry(!own, window, cx);
        // Edits made while the context pointed to another server are hidden now: they go too,
        // or a later Save would still write them.
        self.settings_page.draft.forget(|field| match field {
            Field::Threshold {
                cluster: Some(pin), ..
            }
            | Field::ClusterAlerts(pin) => pin.context == context,
            _ => false,
        });
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
        // A context gone from the kubeconfig may hold overrides pinned to several servers: its
        // one row's Reset clears them all.
        if !self
            .settings_clusters(cx)
            .iter()
            .any(|known| known.context == context)
        {
            let shown = self.shown_settings(cx);
            let pins = shown
                .thresholds
                .clusters
                .iter()
                .map(|c| c.cluster.clone())
                .chain(
                    shown
                        .cluster_alerts
                        .iter()
                        .map(|a| ClusterPin::new(a.context.clone(), a.server.clone())),
                )
                .filter(|pin| pin.context == context);
            for pin in pins {
                changes.push(Change::ClusterAlerts {
                    cluster: pin.clone(),
                    enabled: None,
                });
                changes.extend(ThresholdMetric::ALL.map(|metric| Change::Threshold {
                    cluster: Some(pin.clone()),
                    metric,
                    value: None,
                }));
            }
        }
        for change in changes {
            self.edit_settings(change, cx);
        }
        self.settings_page.thresholds.added.retain(|c| c != context);
        if matches!(self.settings_page.cursor, Some(Stop::Threshold(_))) {
            self.settings_page.cursor = Some(Stop::Threshold(ThresholdStop::Add));
        }
        cx.notify();
    }

    /// Without `context`, shows or hides the clusters Add override offers; with one, gives it a
    /// row and closes the list.
    fn add_cluster_override(
        &mut self,
        context: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.settings_page.open || self.settings_page.saving.is_some() {
            return;
        }
        self.finish_threshold_entry(true, window, cx);
        // From the palette, over another section: show the one with the list and the rows.
        let elsewhere = self.settings_page.section != Section::ClusterOverrides;
        if elsewhere {
            self.show_settings_section(Section::ClusterOverrides, window, cx);
        }
        let page = &mut self.settings_page;
        let Some(context) = context else {
            page.thresholds.picking = elsewhere || !page.thresholds.picking;
            cx.notify();
            return;
        };
        if let Some((index, _)) = self.row_of(context, cx) {
            self.settings_page.thresholds.picking = false;
            self.settings_page.cursor = Some(Stop::Threshold(ThresholdStop::Cell(
                index,
                ThresholdMetric::ALL[0],
            )));
            self.track_threshold_cursor(cx);
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
        if self.settings_page.saving.is_some() {
            return;
        }
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
                    self.edit_cluster_watch(&row.context, next.mode(), cx);
                }
            }
            ThresholdStop::Reset(index) => {
                if let Some(row) = row(index) {
                    self.reset_cluster_overrides(&row.context, window, cx);
                }
            }
            ThresholdStop::Add => self.add_cluster_override(None, window, cx),
            ThresholdStop::Pick(index) => {
                let candidates =
                    settings_thresholds::add_candidates(&self.settings_clusters(cx), &rows);
                if let Some(context) = candidates.get(index) {
                    self.add_cluster_override(Some(context), window, cx);
                }
            }
        }
    }

    /// Remembers which cluster's row the keyboard cursor stands on, and scrolls its control
    /// into view.
    pub(super) fn track_threshold_cursor(&mut self, cx: &App) {
        let Some(Stop::Threshold(stop)) = self.settings_page.cursor else {
            self.settings_page.thresholds.cursor_row = None;
            return;
        };
        let rows = self.override_rows(cx);
        self.settings_page.thresholds.cursor_row = match stop {
            ThresholdStop::Pick(index) => {
                let candidates =
                    settings_thresholds::add_candidates(&self.settings_clusters(cx), &rows);
                self.settings_page
                    .thresholds
                    .pick_scroll
                    .scroll_to_item(index);
                candidates.get(index).cloned()
            }
            _ => stop
                .row()
                .and_then(|index| rows.get(index))
                .map(|row| row.context.clone()),
        };
        self.settings_page
            .scroll
            .scroll_to_item(settings_thresholds::scroll_index(stop, rows.len()));
    }

    /// Keeps the keyboard cursor and the open field on their cluster when rows above come or
    /// go (a Save, a reload of the file or kubeconfig), and drops the cursor when its row is
    /// gone. Retires a value rejected for an override no row is bound to any more, Settings open
    /// or not. Called as the workspace draws and before the cursor moves or is used.
    pub(super) fn follow_threshold_rows(&mut self, cx: &App) {
        if !self.settings_page.open && !self.settings_page.draft.has_rejected() {
            return;
        }
        let rows = self.override_rows(cx);
        // A value rejected for an override no row is bound to any more (its context now
        // points elsewhere) has nowhere to be fixed: it would block Save for good.
        let bound: Vec<_> = rows.iter().filter_map(OverrideRow::pin).collect();
        self.settings_page
            .draft
            .retain_rejected(|field| match field {
                Field::Threshold {
                    cluster: Some(pin), ..
                } => bound.iter().any(|b| b.matches(&pin.context, &pin.server)),
                _ => true,
            });
        if !self.settings_page.open {
            return;
        }
        let index_of = |context: &str| rows.iter().position(|row| row.context == context);
        if let Some(Stop::Threshold(stop)) = self.settings_page.cursor
            && let Some(context) = &self.settings_page.thresholds.cursor_row
        {
            if let ThresholdStop::Pick(_) = stop {
                // A candidate is followed by name, and left for Add override once it is no
                // longer offered, so Enter never adds another cluster.
                let candidates =
                    settings_thresholds::add_candidates(&self.settings_clusters(cx), &rows);
                let pick = candidates.iter().position(|c| c == context);
                self.settings_page.cursor = Some(Stop::Threshold(
                    pick.map_or(ThresholdStop::Add, ThresholdStop::Pick),
                ));
            } else if stop.row().is_some() {
                self.settings_page.cursor =
                    index_of(context).map(|i| Stop::Threshold(stop.at_row(i)));
            }
        }
        if let Some(entry) = &mut self.settings_page.thresholds.entry
            && let Some(pin) = &entry.target.cluster
            && let Some(i) = index_of(&pin.context)
        {
            entry.stop = entry.stop.at_row(i);
        }
    }

    /// Closes the open field when its cluster no longer has a row to draw it in (entering
    /// its text), so focus is not left on a field that is not drawn.
    pub(super) fn close_orphaned_threshold_entry(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let orphaned = self
            .settings_page
            .thresholds
            .entry
            .as_ref()
            .is_some_and(|entry| {
                entry.target.cluster.as_ref().is_some_and(|pin| {
                    !self
                        .override_rows(cx)
                        .iter()
                        .filter_map(OverrideRow::pin)
                        .any(|row| row.matches(&pin.context, &pin.server))
                })
            });
        if orphaned && self.settings_page.open {
            self.finish_threshold_entry(true, window, cx);
        }
    }

    /// Drops what the two sections hold besides the draft: Revert and Reload. A field still
    /// open is closed by the caller first, which has the window to give focus back.
    pub(super) fn revert_thresholds_page(&mut self) {
        let page = &mut self.settings_page.thresholds;
        page.added.clear();
        page.picking = false;
        page.entry = None;
    }

    /// Once a Save is written, rows given by Add override or kept while edited stay only if
    /// they differ from the global settings.
    pub(super) fn saved_thresholds_page(&mut self) {
        self.settings_page.thresholds.added.clear();
    }
}
