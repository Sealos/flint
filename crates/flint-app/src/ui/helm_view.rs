//! A cluster's Helm releases (ADR-0004), read-only, in a dock tab: the list on the left, and the
//! selected release's details, history, masked values or manifest on the right. Every `helm` run
//! goes through the core, held to the definition this tab opened on. Closing the tab or moving
//! the selection drops the run in flight, which ends it.

use std::ops::Range;
use std::sync::Arc;

use flint_core::ClusterId;
use flint_core::handle::CoreHandle;
use flint_core::helm::{
    HelmDetails, HelmError, HelmOutput, HelmPin, HelmQuery, HelmRelease, HelmRevision, MAX_ROWS,
    ReleaseRef,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::dock::{DockItem, TabKey, TabStatus};
use super::registry::{
    self, RefreshHelmReleases, SelectFirst, SelectHelmRelease, SelectLast, SelectNext, SelectPrev,
    ShowHelmDetails, ShowHelmHistory, ShowHelmManifest, ShowHelmValues,
};

const ROW_HEIGHT: f32 = 24.;
const LINE_HEIGHT: f32 = 18.;
/// Lines moved by Page Up and Page Down.
const PAGE: usize = 20;
pub const EMPTY: &str = "No Helm releases on this cluster.";
pub const NO_VALUES: &str = "This release has no user-supplied values.";
pub const MASK_NOTE: &str = "Secrets are masked by pattern, so one in an unusual format can get \
                             through. Chart notes are not shown.";

/// The panes of a selected release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Details,
    History,
    Values,
    Manifest,
}

impl Pane {
    pub const ALL: [Pane; 4] = [Pane::Details, Pane::History, Pane::Values, Pane::Manifest];

    pub fn label(self) -> &'static str {
        match self {
            Pane::Details => "Details",
            Pane::History => "History",
            Pane::Values => "Values",
            Pane::Manifest => "Manifest",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Pane::Details => "helm-pane-details",
            Pane::History => "helm-pane-history",
            Pane::Values => "helm-pane-values",
            Pane::Manifest => "helm-pane-manifest",
        }
    }

    /// The pane `step` places away, wrapping around.
    fn moved(self, step: isize) -> Pane {
        let count = Self::ALL.len() as isize;
        let at = Self::ALL.iter().position(|p| *p == self).unwrap_or(0) as isize;
        Self::ALL[(at + step).rem_euclid(count) as usize]
    }

    fn query(self, release: ReleaseRef) -> HelmQuery {
        match self {
            Pane::Details => HelmQuery::Details(release),
            Pane::History => HelmQuery::History(release),
            Pane::Values => HelmQuery::Values(release),
            Pane::Manifest => HelmQuery::Manifest(release),
        }
    }
}

/// Text held once, with where each line starts: a row borrows its line when drawn, so a large
/// manifest keeps no per-row copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lines {
    text: Arc<str>,
    starts: Vec<u32>,
}

impl Lines {
    pub fn new(text: impl Into<Arc<str>>) -> Self {
        let text: Arc<str> = text.into();
        let mut starts = vec![0];
        starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(ix, byte)| *byte == b'\n' && ix + 1 < text.len())
                .map(|(ix, _)| ix as u32 + 1),
        );
        Self { text, starts }
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn line(&self, ix: usize) -> &str {
        let Some(&start) = self.starts.get(ix) else {
            return "";
        };
        let end = self
            .starts
            .get(ix + 1)
            .map_or(self.text.len(), |next| *next as usize);
        self.text[start as usize..end].trim_end_matches('\n')
    }
}

/// A release's details as lines of text.
pub fn details_text(d: &HelmDetails) -> String {
    let chart = match (d.chart.is_empty(), d.chart_version.is_empty()) {
        (false, false) => format!("{}-{}", d.chart, d.chart_version),
        _ => d.chart.clone(),
    };
    [
        ("Name", d.name.as_str()),
        ("Namespace", d.namespace.as_str()),
        ("Revision", &d.revision.to_string()),
        ("Status", d.status.as_str()),
        ("Chart", chart.as_str()),
        ("App version", d.app_version.as_str()),
        ("First deployed", d.first_deployed.as_str()),
        ("Last deployed", d.last_deployed.as_str()),
        ("Description", d.description.as_str()),
    ]
    .iter()
    .map(|(label, value)| format!("{label}: {}\n", one_line(value)))
    .collect()
}

/// A release's revisions, newest first, one to a line.
pub fn history_text(revisions: &[HelmRevision]) -> String {
    revisions
        .iter()
        .map(|r| {
            format!(
                "{}  {}  {}  {}  {}  {}\n",
                r.revision,
                one_line(&r.status),
                one_line(&r.chart),
                one_line(&r.app_version),
                one_line(&r.updated),
                one_line(&r.description),
            )
        })
        .collect()
}

/// `text` from a cluster, on one line: its control characters can't break a row.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// What a pane shows for a query's output.
pub fn pane_lines(pane: Pane, output: HelmOutput) -> Lines {
    match output {
        HelmOutput::Details(details) => Lines::new(details_text(&details)),
        HelmOutput::History(history) => Lines::new(history_text(&history)),
        HelmOutput::Text(text) if text.is_empty() && pane == Pane::Values => Lines::new(NO_VALUES),
        HelmOutput::Text(text) => Lines::new(text),
        HelmOutput::Releases { .. } => Lines::new(""),
    }
}

/// The row to select in `releases`: the one selected before if it is still there.
pub fn keep_selection(releases: &[HelmRelease], previous: Option<(&str, &str)>) -> Option<usize> {
    if releases.is_empty() {
        return None;
    }
    let kept = previous.and_then(|(name, namespace)| {
        releases
            .iter()
            .position(|r| r.name == name && r.namespace == namespace)
    });
    Some(kept.unwrap_or(0))
}

/// One row, in one line, for accessibility and tests.
pub fn release_label(r: &HelmRelease) -> String {
    format!(
        "{} in {}, revision {}, {}, {}",
        one_line(&r.name),
        one_line(&r.namespace),
        r.revision,
        one_line(&r.status),
        one_line(&r.chart),
    )
}

enum Load<T> {
    Loading,
    Ready(T),
    Failed(HelmError),
}

pub struct HelmView {
    core: CoreHandle,
    cluster: ClusterId,
    /// Taken once, as the tab opens, even if it can't run yet. A run refused because the context
    /// changed stays refused: only a new tab pins the new definition. `None` until the core
    /// answers.
    pin: Option<Result<HelmPin, HelmError>>,
    _pinning: Task<()>,
    /// Said of the `helm` in use when other administrators could replace it. Asked on every
    /// read, and never remembered past this view.
    binary_warning: Option<String>,
    focus: FocusHandle,
    releases: Load<Arc<[HelmRelease]>>,
    truncated: bool,
    selected: Option<usize>,
    /// The selected release's name and namespace, kept across a refresh.
    last_selected: Option<(String, String)>,
    pane: Pane,
    content: Load<Lines>,
    list_scroll: UniformListScrollHandle,
    pane_scroll: UniformListScrollHandle,
    pane_line: usize,
    /// Dropped to end the run in flight.
    list_task: Option<Task<()>>,
    pane_task: Option<Task<()>>,
}

impl HelmView {
    pub fn new(core: CoreHandle, cluster: ClusterId, cx: &mut Context<Self>) -> Self {
        // Asked here, not in the task, so the pin is of the definition the tab opened on.
        let pinning = core.helm_pin(&cluster);
        let _pinning = cx.spawn(async move |this, cx| {
            let pin = pinning.await;
            let _ = this.update(cx, |view, cx| {
                view.pin = Some(pin);
                view.refresh(cx);
            });
        });
        Self {
            core,
            cluster,
            pin: None,
            _pinning,
            binary_warning: None,
            focus: cx.focus_handle(),
            releases: Load::Loading,
            truncated: false,
            selected: None,
            last_selected: None,
            pane: Pane::Details,
            content: Load::Loading,
            list_scroll: UniformListScrollHandle::new(),
            pane_scroll: UniformListScrollHandle::new(),
            pane_line: 0,
            list_task: None,
            pane_task: None,
        }
    }

    fn selected_release(&self) -> Option<&HelmRelease> {
        match &self.releases {
            Load::Ready(releases) => releases.get(self.selected?),
            _ => None,
        }
    }

    /// Reads the list again, then the pane shown.
    pub(in crate::ui) fn refresh(&mut self, cx: &mut Context<Self>) {
        // Before the pin is back there is nothing to read with: its arrival starts the read.
        let Some(pin) = self.pin.clone() else {
            return;
        };
        self.pane_task = None;
        self.releases = Load::Loading;
        let core = self.core.clone();
        let cluster = self.cluster.clone();
        self.list_task = Some(cx.spawn(async move |this, cx| {
            let result = match pin {
                Ok(pin) => {
                    let warning = core.helm_warning(&cluster, &pin).await;
                    let _ = this.update(cx, |view, cx| {
                        view.binary_warning = warning;
                        cx.notify();
                    });
                    core.helm_read(&cluster, &pin, HelmQuery::List).await
                }
                Err(refused) => Err(refused),
            };
            let _ = this.update(cx, |view, cx| view.list_loaded(result, cx));
        }));
        cx.notify();
    }

    fn list_loaded(&mut self, result: Result<HelmOutput, HelmError>, cx: &mut Context<Self>) {
        let previous = self
            .selected_or_last()
            .map(|(name, namespace)| (name.to_owned(), namespace.to_owned()));
        match result {
            Ok(HelmOutput::Releases {
                releases,
                truncated,
            }) => {
                self.selected = keep_selection(
                    &releases,
                    previous.as_ref().map(|(n, s)| (n.as_str(), s.as_str())),
                );
                self.last_selected = self
                    .selected
                    .and_then(|ix| releases.get(ix))
                    .map(|r| (r.name.clone(), r.namespace.clone()));
                self.truncated = truncated;
                self.releases = Load::Ready(releases);
                self.load_pane(cx);
            }
            Ok(_) => self.releases = Load::Failed(HelmError::BadOutput),
            Err(error) => self.releases = Load::Failed(error),
        }
        cx.notify();
    }

    fn selected_or_last(&self) -> Option<(&str, &str)> {
        self.last_selected
            .as_ref()
            .map(|(name, namespace)| (name.as_str(), namespace.as_str()))
    }

    fn load_pane(&mut self, cx: &mut Context<Self>) {
        self.pane_line = 0;
        self.pane_scroll.scroll_to_item(0, ScrollStrategy::Top);
        let Some(Ok(pin)) = self.pin.clone() else {
            self.pane_task = None;
            return;
        };
        let Some(release) = self.selected_release() else {
            self.pane_task = None;
            return;
        };
        let reference = match ReleaseRef::new(&release.name, &release.namespace) {
            Ok(reference) => reference,
            Err(error) => {
                self.pane_task = None;
                self.content = Load::Failed(error);
                cx.notify();
                return;
            }
        };
        let pane = self.pane;
        self.content = Load::Loading;
        let read = self
            .core
            .helm_read(&self.cluster, &pin, pane.query(reference));
        self.pane_task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            let lines = match result {
                Ok(output) => {
                    cx.background_spawn(async move { Ok(pane_lines(pane, output)) })
                        .await
                }
                Err(error) => Err(error),
            };
            let _ = this.update(cx, |view, cx| {
                view.content = match lines {
                    Ok(lines) => Load::Ready(lines),
                    Err(error) => Load::Failed(error),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Load::Ready(releases) = &self.releases else {
            return;
        };
        let Some(release) = releases.get(ix) else {
            return;
        };
        self.last_selected = Some((release.name.clone(), release.namespace.clone()));
        if self.selected != Some(ix) {
            self.selected = Some(ix);
            self.list_scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            self.load_pane(cx);
        }
    }

    /// Selects the release named, if the list holds it.
    pub(in crate::ui) fn select_named(
        &mut self,
        namespace: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        let Load::Ready(releases) = &self.releases else {
            return;
        };
        if let Some(ix) = releases
            .iter()
            .position(|r| r.name == name && r.namespace == namespace)
        {
            self.select(ix, cx);
        }
    }

    pub(in crate::ui) fn set_pane(&mut self, pane: Pane, cx: &mut Context<Self>) {
        if self.pane != pane {
            self.pane = pane;
            self.load_pane(cx);
        }
    }

    fn release_count(&self) -> usize {
        match &self.releases {
            Load::Ready(releases) => releases.len(),
            _ => 0,
        }
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.release_count();
        if count == 0 {
            return;
        }
        let at = self.selected.unwrap_or(0) as isize;
        self.select((at + by).clamp(0, count as isize - 1) as usize, cx);
    }

    fn page(&mut self, down: bool, cx: &mut Context<Self>) {
        let Load::Ready(lines) = &self.content else {
            return;
        };
        let last = lines.len().saturating_sub(1);
        self.pane_line = if down {
            (self.pane_line + PAGE).min(last)
        } else {
            self.pane_line.saturating_sub(PAGE)
        };
        self.pane_scroll
            .scroll_to_item(self.pane_line, ScrollStrategy::Top);
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keys = &event.keystroke;
        if keys.modifiers.modified() {
            return;
        }
        match keys.key.as_str() {
            "left" => self.set_pane(self.pane.moved(-1), cx),
            "right" => self.set_pane(self.pane.moved(1), cx),
            "pagedown" => self.page(true, cx),
            "pageup" => self.page(false, cx),
            "r" => self.refresh(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Load::Ready(releases) = &self.releases else {
            return Vec::new();
        };
        let releases = releases.clone();
        let selected = self.selected;
        let theme = cx.theme().clone();
        range
            .filter_map(|ix| {
                let release = releases.get(ix)?;
                let is_selected = selected == Some(ix);
                Some(
                    h_flex()
                        .id(("helm-release", ix))
                        .aria_label(release_label(release))
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .px_2()
                        .gap_2()
                        .text_xs()
                        .cursor_pointer()
                        .when(is_selected, |row| row.bg(theme.accent))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(release.name.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(release.namespace.clone()),
                        )
                        .on_click(registry::on_click(SelectHelmRelease {
                            namespace: release.namespace.clone(),
                            name: release.name.clone(),
                        }))
                        .test_support()
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_lines(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Load::Ready(lines) = &self.content else {
            return Vec::new();
        };
        let theme = cx.theme();
        range
            .map(|ix| {
                div()
                    .id(("helm-line", ix))
                    .h(px(LINE_HEIGHT))
                    .px_2()
                    .whitespace_nowrap()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .child(SharedString::from(lines.line(ix).to_owned()))
                    .into_any_element()
            })
            .collect()
    }

    fn message(&self, id: &'static str, text: String, cx: &App) -> AnyElement {
        div()
            .id(id)
            .p_3()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .aria_label(text.clone())
            .child(text)
            .test_support()
            .into_any_element()
    }

    fn render_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.content {
            _ if self.selected.is_none() => div().into_any_element(),
            Load::Loading => self
                .message("helm-pane-loading", "Loading…".to_owned(), cx)
                .into_any_element(),
            Load::Failed(error) => self
                .message("helm-pane-error", error.to_string(), cx)
                .into_any_element(),
            Load::Ready(lines) => {
                uniform_list("helm-lines", lines.len(), cx.processor(Self::render_lines))
                    .track_scroll(&self.pane_scroll)
                    .size_full()
                    .into_any_element()
            }
        }
    }
}

impl DockItem for HelmView {
    fn tab_key(&self) -> TabKey {
        TabKey::Helm(self.cluster.clone())
    }

    fn tab_title(&self) -> SharedString {
        "Helm releases".into()
    }

    fn tab_tooltip(&self) -> SharedString {
        format!("Helm releases on {}, read-only.", self.cluster).into()
    }

    fn tab_status(&self) -> TabStatus {
        match self.releases {
            Load::Loading => TabStatus::Starting,
            Load::Ready(_) => TabStatus::Live,
            Load::Failed(_) => TabStatus::Failed,
        }
    }

    fn tab_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn can_restart(&self) -> bool {
        false
    }
}

impl Render for HelmView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let count = self.release_count();
        let summary = match (&self.releases, self.truncated) {
            (Load::Loading, _) => "Loading…".to_owned(),
            (Load::Failed(_), _) => "Helm releases".to_owned(),
            (Load::Ready(_), true) => format!("First {MAX_ROWS} releases"),
            (Load::Ready(_), false) if count == 1 => "1 release".to_owned(),
            (Load::Ready(_), false) => format!("{count} releases"),
        };
        let body = match &self.releases {
            Load::Loading => self
                .message("helm-loading", "Loading…".to_owned(), cx)
                .into_any_element(),
            Load::Failed(error) => self
                .message("helm-error", error.to_string(), cx)
                .into_any_element(),
            Load::Ready(_) if count == 0 => self
                .message("helm-empty", EMPTY.to_owned(), cx)
                .into_any_element(),
            Load::Ready(_) => h_flex()
                .flex_1()
                .min_h_0()
                .items_start()
                .size_full()
                .child(
                    div()
                        .w(px(260.))
                        .flex_none()
                        .h_full()
                        .border_r_1()
                        .border_color(theme.border)
                        .child(
                            uniform_list("helm-releases", count, cx.processor(Self::render_rows))
                                .track_scroll(&self.list_scroll)
                                .size_full(),
                        ),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(
                            h_flex()
                                .gap_1()
                                .px_2()
                                .py_1()
                                .border_b_1()
                                .border_color(theme.border)
                                .children(Pane::ALL.map(|pane| {
                                    let button = Button::new(pane.id())
                                        .ghost()
                                        .xsmall()
                                        .label(pane.label())
                                        .selected(self.pane == pane);
                                    match pane {
                                        Pane::Details => {
                                            button.on_click(registry::on_click(ShowHelmDetails))
                                        }
                                        Pane::History => {
                                            button.on_click(registry::on_click(ShowHelmHistory))
                                        }
                                        Pane::Values => {
                                            button.on_click(registry::on_click(ShowHelmValues))
                                        }
                                        Pane::Manifest => {
                                            button.on_click(registry::on_click(ShowHelmManifest))
                                        }
                                    }
                                })),
                        )
                        .child(div().flex_1().min_h_0().child(self.render_pane(cx)))
                        .when(matches!(self.pane, Pane::Values | Pane::Manifest), |el| {
                            el.child(
                                div()
                                    .id("helm-mask-note")
                                    .px_2()
                                    .py_1()
                                    .border_t_1()
                                    .border_color(theme.border)
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .aria_label(MASK_NOTE)
                                    .child(MASK_NOTE)
                                    .test_support(),
                            )
                        }),
                )
                .into_any_element(),
        };
        v_flex()
            .id("helm-releases-view")
            .aria_label("Helm releases")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|view, _: &SelectNext, _, cx| view.step(1, cx)))
            .on_action(cx.listener(|view, _: &SelectPrev, _, cx| view.step(-1, cx)))
            .on_action(cx.listener(|view, _: &SelectFirst, _, cx| view.select(0, cx)))
            .on_action(cx.listener(|view, _: &SelectLast, _, cx| {
                let last = view.release_count().saturating_sub(1);
                view.select(last, cx);
            }))
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(
                        div()
                            .id("helm-count")
                            .flex_1()
                            .text_color(theme.muted_foreground)
                            .aria_label(summary.clone())
                            .child(summary)
                            .test_support(),
                    )
                    .child(
                        Button::new("helm-refresh")
                            .ghost()
                            .xsmall()
                            .label("Refresh")
                            .tooltip("Read the releases again (R)")
                            .on_click(registry::on_click(RefreshHelmReleases)),
                    ),
            )
            .when_some(self.binary_warning.clone(), |el, warning| {
                el.child(
                    div()
                        .id("helm-binary-warning")
                        .px_3()
                        .py_1()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_xs()
                        .text_color(theme.warning)
                        .aria_label(warning.clone())
                        .child(warning)
                        .test_support(),
                )
            })
            .child(body)
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use flint_core::helm::{HelmDetails, HelmOutput, HelmRelease, HelmRevision};

    use super::{
        Lines, NO_VALUES, Pane, details_text, history_text, keep_selection, pane_lines,
        release_label,
    };

    fn release(name: &str, namespace: &str) -> HelmRelease {
        HelmRelease {
            name: name.into(),
            namespace: namespace.into(),
            revision: 2,
            updated: "now".into(),
            status: "deployed".into(),
            chart: "web-1.0".into(),
            app_version: "1".into(),
        }
    }

    // The action opens one tab per cluster, and a cluster helm may not run on says why.
    #[gpui_kit::test]
    async fn the_action_opens_the_tab_and_says_why_it_cannot_read(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use crate::ui::registry::ActionError;
        use crate::ui::support_tests::open_workspace;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        assert!(matches!(
            t.dispatch(
                cx,
                "workspace::ShowHelmReleases",
                json!({"cluster": "nope"})
            ),
            Err(ActionError::Rejected { .. })
        ));
        for _ in 0..2 {
            t.dispatch(
                cx,
                "workspace::ShowHelmReleases",
                json!({"cluster": "kind-beta"}),
            )
            .unwrap();
            t.wait_for_element(cx, "helm-releases-view").await;
        }
        t.wait_for_element(cx, "helm-error").await;
        assert!(
            t.label(cx, "helm-error")
                .is_some_and(|text| !text.is_empty())
        );
    }

    // A tab that could not run at first stays on the definition it opened on: a refresh after
    // the context moved to another server is refused and starts nothing.
    #[gpui_kit::test]
    async fn a_refresh_does_not_adopt_a_new_server(cx: &mut gpui_kit::TestAppContext) {
        use crate::ui::support_tests::{WAIT, kubeconfig_yaml, open_workspace_with};
        use flint_core::ConnectionState;
        use flint_core::helm::HelmError;
        use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Options};
        use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt as _;

        let server = FakeApiServer::start_blocking(Options::default()).unwrap();
        let scripts = tempfile::tempdir().unwrap();
        let ran = scripts.path().join("ran");
        let helm = scripts.path().join("fake-helm");
        std::fs::write(
            &helm,
            format!("#!/bin/sh\nprintf x >> '{}'\nprintf '[]'\n", ran.display()),
        )
        .unwrap();
        std::fs::set_permissions(&helm, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.toml"),
            format!("helm = \"{}\"\n", helm.display()),
        )
        .unwrap();
        let file = dir.path().join("kubeconfig");
        let path = file.clone();
        let t = open_workspace_with(cx, dir.path(), move |_, servers| {
            std::fs::write(&path, kubeconfig_yaml(&[(CONTEXT, &servers.refused)])).unwrap();
            vec![path]
        })
        .await;
        t.dispatch(
            cx,
            "workspace::ShowHelmReleases",
            json!({ "cluster": CONTEXT }),
        )
        .unwrap();
        t.wait_for_element(cx, "helm-error").await;

        server.write_kubeconfig(&file, Some("demo")).unwrap();
        let core = t.model.read_with(cx, |model, _| model.core().clone());
        core.reload_kubeconfig().await.unwrap();
        t.activate(cx, CONTEXT).await;
        t.wait_for_state(cx, CONTEXT, |s| matches!(s, ConnectionState::Connected(_)))
            .await;

        t.dispatch(cx, "workspace::RefreshHelmReleases", json!({}))
            .unwrap();
        let changed = HelmError::Changed.to_string();
        cx.wait_for(t.window, WAIT, move |window, _| {
            window
                .try_find("helm-error")
                .is_some_and(|e| e.label() == Some(changed.as_str()))
        })
        .await;
        assert!(!ran.exists(), "helm never started on the new server");
    }

    // The warning about the binary is on screen with an accessible name, and blocks nothing.
    #[gpui_kit::test]
    async fn the_view_shows_the_binary_warning(cx: &mut gpui_kit::TestAppContext) {
        use crate::ui::support_tests::open_workspace;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        t.dispatch(
            cx,
            "workspace::ShowHelmReleases",
            json!({"cluster": "kind-beta"}),
        )
        .unwrap();
        t.wait_for_element(cx, "helm-releases-view").await;
        let view = t
            .workspace
            .read_with(cx, |ws, cx| ws.dock_view().read(cx).active_helm().cloned())
            .unwrap();
        view.update(cx, |view, cx| {
            view.binary_warning = Some("Other administrators could replace /x/helm".into());
            cx.notify();
        });
        t.wait_for_element(cx, "helm-binary-warning").await;
        assert_eq!(
            t.label(cx, "helm-binary-warning").as_deref(),
            Some("Other administrators could replace /x/helm")
        );
        t.wait_for_element(cx, "helm-refresh").await;
    }

    #[test]
    fn lines_borrow_from_one_text() {
        let lines = Lines::new("a: 1\n\nb: 2\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.line(0), "a: 1");
        assert_eq!(lines.line(1), "");
        assert_eq!(lines.line(2), "b: 2");
        assert_eq!(lines.line(3), "");
        assert_eq!(Lines::new("").len(), 1);
    }

    #[test]
    fn panes_wrap_with_the_arrow_keys() {
        assert_eq!(Pane::Details.moved(-1), Pane::Manifest);
        assert_eq!(Pane::Manifest.moved(1), Pane::Details);
        assert_eq!(Pane::Values.moved(1), Pane::Manifest);
    }

    #[test]
    fn a_refresh_keeps_the_selected_release() {
        let releases = [release("a", "x"), release("web", "y"), release("web", "z")];
        assert_eq!(keep_selection(&releases, Some(("web", "z"))), Some(2));
        assert_eq!(keep_selection(&releases, Some(("gone", "z"))), Some(0));
        assert_eq!(keep_selection(&[], Some(("web", "z"))), None);
    }

    #[test]
    fn cluster_text_cannot_break_a_row() {
        let label = release_label(&release("we\nb", "ns\u{1b}[2J"));
        assert!(
            !label.contains('\n') && !label.contains('\u{1b}'),
            "{label}"
        );
        let text = history_text(&[HelmRevision {
            revision: 3,
            updated: "t".into(),
            status: "failed".into(),
            chart: "c".into(),
            app_version: "1".into(),
            description: "line\nbreak".into(),
        }]);
        assert_eq!(text.lines().count(), 1, "{text}");
    }

    #[test]
    fn empty_values_say_so() {
        let lines = pane_lines(Pane::Values, HelmOutput::Text("".into()));
        assert_eq!(lines.line(0), NO_VALUES);
    }

    #[test]
    fn details_read_as_labelled_lines() {
        let text = details_text(&HelmDetails {
            name: "web".into(),
            revision: 4,
            chart: "web".into(),
            chart_version: "1.2.3".into(),
            ..HelmDetails::default()
        });
        assert!(text.contains("Revision: 4\n"), "{text}");
        assert!(text.contains("Chart: web-1.2.3\n"), "{text}");
    }
}
