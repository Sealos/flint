//! Compare an object across clusters (#449): one column per cluster, named exactly, with the
//! fields that differ. Strictly read-only.
//!
//! Reads go through [`CoreHandle::fetch_object`], which uses a connection that already exists and
//! never starts one, so a cluster in the background can't launch a credential command from here:
//! it shows as unavailable. The comparison itself is [`flint_core::compare`].

#![allow(
    dead_code,
    reason = "a later change opens this view from the menu and the registry"
)]

use std::sync::Arc;

use flint_core::CoreHandle;
use flint_core::compare::{
    CompareOptions, CompareRow, CompareTarget, Comparison, FieldValue, RowState, SideRead,
    SideState, Verdict, compare,
};
use flint_core::display::{ClusterNames, single_line};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::*;

use super::object::MASK;

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

/// `clusters` once each, in order.
fn distinct(clusters: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    clusters
        .into_iter()
        .filter(|cluster| seen.insert(cluster.clone()))
        .collect()
}

/// Compares `target` on `clusters`. Open it with [`CompareView::new`].
pub struct CompareView {
    core: CoreHandle,
    target: CompareTarget,
    clusters: Vec<String>,
    names: ClusterNames,
    reads: Vec<SideRead>,
    options: CompareOptions,
    show_same: bool,
    comparison: Arc<Comparison>,
    model: CompareModel,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Replaced on each refresh, which drops the reads still running.
    fetches: Vec<Task<()>>,
    rebuild: Option<Task<()>>,
}

impl CompareView {
    /// Reads `target` on each of `clusters` and compares them. `names` must cover the kubeconfig's
    /// contexts, so each column names its exact cluster.
    pub fn new(
        core: CoreHandle,
        target: CompareTarget,
        clusters: Vec<String>,
        names: ClusterNames,
        cx: &mut Context<Self>,
    ) -> Self {
        let clusters = distinct(clusters);
        let mut view = Self {
            core,
            target,
            reads: vec![SideRead::Reading; clusters.len()],
            clusters,
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

    /// Reads every cluster again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reads = vec![SideRead::Reading; self.clusters.len()];
        let fetches = self
            .clusters
            .iter()
            .enumerate()
            .map(|(side, cluster)| {
                let fetch = self.core.fetch_object(
                    cluster,
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

    pub fn model(&self) -> &CompareModel {
        &self.model
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
                .child(div().truncate().child(side.name.clone()))
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
            .key_context("Compare")
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
                    ),
            )
            .child(
                h_flex()
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
