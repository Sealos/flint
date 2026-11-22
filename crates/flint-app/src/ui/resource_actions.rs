//! The resource actions of a selected row (UP-53, UP-54): delete, scale, rollout restart, cordon,
//! uncordon and drain.
//!
//! Each is a registered action. It asks what it needs first (a replica count, a drain's options),
//! has the core review it ([`CoreHandle::review_write`](flint_core::CoreHandle::review_write)),
//! and goes through the shared confirmation ([`write_confirm`]), which runs what was reviewed. A
//! target carries the object's uid and no `resourceVersion`, so a status update is no conflict.
//!
//! A write is never run again by itself: when the object changed, a new review opens, saying so,
//! and it takes a new confirmation.
//!
//! One write runs per object, across windows ([`RunningWrites`]). Two confirmations can be open
//! for one object, so the object is claimed as a confirmation runs, right before the write, and
//! released when the write ends, whatever became of the window it started in.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use flint_core::KindId;
use flint_core::display::{ClusterNames, cluster_name_key, single_line};
use flint_core::redact::first_line;
use flint_core::write::{
    BlockedReason, DrainReport, PodName, WriteError, WriteOutcome, WriteProposal, WriteReview,
    WriteTarget,
};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, Theme, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::confirm::{self, DialogText};
use super::detail::access::Gated;
use super::detail::{DetailView, Opening, Subject, protected_reason, refused};
use super::notice::{notice, notice_with};
use super::registry::{
    self, Bind, CordonNode, DeleteObject, DrainNode, DrainOption, OnView, RestartObject,
    ScaleObject, ShowDrainReport, ToggleDrainOption, UncordonNode,
};
use super::row_menu::Availability;
use super::workspace::Workspace;
use super::write_confirm;

/// The most replicas a workload can have: the API's `int32`.
const MAX_REPLICAS: u32 = i32::MAX as u32;

/// How many pods a line of a drain's report names; the rest are counted.
const REPORT_NAMES: usize = 5;

/// How many reasons a drain's report lists for blocked pods; the rest are counted.
const REPORT_REASONS: usize = 6;

/// The longest reason from the server a drain's report quotes.
const REPORT_REASON_LEN: usize = 120;

/// How many running drains the strip names; the rest are counted.
const STRIP_NAMES: usize = 3;

/// A resource action, as the row menu and the guards know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum ResourceAction {
    Delete,
    Scale,
    RolloutRestart,
    Cordon,
    Uncordon,
    Drain,
}

impl ResourceAction {
    /// The menu's entry. Each opens a dialog first.
    pub(in crate::ui) fn label(self) -> &'static str {
        match self {
            ResourceAction::Delete => "Delete…",
            ResourceAction::Scale => "Scale…",
            ResourceAction::RolloutRestart => "Rollout Restart…",
            ResourceAction::Cordon => "Cordon…",
            ResourceAction::Uncordon => "Uncordon…",
            ResourceAction::Drain => "Drain…",
        }
    }

    /// The registered action.
    pub(in crate::ui) fn boxed(self) -> Box<dyn Action> {
        match self {
            ResourceAction::Delete => Box::new(DeleteObject),
            ResourceAction::Scale => Box::new(ScaleObject),
            ResourceAction::RolloutRestart => Box::new(RestartObject),
            ResourceAction::Cordon => Box::new(CordonNode),
            ResourceAction::Uncordon => Box::new(UncordonNode),
            ResourceAction::Drain => Box::new(DrainNode),
        }
    }

    /// Which resource action `action` is, if it is one.
    pub(in crate::ui) fn of(action: &dyn Action) -> Option<Self> {
        let any = action.as_any();
        [
            (any.is::<DeleteObject>(), ResourceAction::Delete),
            (any.is::<ScaleObject>(), ResourceAction::Scale),
            (any.is::<RestartObject>(), ResourceAction::RolloutRestart),
            (any.is::<CordonNode>(), ResourceAction::Cordon),
            (any.is::<UncordonNode>(), ResourceAction::Uncordon),
            (any.is::<DrainNode>(), ResourceAction::Drain),
        ]
        .into_iter()
        .find_map(|(is, resource)| is.then_some(resource))
    }

    /// What is done to an object while this runs ("web is being scaled").
    fn doing(self) -> &'static str {
        match self {
            ResourceAction::Delete => "deleted",
            ResourceAction::Scale => "scaled",
            ResourceAction::RolloutRestart => "restarted",
            ResourceAction::Cordon => "cordoned",
            ResourceAction::Uncordon => "uncordoned",
            ResourceAction::Drain => "drained",
        }
    }

    /// What could not be done, as a refusal's sentence starts.
    pub(in crate::ui) fn cant(self) -> &'static str {
        match self {
            ResourceAction::Delete => "Can't delete it",
            ResourceAction::Scale => "Can't scale it",
            ResourceAction::RolloutRestart => "Can't restart it",
            ResourceAction::Cordon => "Can't cordon the node",
            ResourceAction::Uncordon => "Can't uncordon the node",
            ResourceAction::Drain => "Can't drain the node",
        }
    }
}

/// What runs on an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum RunningWrite {
    Action(ResourceAction),
    YamlEdit,
}

impl From<ResourceAction> for RunningWrite {
    fn from(action: ResourceAction) -> Self {
        RunningWrite::Action(action)
    }
}

impl RunningWrite {
    fn doing(self) -> &'static str {
        match self {
            RunningWrite::Action(action) => action.doing(),
            RunningWrite::YamlEdit => "edited",
        }
    }
}

/// The object a write runs on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::ui) struct WriteKey {
    cluster: String,
    kind: KindId,
    namespace: Option<String>,
    name: String,
}

impl WriteKey {
    pub(in crate::ui) fn new(
        cluster: &str,
        kind: &KindId,
        namespace: Option<&str>,
        name: &str,
    ) -> Self {
        WriteKey {
            cluster: cluster.to_owned(),
            kind: kind.clone(),
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
        }
    }

    pub(in crate::ui) fn of(target: &WriteTarget) -> Self {
        Self::new(
            &target.cluster,
            &target.kind,
            target.namespace.as_deref(),
            &target.name,
        )
    }

    /// Why no other action can run on this object while `running` does.
    fn busy(&self, running: RunningWrite) -> String {
        format!("{} is being {}", self.name, running.doing())
    }
}

/// A drain that ended, until its report is viewed or a newer one replaces it.
struct FinishedDrain {
    node: String,
    report: DrainReport,
}

/// The writes running now, in every window, and the latest drain's result.
#[derive(Default)]
pub(in crate::ui) struct RunningWrites {
    running: BTreeMap<WriteKey, RunningWrite>,
    finished: Option<FinishedDrain>,
}

impl Global for RunningWrites {}

/// What runs on `key` now, if anything.
pub(in crate::ui) fn running_on(key: &WriteKey, cx: &App) -> Option<RunningWrite> {
    cx.try_global::<RunningWrites>()?.running.get(key).copied()
}

/// Claims `key` for `action`. `Err` says what runs on it already.
pub(in crate::ui) fn claim(
    key: &WriteKey,
    write: impl Into<RunningWrite>,
    cx: &mut App,
) -> Result<(), String> {
    if let Some(running) = running_on(key, cx) {
        return Err(key.busy(running));
    }
    cx.update_default_global::<RunningWrites, _>(|writes, _| {
        writes.running.insert(key.clone(), write.into());
    });
    Ok(())
}

/// Ends the claim on `key`.
pub(in crate::ui) fn release(key: &WriteKey, cx: &mut App) {
    cx.update_default_global::<RunningWrites, _>(|writes, _| {
        writes.running.remove(key);
    });
}

fn is_scalable(kind: &KindId) -> bool {
    matches!(
        kind.as_str(),
        "deployments.apps" | "statefulsets.apps" | "replicasets.apps" | "replicationcontrollers"
    )
}

fn is_restartable(kind: &KindId) -> bool {
    matches!(
        kind.as_str(),
        "deployments.apps" | "daemonsets.apps" | "statefulsets.apps"
    )
}

/// Whether rows of `kind` have a resource action, whatever state the object is in. A kind that
/// can be deleted has one (`deletable`, from discovery's verbs).
pub(in crate::ui) fn kind_has_actions(kind: &KindId, deletable: bool) -> bool {
    deletable || is_scalable(kind) || is_restartable(kind) || kind.as_str() == "nodes"
}

/// The actions a row of `kind` offers, in menu order, the destructive one last. A node offers
/// Uncordon when `cordoned`, else Cordon (also while that is not known yet, disabled).
pub(in crate::ui) fn offered_for(
    kind: &KindId,
    deletable: bool,
    cordoned: Option<bool>,
) -> Vec<ResourceAction> {
    let mut actions = Vec::new();
    if is_scalable(kind) {
        actions.push(ResourceAction::Scale);
    }
    if is_restartable(kind) {
        actions.push(ResourceAction::RolloutRestart);
    }
    if kind.as_str() == "nodes" {
        actions.push(if cordoned == Some(true) {
            ResourceAction::Uncordon
        } else {
            ResourceAction::Cordon
        });
        actions.push(ResourceAction::Drain);
    }
    if deletable {
        actions.push(ResourceAction::Delete);
    }
    actions
}

/// The actions the selected row offers now.
pub(in crate::ui) fn offered(ws: &Workspace, detail: &DetailView, cx: &App) -> Vec<ResourceAction> {
    let object = detail.object();
    let deletable = ws.kind_deletable(&object.cluster, &object.kind, cx);
    let cordoned = (object.kind.as_str() == "nodes")
        .then(|| detail.node_cordoned().ok())
        .flatten();
    offered_for(&object.kind, deletable, cordoned)
}

/// Whether `action` can run on the selected row now, and why not: what the menu shows, the
/// guards check, and the handler checks again.
pub(in crate::ui) fn available(ws: &Workspace, action: ResourceAction, cx: &App) -> Availability {
    check(ws, action, cx).map_err(|refusal| refusal.why)
}

/// Why a resource action cannot run now.
pub(in crate::ui) struct Refusal {
    pub why: String,
    /// The cluster's protection is what refused it: the menu shortens this one, by this
    /// decision and never by comparing the text.
    pub protected: bool,
}

impl From<String> for Refusal {
    fn from(why: String) -> Self {
        Refusal {
            why,
            protected: false,
        }
    }
}

/// [`available`], saying whether protection is the reason.
pub(in crate::ui) fn check(
    ws: &Workspace,
    action: ResourceAction,
    cx: &App,
) -> Result<(), Refusal> {
    let Some(detail) = ws.detail_view() else {
        return Err("no object is selected".to_owned().into());
    };
    let detail = detail.read(cx);
    let object = detail.object();
    let key = WriteKey::new(
        &object.cluster,
        &object.kind,
        object.namespace.as_deref(),
        &object.name,
    );
    if let Some(running) = running_on(&key, cx) {
        return Err(key.busy(running).into());
    }
    if !offered(ws, detail, cx).contains(&action) {
        return Err(not_offered(action, detail).into());
    }
    if let Some(why) = ws.sessions_blocked(&object.cluster, cx) {
        return Err(why.into());
    }
    if ws.is_protected(&object.cluster, cx) {
        return Err(Refusal {
            why: protected_reason(&ws.cluster_names(cx).exact(&object.cluster)),
            protected: true,
        });
    }
    if detail.is_gone() {
        return Err(format!("{} was deleted", object.name).into());
    }
    if action == ResourceAction::Cordon {
        // Offered while the node is not known: it says what it waits for.
        detail.node_cordoned()?;
    }
    match detail.forbidden(Gated::Write(action)) {
        Some(why) => Err(why.into()),
        None => Ok(()),
    }
}

/// Why the row does not offer `action`.
fn not_offered(action: ResourceAction, detail: &DetailView) -> String {
    let name = &detail.object().name;
    match (action, detail.node_cordoned()) {
        (ResourceAction::Cordon, Ok(true)) => format!("{name} is cordoned already"),
        (ResourceAction::Uncordon, Ok(false)) => format!("{name} is not cordoned"),
        (ResourceAction::Uncordon, Err(why)) => why,
        _ => format!("{} does not apply to this kind", action.label()),
    }
}

/// The change to review for the selected row: its uid (a row of an API that sends none is keyed
/// by its name, which is no uid), and no `resourceVersion`.
fn target_of(ws: &Workspace, cx: &App) -> Option<WriteTarget> {
    let detail = ws.detail_view()?;
    let object = detail.read(cx).object().clone();
    let server = ws
        .cluster_summary(&object.cluster, cx)
        .and_then(|c| c.info.server.clone());
    let uid = Some(object.uid.to_string()).filter(|u| !u.contains('/') && *u != object.name);
    Some(WriteTarget {
        cluster: object.cluster,
        server,
        kind: object.kind,
        namespace: object.namespace,
        name: object.name,
        uid,
        resource_version: None,
    })
}

/// A change on its way through review and confirmation.
#[derive(Clone)]
struct Request {
    action: ResourceAction,
    target: WriteTarget,
    proposal: WriteProposal,
}

/// Runs `action` on the selected row: its dialog if it has one, then the review.
pub(in crate::ui) fn request(
    ws: &mut Workspace,
    action: ResourceAction,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let target = match available(ws, action, cx).map(|()| target_of(ws, cx)) {
        Ok(Some(target)) => target,
        Ok(None) => return,
        Err(why) => {
            ws.refuse(refused(Opening::Write(action), &why), cx);
            return;
        }
    };
    let proposal = match action {
        ResourceAction::Scale => return open_scale(ws, target, window, cx),
        ResourceAction::Drain => return open_drain(target, window, cx),
        ResourceAction::Delete => WriteProposal::Delete,
        ResourceAction::RolloutRestart => WriteProposal::RolloutRestart,
        ResourceAction::Cordon => WriteProposal::Cordon,
        ResourceAction::Uncordon => WriteProposal::Uncordon,
    };
    let request = Request {
        action,
        target,
        proposal,
    };
    start_review(ws, request, None, window, cx);
}

/// Has the core review `request`; the confirmation opens with the result, first saying `note`.
fn start_review(
    ws: &mut Workspace,
    request: Request,
    note: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let review = ws
        .core(cx)
        .review_write(request.target.clone(), request.proposal.clone());
    let handle = window.window_handle();
    let task = cx.spawn(async move |this, cx| {
        let result = review.await;
        let _ = cx.update_window(handle, |_, window, cx| {
            let _ = this.update(cx, |ws, cx| reviewed(ws, request, note, result, window, cx));
        });
    });
    ws.set_write_review(task);
}

fn reviewed(
    ws: &mut Workspace,
    request: Request,
    note: Option<String>,
    result: Result<WriteReview, WriteError>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match result {
        Ok(review) => confirm_review(ws, request, review, note, window, cx),
        // Another object has the name now: this review is of that one, and says so.
        Err(WriteError::Replaced) if request.target.uid.is_some() => {
            let note = replaced_note(&request.target.name);
            let mut request = request;
            request.target.uid = None;
            start_review(ws, request, Some(note), window, cx);
        }
        Err(error) => ws.refuse(
            refused(Opening::Write(request.action), &error.to_string()),
            cx,
        ),
    }
}

fn confirm_review(
    ws: &mut Workspace,
    request: Request,
    review: WriteReview,
    note: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let core = ws.core(cx).clone();
    let names = ws.cluster_names(cx);
    let this = cx.weak_entity();
    let handle = window.window_handle();
    let key = WriteKey::of(&request.target);
    let begin = {
        let (key, this, action) = (key.clone(), this.clone(), request.action);
        move |cx: &mut App| match claim(&key, action, cx) {
            Ok(()) => true,
            Err(why) => {
                let _ = this.update(cx, |ws, cx| {
                    ws.refuse(refused(Opening::Write(action), &why), cx)
                });
                false
            }
        }
    };
    write_confirm::open_noted(
        window,
        cx,
        core,
        review,
        &names,
        note.as_deref(),
        begin,
        move |result, cx| {
            // First, and outside the window: a closed one must not keep the object claimed.
            release(&key, cx);
            if let Ok(WriteOutcome {
                drain: Some(report),
                ..
            }) = &result
            {
                let finished = FinishedDrain {
                    node: request.target.name.clone(),
                    report: report.clone(),
                };
                cx.update_default_global::<RunningWrites, _>(|writes, _| {
                    writes.finished = Some(finished);
                });
            }
            let request = request.clone();
            let this = this.clone();
            let _ = cx.update_window(handle, |_, window, cx| {
                let _ = this.update(cx, |ws, cx| written(ws, request, result, window, cx));
            });
        },
    );
}

/// What follows a write that ran, in the window it started in. A drain's result is in the strip.
fn written(
    ws: &mut Workspace,
    request: Request,
    result: Result<WriteOutcome, WriteError>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match result {
        Ok(_) => {}
        // Never run again by itself: a new review, and a new confirmation.
        Err(WriteError::Conflict | WriteError::Replaced) => {
            let note = changed_note(&request.target.name);
            start_review(ws, request, Some(note), window, cx);
        }
        Err(error) => ws.refuse(
            refused(Opening::Write(request.action), &error.to_string()),
            cx,
        ),
    }
}

/// Whether a drain's report is waiting to be viewed.
pub(in crate::ui) fn has_drain_result(cx: &App) -> bool {
    cx.try_global::<RunningWrites>()
        .is_some_and(|w| w.finished.is_some())
}

/// Opens the latest drain's report; viewing it takes it off the strip.
pub(in crate::ui) fn show_drain_report(window: &mut Window, cx: &mut App) {
    let finished = cx.update_default_global::<RunningWrites, _>(|writes, _| writes.finished.take());
    if let Some(finished) = finished {
        let text = drain_report(&finished.node, &finished.report);
        confirm::open(window, cx, text, |_, _| {});
    }
}

/// The strip's line for the drains running: each is named, up to [`STRIP_NAMES`], then counted.
/// Across clusters a name says which, by [`strip_cluster_labels`]. `names` is called only then:
/// building the labels costs too much for a strip that does not show them.
fn draining_text(keys: &[&WriteKey], names: impl FnOnce() -> ClusterNames) -> Option<String> {
    let across = keys.windows(2).any(|w| w[0].cluster != w[1].cluster);
    let labels = across.then(|| strip_cluster_labels(keys, &names()));
    let name = |key: &&WriteKey| match &labels {
        Some(labels) => format!("{}/{}", labels[key.cluster.as_str()], key.name),
        None => key.name.clone(),
    };
    match keys {
        [] => None,
        [one] => Some(format!("Draining {}…", name(one))),
        many => {
            let names: Vec<String> = many.iter().take(STRIP_NAMES).map(name).collect();
            let more = match many.len().saturating_sub(STRIP_NAMES) {
                0 => String::new(),
                more => format!(" and {more} more"),
            };
            Some(format!(
                "Draining {} nodes: {}{more}…",
                many.len(),
                names.join(", ")
            ))
        }
    }
}

/// What the strip calls each cluster of `keys`: its exact label, or its whole context on one line
/// for all of them when two labels read the same. A context that left kubeconfig is named whole,
/// and that can read like the short label of one still there.
fn strip_cluster_labels<'a>(
    keys: &[&'a WriteKey],
    names: &ClusterNames,
) -> HashMap<&'a str, String> {
    let clusters: BTreeSet<&str> = keys.iter().map(|key| key.cluster.as_str()).collect();
    let read: BTreeSet<String> = clusters
        .iter()
        .map(|cluster| cluster_name_key(&names.exact(cluster)))
        .collect();
    let distinct = read.len() == clusters.len();
    clusters
        .into_iter()
        .map(|cluster| {
            let label = match distinct {
                true => names.exact(cluster),
                false => single_line(cluster),
            };
            (cluster, label)
        })
        .collect()
}

/// What the strip says of a drain that ended.
fn finished_text(finished: &FinishedDrain) -> String {
    let how = match finished.report.stopped {
        Some(_) => "stopped",
        None => "finished",
    };
    format!("Drain of {} {how}", finished.node)
}

/// The workspace's strips for drains: those running, and the latest result with its button.
pub(in crate::ui) fn drain_strips(
    names: impl FnOnce() -> ClusterNames,
    theme: &Theme,
    cx: &App,
) -> Vec<AnyElement> {
    let Some(writes) = cx.try_global::<RunningWrites>() else {
        return Vec::new();
    };
    let mut strips = Vec::new();
    let draining: Vec<&WriteKey> = writes
        .running
        .iter()
        .filter(|(_, write)| **write == RunningWrite::Action(ResourceAction::Drain))
        .map(|(key, _)| key)
        .collect();
    if let Some(text) = draining_text(&draining, names) {
        strips.push(
            notice("drain-status", text.clone(), theme.info)
                .aria_label(text)
                .test_support()
                .into_any_element(),
        );
    }
    if let Some(finished) = &writes.finished {
        let text = finished_text(finished);
        let color = match finished.report.stopped {
            Some(_) => theme.warning,
            None => theme.info,
        };
        let view = Button::new("drain-view-result")
            .label("View result")
            .xsmall()
            .on_click(registry::on_click(ShowDrainReport));
        strips.push(
            notice_with("drain-result", text.clone(), color, view)
                .aria_label(text)
                .test_support()
                .into_any_element(),
        );
    }
    strips
}

/// What a review opened again says first.
fn changed_note(name: &str) -> String {
    format!("{name} changed since the last review. Nothing was applied: check it again.")
}

/// What a review of another object of the same name says first.
fn replaced_note(name: &str) -> String {
    format!("{name} was deleted and created again. This is a review of the new object.")
}

/// The replica count `typed`, or why it is not one.
fn parse_replicas(typed: &str) -> Result<u32, String> {
    let typed = typed.trim();
    let wrong = || "Enter a whole number of replicas, 0 or more.".to_owned();
    if typed.is_empty() || !typed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(wrong());
    }
    let too_many = || format!("A workload has at most {MAX_REPLICAS} replicas.");
    match typed.parse::<u32>() {
        Ok(count) if count <= MAX_REPLICAS => Ok(count),
        // All digits: it can only be too large.
        _ => Err(too_many()),
    }
}

/// Which object a dialog before the review is about.
fn object_lines(target: &WriteTarget) -> String {
    write_confirm::target_lines(target).join("\n")
}

/// The object's name as the title of a dialog before the review quotes it.
fn pre_review_title_name(target: &WriteTarget) -> String {
    confirm::title_name(&flint_core::display::single_line(&target.name))
}

fn scale_dialog(target: &WriteTarget, current: Option<u64>) -> DialogText {
    let now = current.map_or("not known yet".to_owned(), |n| n.to_string());
    DialogText {
        title: format!("Scale {}", pre_review_title_name(target)),
        text: format!(
            "Replicas now: {now}. The next step shows the change to confirm.\n\n{}",
            object_lines(target)
        ),
        ok: "Continue".into(),
        cancel: "Cancel".into(),
        id: "scale-description",
        default_cancel: false,
    }
}

/// The replica count field under the scale dialog's text.
struct ScaleField {
    input: Entity<InputState>,
    error: Option<String>,
}

impl Render for ScaleField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        v_flex()
            .gap_1()
            .child(Input::new(&self.input).id("scale-input").small())
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("scale-error")
                        .aria_label(error.clone())
                        .text_xs()
                        .text_color(danger)
                        .child(error)
                        .test_support(),
                )
            })
    }
}

fn open_scale(
    ws: &mut Workspace,
    target: WriteTarget,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let current = ws.detail_view().and_then(|d| d.read(cx).replicas());
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder("Replicas")
            .default_value(current.map_or(String::new(), |n| n.to_string()))
    });
    let field = cx.new(|_| ScaleField {
        input: input.clone(),
        error: None,
    });
    let text = scale_dialog(&target, current);
    let this = cx.weak_entity();
    let content = field.clone().into();
    let pinned = write_confirm::target_lines(&target).len();
    confirm::open_pinned_with(window, cx, text, pinned, content, move |window, cx| {
        let typed = field.read(cx).input.read(cx).value().to_string();
        let to = match parse_replicas(&typed) {
            Ok(to) => to,
            Err(error) => {
                field.update(cx, |f, cx| {
                    f.error = Some(error);
                    cx.notify();
                });
                return false;
            }
        };
        let request = Request {
            action: ResourceAction::Scale,
            target: target.clone(),
            proposal: WriteProposal::Scale { to },
        };
        let _ = this.update(cx, |ws, cx| start_review(ws, request, None, window, cx));
        true
    });
    input.update(cx, |input, cx| input.focus(window, cx));
}

fn drain_dialog(target: &WriteTarget) -> DialogText {
    DialogText {
        title: format!("Drain {}", pre_review_title_name(target)),
        text: format!(
            "Cordons the node and evicts its pods. {}\nOther pods the options below do not \
             cover stay on the node and are reported. The next step shows the change to \
             confirm.\n\n{}",
            write_confirm::COMPLETED_PODS,
            object_lines(target)
        ),
        ok: "Continue".into(),
        cancel: "Cancel".into(),
        id: "drain-description",
        default_cancel: false,
    }
}

/// The two options of the drain dialog, both off until chosen. Each checkbox performs a
/// registered action from this view's focus, as the AI scope pickers do.
pub(in crate::ui) struct DrainChoices {
    focus: FocusHandle,
    delete_emptydir_data: bool,
    force: bool,
}

impl DrainChoices {
    fn new(cx: &mut Context<Self>) -> Self {
        DrainChoices {
            focus: cx.focus_handle(),
            delete_emptydir_data: false,
            force: false,
        }
    }

    /// Every action of the dialog and its handler, for [`Bind`].
    pub(in crate::ui) fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleDrainOption>(|this, action, _, cx| {
            match action.option {
                DrainOption::DeleteEmptydirData => {
                    this.delete_emptydir_data = !this.delete_emptydir_data
                }
                DrainOption::Force => this.force = !this.force,
            }
            cx.notify();
        })
    }
}

impl Render for DrainChoices {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let says = |id: &'static str, text: &'static str| {
            div()
                .id(id)
                .aria_label(text)
                .pl_6()
                .text_xs()
                .text_color(muted)
                .child(text)
                .test_support()
        };
        let option = |id: &'static str, label: &'static str, checked, option| {
            Checkbox::new(id)
                .label(label)
                .checked(checked)
                .on_click(registry::on_click_in(
                    self.focus.clone(),
                    ToggleDrainOption { option },
                ))
        };
        let element = v_flex()
            .id("drain-options")
            .track_focus(&self.focus)
            .mt_2()
            .gap_1()
            .child(option(
                "drain-emptydir",
                "Delete emptyDir data",
                self.delete_emptydir_data,
                DrainOption::DeleteEmptydirData,
            ))
            .child(says(
                "drain-emptydir-note",
                "Also evicts pods with emptyDir volumes. That data is lost.",
            ))
            .child(option(
                "drain-force",
                "Force pods without a controller",
                self.force,
                DrainOption::Force,
            ))
            .child(says(
                "drain-force-note",
                "Also evicts pods no controller manages. Nothing recreates them.",
            ));
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }
}

fn open_drain(target: WriteTarget, window: &mut Window, cx: &mut Context<Workspace>) {
    let choices = cx.new(DrainChoices::new);
    let text = drain_dialog(&target);
    let this = cx.weak_entity();
    let content = choices.clone().into();
    let pinned = write_confirm::target_lines(&target).len();
    confirm::open_pinned_with(window, cx, text, pinned, content, move |window, cx| {
        let chosen = choices.read(cx);
        let request = Request {
            action: ResourceAction::Drain,
            target: target.clone(),
            proposal: WriteProposal::Drain {
                delete_emptydir_data: chosen.delete_emptydir_data,
                force: chosen.force,
            },
        };
        let _ = this.update(cx, |ws, cx| start_review(ws, request, None, window, cx));
        true
    });
}

/// Pods as a report names them: the first few, and how many more.
fn pod_names<'a>(pods: impl ExactSizeIterator<Item = &'a PodName>) -> String {
    let total = pods.len();
    let names: Vec<String> = pods
        .take(REPORT_NAMES)
        .map(|p| format!("{}/{}", p.namespace, p.name))
        .collect();
    match total.saturating_sub(REPORT_NAMES) {
        0 => names.join(", "),
        more => format!("{} and {more} more", names.join(", ")),
    }
}

/// `count` pods, in words.
fn pods(count: usize) -> String {
    match count {
        1 => "1 pod".to_owned(),
        n => format!("{n} pods"),
    }
}

/// Why a drain left a pod, in words.
fn blocked_reason(reason: &BlockedReason) -> String {
    match reason {
        BlockedReason::DisruptionBudget => {
            "a PodDisruptionBudget does not allow evicting them now".to_owned()
        }
        BlockedReason::Unmanaged => "no controller would recreate them".to_owned(),
        BlockedReason::LocalStorage => "their emptyDir data would be lost".to_owned(),
        BlockedReason::Changed => "they were replaced since they were listed".to_owned(),
        BlockedReason::Throttled => "the API server is limiting requests".to_owned(),
        BlockedReason::ManagedSinceReview => {
            "their GitOps manager is not the one reviewed".to_owned()
        }
        BlockedReason::Failed(why) => {
            format!(
                "the eviction failed: {}",
                first_line(why, REPORT_REASON_LEN)
            )
        }
        _ => "Flint cannot say why".to_owned(),
    }
}

/// The blocked pods, a line per reason, in the order first met.
fn blocked_lines(blocked: &[(PodName, BlockedReason)]) -> Vec<String> {
    let mut groups: Vec<(String, Vec<&PodName>)> = Vec::new();
    for (pod, reason) in blocked {
        let reason = blocked_reason(reason);
        match groups.iter_mut().find(|(r, _)| *r == reason) {
            Some((_, pods)) => pods.push(pod),
            None => groups.push((reason, vec![pod])),
        }
    }
    let hidden: usize = groups
        .iter()
        .skip(REPORT_REASONS)
        .map(|(_, pods)| pods.len())
        .sum();
    let mut lines: Vec<String> = groups
        .iter()
        .take(REPORT_REASONS)
        .map(|(reason, group)| {
            format!(
                "Blocked, {} ({reason}): {}",
                pods(group.len()),
                pod_names(group.iter().copied())
            )
        })
        .collect();
    if hidden > 0 {
        lines.push(format!("Blocked for other reasons: {}.", pods(hidden)));
    }
    lines
}

/// What a drain did, also when it stopped early: a notice with only OK.
fn drain_report(node: &str, report: &DrainReport) -> DialogText {
    let left = !report.blocked.is_empty()
        || !report.remaining.is_empty()
        || !report.terminating.is_empty();
    let title = if report.stopped.is_some() {
        format!("Drain of {node} stopped")
    } else if left {
        format!("Drain of {node} left pods")
    } else {
        format!("Drained {node}")
    };
    let mut lines = Vec::new();
    if let Some(why) = &report.stopped {
        lines.push(format!("Stopped early: {why}."));
    }
    lines.push(if report.cordoned {
        "Cordoned: yes.".to_owned()
    } else {
        "Cordoned: no.".to_owned()
    });
    let line = |lines: &mut Vec<String>, what: &str, list: &[PodName]| {
        if !list.is_empty() {
            lines.push(format!(
                "{what} {}: {}",
                pods(list.len()),
                pod_names(list.iter())
            ));
        }
    };
    if report.evicted.is_empty() {
        lines.push("Evicted 0 pods.".to_owned());
    } else {
        line(&mut lines, "Evicted", &report.evicted);
    }
    lines.extend(blocked_lines(&report.blocked));
    line(
        &mut lines,
        "Skipped (DaemonSet or mirror),",
        &report.skipped,
    );
    line(&mut lines, "Not tried,", &report.remaining);
    line(&mut lines, "May still be terminating,", &report.terminating);
    DialogText {
        title,
        text: lines.join("\n"),
        ok: "OK".into(),
        cancel: String::new(),
        id: "drain-report-description",
        default_cancel: false,
    }
}

/// Whether the row of `subject` shows Logs and Shell, beside its resource actions.
pub(in crate::ui) fn has_sessions(subject: Subject) -> bool {
    subject != Subject::Other
}

#[cfg(test)]
mod tests {
    use flint_core::KindId;
    use flint_core::display::ClusterNames;
    use flint_core::write::{BlockedReason, DrainReport, PodName, WriteError};

    use super::{
        REPORT_NAMES, ResourceAction as A, WriteKey, changed_note, drain_report, draining_text,
        offered_for, parse_replicas,
    };

    fn pod(name: &str) -> PodName {
        PodName {
            namespace: "demo".into(),
            name: name.into(),
        }
    }

    // covers UP-53: each kind offers its actions, and Delete only where discovery lists the verb.
    #[test]
    fn each_kind_offers_its_actions() {
        let apps = |plural| KindId::new("apps", plural);
        for (kind, deletable, cordoned, wanted) in [
            (
                apps("deployments"),
                true,
                None,
                &[A::Scale, A::RolloutRestart, A::Delete][..],
            ),
            (
                apps("statefulsets"),
                false,
                None,
                &[A::Scale, A::RolloutRestart],
            ),
            (
                apps("daemonsets"),
                true,
                None,
                &[A::RolloutRestart, A::Delete],
            ),
            (apps("replicasets"), true, None, &[A::Scale, A::Delete]),
            (
                KindId::new("", "replicationcontrollers"),
                false,
                None,
                &[A::Scale],
            ),
            (
                KindId::new("", "nodes"),
                true,
                Some(false),
                &[A::Cordon, A::Drain, A::Delete],
            ),
            (
                KindId::new("", "nodes"),
                false,
                Some(true),
                &[A::Uncordon, A::Drain],
            ),
            (
                KindId::new("", "nodes"),
                false,
                None,
                &[A::Cordon, A::Drain],
            ),
            (KindId::pods(), true, None, &[A::Delete]),
            (KindId::pods(), false, None, &[]),
            (
                KindId::new("example.com", "widgets"),
                true,
                None,
                &[A::Delete],
            ),
        ] {
            assert_eq!(offered_for(&kind, deletable, cordoned), wanted, "{kind}");
        }
    }

    // covers UP-53: the replica count is a non-negative whole number the API can hold.
    #[test]
    fn a_replica_count_is_validated() {
        assert_eq!(parse_replicas("0"), Ok(0));
        assert_eq!(parse_replicas(" 12 "), Ok(12));
        assert_eq!(parse_replicas("2147483647"), Ok(2_147_483_647));
        for wrong in ["", " ", "-1", "+1", "1.5", "two", "1e3", "٣"] {
            assert_eq!(
                parse_replicas(wrong),
                Err("Enter a whole number of replicas, 0 or more.".to_owned()),
                "{wrong:?}"
            );
        }
        for large in ["2147483648", "99999999999999999999"] {
            assert!(
                parse_replicas(large).unwrap_err().contains("at most"),
                "{large}"
            );
        }
    }

    // The first drain and scale dialogs pin the same target lines above their controls, at the
    // minimum window and with very long names.
    #[gpui_kit::test]
    async fn the_dialogs_before_a_review_pin_the_target(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::component::WindowExt as _;
        use gpui_kit::test::TestWindowExt as _;
        use gpui_kit::{AppContext as _, VisualTestContext, px, size};

        use super::{WriteTarget, confirm, open_drain, open_scale};
        use crate::ui::support_tests::open_workspace;
        use crate::ui::workspace::MIN_WINDOW_SIZE;

        let (width, height) = MIN_WINDOW_SIZE;
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        VisualTestContext::from_window(t.window, cx).simulate_resize(size(px(width), px(height)));
        let target = WriteTarget {
            cluster: "c".repeat(5_000).as_str().into(),
            server: Some(format!("https://{}.example.invalid:6443", "s".repeat(600))),
            kind: KindId::new("apps", "deployments"),
            namespace: Some("n".repeat(600)),
            name: "o".repeat(600),
            uid: None,
            resource_version: None,
        };
        for (control, drain) in [("scale-input", false), ("drain-force", true)] {
            let on = target.clone();
            cx.update_window(t.window, |_, window, cx| {
                t.workspace.update(cx, |ws, cx| {
                    if drain {
                        open_drain(on, window, cx)
                    } else {
                        open_scale(ws, on, window, cx)
                    }
                })
            })
            .unwrap();
            for _ in 0..4 {
                cx.run_until_parked();
                cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
                    .unwrap();
            }
            cx.update_window(t.window, |_, window, cx| {
                let lines = confirm::assert_pinned_lines_fit(window, 4, px(height));
                assert!(lines[0].starts_with("Context: c"), "{}", lines[0]);
                let controls = window.find(control).bounds();
                let ok = window.find("ok").bounds();
                assert!(
                    controls.bottom() <= ok.origin.y,
                    "{controls:?} above {ok:?}"
                );
                confirm::assert_full_text_reachable(window, cx);
                confirm::assert_pinned_lines_fit(window, 4, px(height));
                window.close_dialog(cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
    }

    // covers UP-53: a report says everything a drain did and left, and names a budget as the
    // reason a pod stayed.
    #[test]
    fn a_drain_report_says_what_it_did() {
        let mut report = DrainReport::default();
        report.cordoned = true;
        report.evicted = vec![pod("a"), pod("b")];
        report.blocked = vec![
            (pod("c"), BlockedReason::DisruptionBudget),
            (pod("d"), BlockedReason::Unmanaged),
            (pod("e"), BlockedReason::LocalStorage),
            (pod("f"), BlockedReason::DisruptionBudget),
        ];
        report.skipped = vec![pod("ds")];
        report.terminating = vec![pod("a")];
        let shown = drain_report("node-1", &report);
        assert_eq!(shown.title, "Drain of node-1 left pods");
        assert_eq!(shown.cancel, "", "a notice, with only OK");
        for line in [
            "Cordoned: yes.",
            "Evicted 2 pods: demo/a, demo/b",
            "Blocked, 2 pods (a PodDisruptionBudget does not allow evicting them now): demo/c, \
             demo/f",
            "Blocked, 1 pod (no controller would recreate them): demo/d",
            "Blocked, 1 pod (their emptyDir data would be lost): demo/e",
            "Skipped (DaemonSet or mirror), 1 pod: demo/ds",
            "May still be terminating, 1 pod: demo/a",
        ] {
            assert!(
                shown.text.lines().any(|l| l == line),
                "{line} in {}",
                shown.text
            );
        }
    }

    // covers UP-53: the options dialog says, before any choice, that ended pods are removed.
    #[test]
    fn the_drain_dialog_says_completed_pods_are_removed() {
        let target = flint_core::write::WriteTarget {
            cluster: "alpha".into(),
            server: None,
            kind: KindId::new("", "nodes"),
            namespace: None,
            name: "node-1".into(),
            uid: None,
            resource_version: None,
        };
        let text = super::drain_dialog(&target).text;
        assert!(
            text.contains("Completed and failed pods are removed whatever these options say."),
            "{text}"
        );
    }

    // covers UP-53: a drain that stopped says why first, and what it did not get to.
    #[test]
    fn a_stopped_drain_says_why_and_what_is_left() {
        let mut report = DrainReport::default();
        report.cordoned = true;
        report.evicted = vec![pod("a")];
        report.remaining = vec![pod("b"), pod("c")];
        report.stopped = Some(WriteError::Failed("the API server went away".into()));
        let shown = drain_report("node-1", &report);
        assert_eq!(shown.title, "Drain of node-1 stopped");
        assert!(
            shown
                .text
                .starts_with("Stopped early: the API server went away."),
            "{}",
            shown.text
        );
        assert!(
            shown.text.contains("Evicted 1 pod: demo/a"),
            "{}",
            shown.text
        );
        assert!(
            shown.text.contains("Not tried, 2 pods: demo/b, demo/c"),
            "{}",
            shown.text
        );
    }

    // covers UP-53: a clean drain says so, and a long list stays short.
    #[test]
    fn a_clean_drain_and_a_long_list() {
        let mut report = DrainReport::default();
        report.cordoned = true;
        report.evicted = (0..REPORT_NAMES + 3)
            .map(|i| pod(&format!("p{i}")))
            .collect();
        let shown = drain_report("node-1", &report);
        assert_eq!(shown.title, "Drained node-1");
        assert!(shown.text.contains("and 3 more"), "{}", shown.text);
        assert!(!shown.text.contains("p7"), "{}", shown.text);
    }

    // covers UP-53: every running drain is named or counted, and clusters are told apart.
    #[test]
    fn the_strip_names_or_counts_running_drains() {
        let nodes = KindId::new("", "nodes");
        let key = |cluster, name| WriteKey::new(cluster, &nodes, None, name);
        let (a, b, c, d) = (key("x", "a"), key("x", "b"), key("x", "c"), key("x", "d"));
        let names = ClusterNames::new(["x", "y"], &Default::default());
        assert_eq!(draining_text(&[], || names.clone()), None);
        assert_eq!(
            draining_text(&[&a], || names.clone()).unwrap(),
            "Draining a…"
        );
        assert_eq!(
            draining_text(&[&a, &b], || names.clone()).unwrap(),
            "Draining 2 nodes: a, b…"
        );
        assert_eq!(
            draining_text(&[&a, &b, &c, &d], || names.clone()).unwrap(),
            "Draining 4 nodes: a, b, c and 1 more…"
        );
        let other = key("y", "a");
        assert_eq!(
            draining_text(&[&a, &other], || names.clone()).unwrap(),
            "Draining 2 nodes: x/a, y/a…"
        );
    }

    // covers UP-53: near-identical EKS names read differently on the strip, by the shared label.
    #[test]
    fn the_strip_tells_near_identical_eks_clusters_apart() {
        let one = "arn:aws:eks:eu-west-1:123456789012:cluster/payments-production-main";
        let two = "arn:aws:eks:eu-west-2:123456789012:cluster/payments-production-main";
        let nodes = KindId::new("", "nodes");
        let (a, b) = (
            WriteKey::new(one, &nodes, None, "node-1"),
            WriteKey::new(two, &nodes, None, "node-1"),
        );
        let names = ClusterNames::new([one, two], &Default::default());
        let text = draining_text(&[&a, &b], || names.clone()).unwrap();
        assert!(
            text.contains(&format!("{}/node-1", names.with_real(one))),
            "{text}"
        );
        assert!(
            text.contains(&format!("{}/node-1", names.with_real(two))),
            "{text}"
        );
        assert!(
            text.contains("eu-west-1") && text.contains("eu-west-2"),
            "{text}"
        );
    }

    // covers UP-53: clusters alike at both ends read differently on the strip.
    #[test]
    fn the_strip_tells_similar_clusters_apart() {
        let nodes = KindId::new("", "nodes");
        let blue = WriteKey::new(
            "team-platform-production-blue-cluster-main",
            &nodes,
            None,
            "a",
        );
        let green = WriteKey::new(
            "team-platform-production-green-cluster-main",
            &nodes,
            None,
            "a",
        );
        let names = ClusterNames::new(
            [blue.cluster.as_str(), green.cluster.as_str()],
            &Default::default(),
        );
        let text = draining_text(&[&blue, &green], || names.clone()).unwrap();
        let (one, two) = (names.exact(&blue.cluster), names.exact(&green.cluster));
        assert_ne!(one, two);
        assert!(text.contains(&format!("{one}/a")), "{text}");
        assert!(text.contains(&format!("{two}/a")), "{text}");

        // A cluster that left kubeconfig mid-drain is named whole.
        let names = ClusterNames::new([blue.cluster.as_str()], &Default::default());
        let text = draining_text(&[&blue, &green], || names.clone()).unwrap();
        assert!(
            text.contains("team-platform-production-green-cluster-main/a"),
            "{text}"
        );
    }

    // covers UP-53: a context that left kubeconfig cannot read like one still there.
    #[test]
    fn the_strip_names_clusters_whole_when_a_removed_one_reads_like_another() {
        let eks = "arn:aws:eks:eu-west-1:123456789012:cluster/prod";
        let nodes = KindId::new("", "nodes");
        let (kept, gone) = (
            WriteKey::new(eks, &nodes, None, "node-1"),
            WriteKey::new("prod", &nodes, None, "node-1"),
        );
        let names = ClusterNames::new([eks], &Default::default());
        assert_eq!(names.exact(eks), names.exact("prod"));
        assert_eq!(
            draining_text(&[&kept, &gone], || names.clone()).unwrap(),
            format!("Draining 2 nodes: {eks}/node-1, prod/node-1…")
        );
    }

    // covers UP-53: the labels are built only for a strip that names clusters.
    #[test]
    fn the_strip_builds_labels_only_across_clusters() {
        let nodes = KindId::new("", "nodes");
        let key = |cluster, name| WriteKey::new(cluster, &nodes, None, name);
        let (a, b) = (key("x", "a"), key("x", "b"));
        let unused = || -> ClusterNames { panic!("labels built for one cluster") };
        assert_eq!(draining_text(&[], unused), None);
        assert_eq!(draining_text(&[&a], unused).unwrap(), "Draining a…");
        assert_eq!(
            draining_text(&[&a, &b], unused).unwrap(),
            "Draining 2 nodes: a, b…"
        );
    }

    // covers UP-52: a review opened again says the object changed and that nothing ran.
    #[test]
    fn a_new_review_says_the_object_changed() {
        let note = changed_note("web");
        assert!(note.contains("web changed"), "{note}");
        assert!(note.contains("Nothing was applied"), "{note}");
    }
}
