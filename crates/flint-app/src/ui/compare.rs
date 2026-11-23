//! Compare an object across clusters (#449): one column per cluster, named exactly, with the
//! fields that differ. Strictly read-only.
//!
//! Reads go through [`CoreHandle::fetch_pinned_object`]: one GET on a connection that already
//! exists, so a cluster in the background can't launch a credential command from here (it shows
//! as unavailable). Each column stays on the definition it opened on ([`ReadPin`]): a paused or
//! re-pointed cluster is not asked, and what a re-pointed one answered is dropped.
//! The comparison itself is [`flint_core::compare`].

use std::sync::Arc;

use flint_core::compare::{
    CompareOptions, CompareRow, CompareTarget, Comparison, FieldValue, RowState, SideRead,
    SideState, Verdict, compare,
};
use flint_core::display::{ClusterNames, single_line};
use flint_core::handle::ReadPin;
use flint_core::read::KindId;
use flint_core::{ClusterSummary, ConnectionState, CoreHandle};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use serde::Deserialize;

use super::clusters::ClustersModel;
use super::dock::{DockItem, TabKey};
use super::dock_model::TabStatus;
use super::object::MASK;
use super::registry::{self, Bind};

actions!(
    compare,
    [
        /// Compare the selected object across the connected clusters, in a dock tab.
        OpenCompare,
        /// Read the compared object again on every cluster.
        RefreshCompare,
        /// Show or hide the fields that match.
        ToggleCompareSame,
        /// Include or ignore the fields each cluster assigns for itself.
        ToggleCompareVolatile
    ]
);

/// Compare one exact object across the connected clusters: what a row's menu opens, so it stays
/// that row's object whatever gets selected while the menu is open.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = compare, no_json)]
#[serde(deny_unknown_fields)]
pub struct CompareObject {
    pub cluster: String,
    /// `plural`, or `plural.group`.
    pub kind: String,
    #[serde(default)]
    pub namespace: Option<String>,
    pub name: String,
}

impl CompareObject {
    pub fn target(&self) -> CompareTarget {
        let (plural, group) = self.kind.split_once('.').unwrap_or((&self.kind, ""));
        CompareTarget {
            kind: KindId::new(group, plural),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
        }
    }
}

/// The key context of the compare view.
pub const COMPARE_VIEW: &str = "Compare";
/// Why a paused cluster is not read.
pub const PAUSED: &str = "paused while its kubeconfig can't be confirmed";
/// Why a cluster that left kubeconfig is not read.
const REMOVED: &str = "no longer in kubeconfig";
/// Why a column's cluster is not read, and what it read is dropped.
pub const CHANGED: &str = "its connection changed since this comparison opened; open it again";
/// Why there is nothing to compare with.
pub const NO_OTHER_CLUSTER: &str = "no other cluster is connected";

/// A row's height.
const ROW_HEIGHT: f32 = 24.;
/// The field path column's width.
const PATH_WIDTH: f32 = 280.;
/// The longest value drawn in a cell, in characters. The comparison uses the whole value.
const MAX_VALUE_CHARS: usize = 200;
/// What a cell says for a field the object does not set.
pub const NOT_SET: &str = "not set";
/// What a cell and a header say for a cluster without the object.
pub const MISSING: &str = "Missing";
/// What a cell says when a cluster could not be read.
pub const UNAVAILABLE: &str = "Unavailable";
/// What a cell and a header say while a cluster's answer is on its way.
pub const READING: &str = "Reading…";

/// One cluster's column header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideHeader {
    /// The cluster, told from every other ([`ClusterNames::exact`]).
    pub name: String,
    pub status: String,
    pub problem: bool,
}

/// One row as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowModel {
    pub path: String,
    pub cells: Vec<String>,
    pub state: RowState,
}

/// What the view draws, built without a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompareModel {
    pub title: String,
    pub verdict: String,
    pub headers: Vec<SideHeader>,
    pub rows: Vec<RowModel>,
}

impl CompareModel {
    pub fn new(
        target: &CompareTarget,
        clusters: &[String],
        names: &ClusterNames,
        comparison: &Comparison,
        show_same: bool,
    ) -> Self {
        let headers = clusters
            .iter()
            .zip(&comparison.sides)
            .map(|(cluster, side)| header(names.exact(cluster), side))
            .collect();
        let rows = comparison
            .rows
            .iter()
            .filter(|row| show_same || row.state != RowState::Same)
            .map(|row| row_model(row, &comparison.sides))
            .collect();
        Self {
            title: title(target),
            verdict: verdict_text(comparison.verdict()),
            headers,
            rows,
        }
    }
}

fn title(target: &CompareTarget) -> String {
    let name = single_line(&target.name);
    let kind = single_line(target.kind.as_str());
    match &target.namespace {
        Some(namespace) => format!("{kind} {}/{name}", single_line(namespace)),
        None => format!("{kind} {name}"),
    }
}

fn header(name: String, side: &SideState) -> SideHeader {
    let (status, problem) = match side {
        SideState::Reading => (READING.to_owned(), false),
        SideState::Present => ("Read".to_owned(), false),
        SideState::Missing => (MISSING.to_owned(), true),
        SideState::Unavailable(why) => (format!("{UNAVAILABLE}: {}", shown(why)), true),
    };
    SideHeader {
        name,
        status,
        problem,
    }
}

fn row_model(row: &CompareRow, sides: &[SideState]) -> RowModel {
    RowModel {
        path: single_line(&row.path),
        cells: row
            .values
            .iter()
            .zip(sides)
            .map(|(value, side)| cell_text(value, side))
            .collect(),
        state: row.state,
    }
}

fn cell_text(value: &FieldValue, side: &SideState) -> String {
    match value {
        FieldValue::Unknown if *side == SideState::Reading => READING.to_owned(),
        FieldValue::Value(serde_json::Value::String(text)) => shown(text),
        FieldValue::Value(other) => shown(&other.to_string()),
        FieldValue::Hidden => MASK.to_owned(),
        FieldValue::NotSet => NOT_SET.to_owned(),
        FieldValue::NoObject => MISSING.to_owned(),
        FieldValue::Unknown => UNAVAILABLE.to_owned(),
    }
}

/// `text` on one line and bounded. A cut value says how long it was, so two values that differ
/// only past the cut don't read as a mistake in the row's state.
fn shown(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_VALUE_CHARS {
        return single_line(text);
    }
    let start: String = text.chars().take(MAX_VALUE_CHARS).collect();
    format!("{}… ({total} characters)", single_line(&start))
}

fn verdict_text(verdict: Verdict) -> String {
    match verdict {
        Verdict::Reading => READING.to_owned(),
        Verdict::NotComparable => "Fewer than two clusters have a readable object.".to_owned(),
        Verdict::Differs { fields: 1 } => "1 field differs.".to_owned(),
        Verdict::Differs { fields } => format!("{fields} fields differ."),
        Verdict::MatchOnSome => {
            "The readable objects match, but not every cluster could be compared.".to_owned()
        }
        Verdict::SameExceptHidden { fields: 1 } => {
            "No differences found. 1 hidden value was not compared.".to_owned()
        }
        Verdict::SameExceptHidden { fields } => {
            format!("No differences found. {fields} hidden values were not compared.")
        }
        Verdict::Same => "No differences.".to_owned(),
    }
}

/// A column's cluster name, wrapped rather than cut: names that differ only at the end (two ARNs)
/// would otherwise clip to the same text in a narrow window.
fn side_name(name: &str) -> Div {
    div().w_full().whitespace_normal().child(name.to_owned())
}

/// `clusters` once each, in order.
fn distinct(clusters: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    clusters
        .into_iter()
        .filter(|cluster| seen.insert(cluster.clone()))
        .collect()
}

fn is_connected(cluster: &ClusterSummary) -> bool {
    matches!(cluster.state, ConnectionState::Connected(_))
}

/// The columns for an object on `origin`: it first, then every other connected cluster, and the
/// paused ones, which show why they were not read rather than disappear.
pub fn compare_clusters(origin: &str, clusters: &[ClusterSummary]) -> Vec<String> {
    std::iter::once(origin.to_owned())
        .chain(
            clusters
                .iter()
                .filter(|c| c.info.name != origin && (c.paused || is_connected(c)))
                .map(|c| c.info.name.clone()),
        )
        .collect()
}

/// Whether an object on `origin` can be compared now: `Err` says why not.
pub fn can_compare(origin: &str, clusters: &[ClusterSummary]) -> Result<(), String> {
    if let Some(why) = held_back(origin, clusters) {
        return Err(format!("this cluster is {why}"));
    }
    if !clusters
        .iter()
        .any(|c| c.info.name == origin && is_connected(c))
    {
        return Err("this cluster is not connected".to_owned());
    }
    if has_other_cluster(origin, clusters) {
        Ok(())
    } else {
        Err(NO_OTHER_CLUSTER.to_owned())
    }
}

/// Whether a cluster other than `origin` could be compared with now.
pub fn has_other_cluster(origin: &str, clusters: &[ClusterSummary]) -> bool {
    clusters
        .iter()
        .any(|c| c.info.name != origin && !c.paused && is_connected(c))
}

/// `why` a comparison did not open, as one sentence.
pub fn refused(why: &str) -> String {
    format!(
        "Can't compare this object: {}.",
        why.trim_end().trim_end_matches('.')
    )
}

/// Why `cluster` must not be sent a read, if so. Checked before every read: the core refuses
/// these too, and this keeps the request from being made at all.
pub fn held_back(cluster: &str, clusters: &[ClusterSummary]) -> Option<&'static str> {
    match clusters.iter().find(|c| c.info.name == cluster) {
        Some(c) if c.paused => Some(PAUSED),
        Some(_) => None,
        None => Some(REMOVED),
    }
}

/// Why column `cluster`, opened at `pin`, must not be read now. A pause comes first: while the
/// server shown may be a fallback, it says nothing yet about the context having moved.
fn column_held(
    cluster: &str,
    pin: Option<&ReadPin>,
    clusters: &[ClusterSummary],
) -> Option<&'static str> {
    held_back(cluster, clusters).or_else(|| {
        let now = clusters.iter().find(|c| c.info.name == cluster)?;
        (pin != Some(&now.read_pin())).then_some(CHANGED)
    })
}

/// Each of `columns` as its cluster is now.
fn column_pins(columns: &[String], clusters: &[ClusterSummary]) -> Vec<Option<ReadPin>> {
    columns
        .iter()
        .map(|column| {
            clusters
                .iter()
                .find(|c| c.info.name == *column)
                .map(ClusterSummary::read_pin)
        })
        .collect()
}

/// Names told from the current contexts and from `columns`, which may have left kubeconfig.
fn column_names(
    columns: &[String],
    clusters: &[ClusterSummary],
    custom: &std::collections::BTreeMap<String, String>,
) -> ClusterNames {
    let all = distinct(
        clusters
            .iter()
            .map(|c| c.info.name.clone())
            .chain(columns.iter().cloned())
            .collect(),
    );
    ClusterNames::new(all.iter().map(String::as_str), custom)
}

/// Compares `target` on `clusters`. Open it with [`CompareView::new`].
pub struct CompareView {
    core: CoreHandle,
    source: Entity<ClustersModel>,
    /// The cluster the comparison was opened from: the tab belongs to it.
    origin: String,
    target: CompareTarget,
    clusters: Vec<String>,
    /// What each column's cluster was when it opened. `None`: it was not in kubeconfig.
    pins: Vec<Option<ReadPin>>,
    names: ClusterNames,
    reads: Vec<SideRead>,
    options: CompareOptions,
    show_same: bool,
    comparison: Arc<Comparison>,
    model: CompareModel,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// One per cluster, replaced on each refresh, which drops the reads still running.
    fetches: Vec<Task<()>>,
    rebuild: Option<Task<()>>,
    _source: Subscription,
}

impl CompareView {
    /// Reads `target` on `origin` and each of `clusters` and compares them, naming each column
    /// with `source`'s [`ClusterNames`] and following its renames and pauses.
    pub fn new(
        source: Entity<ClustersModel>,
        origin: String,
        target: CompareTarget,
        clusters: Vec<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let clusters = distinct(clusters);
        let (core, names, pins) = {
            let model = source.read(cx);
            (
                model.core().clone(),
                column_names(&clusters, &model.clusters, &model.settings.cluster_names),
                column_pins(&clusters, &model.clusters),
            )
        };
        let observed = cx.observe(&source, |view, _, cx| view.clusters_changed(cx));
        let mut view = Self {
            core,
            source,
            origin,
            _source: observed,
            target,
            reads: vec![SideRead::Reading; clusters.len()],
            clusters,
            pins,
            names,
            options: CompareOptions::default(),
            show_same: false,
            comparison: Arc::default(),
            model: CompareModel::default(),
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            fetches: Vec::new(),
            rebuild: None,
        };
        view.refresh(cx);
        view
    }

    /// The handlers of this view's registered actions.
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<RefreshCompare>(|view, _, _, cx| view.refresh(cx))
            .bind::<ToggleCompareSame>(|view, _, _, cx| {
                let show = !view.show_same;
                view.set_show_same(show, cx);
            })
            .bind::<ToggleCompareVolatile>(|view, _, _, cx| {
                let include = !view.options.include_volatile;
                view.set_include_volatile(include, cx);
            })
    }

    /// Reads every cluster again, except those held back ([`held_back`]).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let held: Vec<Option<&'static str>> = {
            let known = &self.source.read(cx).clusters;
            self.clusters
                .iter()
                .zip(&self.pins)
                .map(|(cluster, pin)| column_held(cluster, pin.as_ref(), known))
                .collect()
        };
        self.reads = held
            .iter()
            .map(|why| {
                why.map_or(SideRead::Reading, |why| {
                    SideRead::Unavailable(why.to_owned())
                })
            })
            .collect();
        let fetches = self
            .clusters
            .iter()
            .enumerate()
            .map(|(side, cluster)| {
                let Some(pin) = self.pins[side].as_ref().filter(|_| held[side].is_none()) else {
                    return Task::ready(());
                };
                let fetch = self.core.fetch_pinned_object(
                    cluster,
                    pin,
                    &self.target.kind,
                    self.target.namespace.as_deref(),
                    &self.target.name,
                );
                cx.spawn(async move |this, cx| {
                    let fetched = fetch.await;
                    let _ = this.update(cx, |view, cx| {
                        view.reads[side] = SideRead::from_fetch(&view.target, fetched);
                        view.recompare(cx);
                    });
                })
            })
            .collect();
        self.fetches = fetches;
        self.recompare(cx);
    }

    /// Opened again on this object: if a column's cluster has changed, every column takes the
    /// clusters as they are now. A tab whose columns all still hold is left as it is.
    pub fn reopen(&mut self, clusters: Vec<String>, cx: &mut Context<Self>) {
        let stale = {
            let known = &self.source.read(cx).clusters;
            self.clusters
                .iter()
                .zip(&self.pins)
                .any(|(c, pin)| column_held(c, pin.as_ref(), known) == Some(CHANGED))
        };
        if !stale {
            return;
        }
        self.clusters = distinct(clusters);
        self.pins = column_pins(&self.clusters, &self.source.read(cx).clusters);
        self.follow_names(cx);
        self.refresh(cx);
    }

    fn follow_names(&mut self, cx: &mut Context<Self>) {
        let names = {
            let model = self.source.read(cx);
            column_names(
                &self.clusters,
                &model.clusters,
                &model.settings.cluster_names,
            )
        };
        self.set_names(names, cx);
    }

    /// Shows or hides the fields that match.
    pub fn set_show_same(&mut self, show_same: bool, cx: &mut Context<Self>) {
        if self.show_same != show_same {
            self.show_same = show_same;
            self.rebuild_model(cx);
        }
    }

    /// Includes or ignores the fields each cluster assigns for itself.
    pub fn set_include_volatile(&mut self, include_volatile: bool, cx: &mut Context<Self>) {
        if self.options.include_volatile != include_volatile {
            self.options.include_volatile = include_volatile;
            self.recompare(cx);
        }
    }

    /// The cluster names changed (a rename, a reload).
    pub fn set_names(&mut self, names: ClusterNames, cx: &mut Context<Self>) {
        if self.names != names {
            self.names = names;
            self.rebuild_model(cx);
        }
    }

    #[cfg(any(test, feature = "automation"))]
    pub fn model(&self) -> &CompareModel {
        &self.model
    }

    #[cfg(any(test, feature = "automation"))]
    pub fn show_same(&self) -> bool {
        self.show_same
    }

    #[cfg(any(test, feature = "automation"))]
    pub fn include_volatile(&self) -> bool {
        self.options.include_volatile
    }

    /// The cluster list changed: follows renames, stops a read still running on a cluster that
    /// was paused or removed, and drops whatever a re-pointed cluster's column holds.
    fn clusters_changed(&mut self, cx: &mut Context<Self>) {
        self.follow_names(cx);
        let held: Vec<Option<&'static str>> = {
            let known = &self.source.read(cx).clusters;
            self.clusters
                .iter()
                .zip(&self.pins)
                .map(|(cluster, pin)| column_held(cluster, pin.as_ref(), known))
                .collect()
        };
        let mut stopped = false;
        for (side, why) in held.into_iter().enumerate() {
            let Some(why) = why else { continue };
            let stop = match &self.reads[side] {
                SideRead::Reading => true,
                SideRead::Unavailable(said) => why == CHANGED && said != CHANGED,
                _ => why == CHANGED,
            };
            if stop {
                self.fetches[side] = Task::ready(());
                self.reads[side] = SideRead::Unavailable(why.to_owned());
                stopped = true;
            }
        }
        if stopped {
            self.recompare(cx);
        }
    }

    /// Compares off the UI thread: objects can be large. A newer run replaces this one.
    fn recompare(&mut self, cx: &mut Context<Self>) {
        let (target, reads, options) = (self.target.clone(), self.reads.clone(), self.options);
        self.rebuild = Some(cx.spawn(async move |this, cx| {
            let comparison = cx
                .background_spawn(async move { compare(&target, &reads, options) })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.comparison = Arc::new(comparison);
                view.rebuild_model(cx);
            });
        }));
    }

    fn rebuild_model(&mut self, cx: &mut Context<Self>) {
        self.model = CompareModel::new(
            &self.target,
            &self.clusters,
            &self.names,
            &self.comparison,
            self.show_same,
        );
        cx.notify();
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        range
            .filter_map(|ix| {
                let row = self.model.rows.get(ix)?;
                let color = match row.state {
                    RowState::Differs => theme.warning,
                    RowState::Same => theme.foreground,
                    RowState::Hidden | RowState::NotCompared => theme.muted_foreground,
                };
                let label = format!("{}: {}", row.path, row.cells.join(", "));
                Some(
                    h_flex()
                        .id(("compare-row", ix))
                        .aria_label(label)
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .px_3()
                        .gap_3()
                        .text_xs()
                        .child(
                            div()
                                .w(px(PATH_WIDTH))
                                .flex_none()
                                .truncate()
                                .text_color(color)
                                .child(row.path.clone()),
                        )
                        .children(row.cells.iter().map(|cell| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(color)
                                .child(cell.clone())
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl Focusable for CompareView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl DockItem for CompareView {
    fn tab_key(&self) -> TabKey {
        TabKey::Compare(self.origin.clone(), self.target.clone())
    }

    fn tab_title(&self) -> SharedString {
        format!("Compare {}", single_line(&self.target.name)).into()
    }

    fn tab_tooltip(&self) -> SharedString {
        format!(
            "Compare {} across clusters. Read-only.",
            title(&self.target)
        )
        .into()
    }

    fn tab_status(&self) -> TabStatus {
        TabStatus::Live
    }

    fn tab_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn can_restart(&self) -> bool {
        false
    }
}

impl Render for CompareView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.model.rows.len();
        let headers = self.model.headers.iter().enumerate().map(|(ix, side)| {
            let status = if side.problem {
                theme.danger
            } else {
                theme.muted_foreground
            };
            v_flex()
                .id(("compare-side", ix))
                .aria_label(format!("{}: {}", side.name, side.status))
                .flex_1()
                .min_w_0()
                .child(side_name(&side.name))
                .child(
                    div()
                        .truncate()
                        .text_color(status)
                        .child(side.status.clone()),
                )
                .test_support()
        });
        v_flex()
            .id("compare")
            .key_context(COMPARE_VIEW)
            .track_focus(&self.focus)
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .child(
                        div()
                            .id("compare-title")
                            .aria_label(self.model.title.clone())
                            .truncate()
                            .child(self.model.title.clone())
                            .test_support(),
                    )
                    .child(
                        div()
                            .id("compare-verdict")
                            .aria_label(self.model.verdict.clone())
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.model.verdict.clone())
                            .test_support(),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("compare-refresh")
                                    .xsmall()
                                    .ghost()
                                    .label("Refresh")
                                    .tooltip("Read the object again on every cluster")
                                    .on_click(registry::on_click(RefreshCompare)),
                            )
                            .child(
                                Button::new("compare-show-same")
                                    .xsmall()
                                    .ghost()
                                    .label("Show Matching")
                                    .selected(self.show_same)
                                    .tooltip("Show the fields that match too")
                                    .on_click(registry::on_click(ToggleCompareSame)),
                            )
                            .child(
                                Button::new("compare-include-volatile")
                                    .xsmall()
                                    .ghost()
                                    .label("Cluster-Assigned")
                                    .selected(self.options.include_volatile)
                                    .tooltip("Include the fields each cluster assigns for itself")
                                    .on_click(registry::on_click(ToggleCompareVolatile)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .items_start()
                    .px_3()
                    .pb_2()
                    .gap_3()
                    .text_xs()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().w(px(PATH_WIDTH)).flex_none().child("Field"))
                    .children(headers),
            )
            .child(
                uniform_list("compare-rows", rows, cx.processor(Self::render_rows))
                    .track_scroll(&self.scroll)
                    .flex_1(),
            )
    }
}

#[cfg(test)]
#[path = "compare_tests.rs"]
mod tests;
