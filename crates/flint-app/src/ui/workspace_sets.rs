//! Cluster sets in the workspace: choosing clusters in the sidebar, saving and opening named sets,
//! and the combined list of the running set.
//!
//! A row's actions go through the detail pane, which holds the row's own cluster, so they take
//! the same path and write gate as a single-cluster row. Before a row opens, and whenever a
//! member changes, [`member_refusal`] closes a row whose cluster cannot be acted on.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flint_core::cluster_sets::{
    ClusterSet, MAX_CLUSTER_SET_MEMBERS, MIN_CLUSTER_SET_MEMBERS, create_cluster_set,
    load_cluster_sets,
};
use flint_core::combined::CombinedMember;
use flint_core::handle::{AdmittedMember, MemberReport, MemberStatus};
use flint_core::settings::ClusterPin;
use flint_core::{ClusterId, ClusterSummary, KindId, KindSnapshot, NavEntry, SyncState};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::detail::{DetailEvent, DetailTab, DetailView, ObjectRef};
use super::super::focus::release_unfocused_input;
use super::super::list::combined::{CombinedListEvent, CombinedResourceList};
use super::super::list_model::combined::CombinedSource;
use super::super::registry::{
    self, ActionError, Bind, CloseClusterSet, CloseDetail, FocusFilter, Forward, Guards,
    OpenClusterSet, OpenPickedClusters, OpenSelectedRow, SaveClusterSet, SelectFirst, SelectLast,
    SelectNext, SelectPrev, ToggleClusterPicking, TogglePickedCluster,
};
use super::{Workspace, takes_focus};

/// How often the running set's members are asked for what the connection state leaves out.
const REPORT_EVERY: Duration = Duration::from_secs(1);

/// Where `settings.toml` lives, for saving sets.
pub(in crate::ui) struct SettingsDir(pub PathBuf);

impl Global for SettingsDir {}

/// The sidebar's choice, the saved sets and the running set's view.
pub(super) struct ClusterSets {
    picking: bool,
    picked: Vec<ClusterId>,
    saved: Vec<ClusterSet>,
    name_input: Entity<InputState>,
    error: Option<String>,
    pub(super) running: Option<RunningSet>,
    /// The newest admission still waiting for the core's answer.
    opening: Option<u64>,
    next_opening: u64,
    /// Tabs opened from a set's row, with the member they stay bound to after the set closes.
    tab_pins: HashMap<u64, AdmittedMember>,
    pub(super) restart: Task<()>,
    _admit: Task<()>,
    _io: Task<()>,
}

/// The running set and its open combined list.
pub(super) struct RunningSet {
    label: String,
    members: Vec<AdmittedMember>,
    /// Members that left their admitted server: their rows are gone and stay gone.
    changed: HashSet<ClusterId>,
    kind: KindId,
    pub(super) list: Entity<CombinedResourceList>,
    snapshots: Vec<Arc<KindSnapshot>>,
    reports: Vec<MemberReport>,
    building: bool,
    rebuild: bool,
    pub(super) _feeds: Vec<Task<()>>,
    _build: Task<()>,
    _reports: Task<()>,
    _list_events: Subscription,
}

impl ClusterSets {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        Self {
            picking: false,
            picked: Vec::new(),
            saved: Vec::new(),
            name_input: cx.new(|cx| InputState::new(window, cx).placeholder("Set name")),
            error: None,
            running: None,
            opening: None,
            next_opening: 0,
            tab_pins: HashMap::new(),
            restart: Task::ready(()),
            _admit: Task::ready(()),
            _io: Task::ready(()),
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.running.is_some()
    }

    /// The open set's member for `cluster`, as the core admitted it.
    pub(super) fn member(&self, cluster: &str) -> Option<&AdmittedMember> {
        self.running
            .as_ref()?
            .members
            .iter()
            .find(|m| m.context() == cluster)
    }

    /// Whether the open set has `cluster` as a member.
    pub(super) fn has_member(&self, cluster: Option<&str>) -> bool {
        cluster.is_some_and(|cluster| self.member(cluster).is_some())
    }

    /// The member tab `id` was opened from, if it came from a set's row.
    pub(super) fn tab_pin(&self, id: u64) -> Option<&AdmittedMember> {
        self.tab_pins.get(&id)
    }

    /// Binds tab `id` to `member`. A tab that already has a pin keeps it.
    pub(super) fn keep_tab_pin(&mut self, id: u64, member: Option<AdmittedMember>) {
        if let Some(member) = member {
            self.tab_pins.entry(id).or_insert(member);
        }
    }

    pub(super) fn picking(&self) -> bool {
        self.picking
    }

    fn is_picked(&self, cluster: &str) -> bool {
        self.picked.iter().any(|c| c == cluster)
    }
}

/// Why a row of `pin`'s cluster cannot be opened or acted on, if so. Protection is not here: a
/// protected cluster's rows open read-only and the write path refuses their actions.
pub(super) fn member_refusal(
    pin: &ClusterPin,
    summary: Option<&ClusterSummary>,
    report: Option<&MemberReport>,
    shown: &str,
) -> Option<String> {
    let Some(summary) = summary else {
        return Some(format!("{shown} is no longer in kubeconfig"));
    };
    if !summary.confirmed {
        return Some(format!(
            "{shown} is provisional: a kubeconfig file defining it can't be read"
        ));
    }
    let on_pin = summary
        .info
        .server
        .as_deref()
        .is_some_and(|server| pin.matches(&pin.context, server));
    if !on_pin {
        return Some(format!("{shown} now points at another server"));
    }
    let report = report?;
    if report.paused {
        return Some(format!(
            "{shown} is paused: a kubeconfig file defining it can't be read"
        ));
    }
    match &report.status {
        MemberStatus::SignInNeeded { .. } => {
            Some(format!("{shown} needs sign-in: select it in the sidebar"))
        }
        _ => None,
    }
}

/// `snapshot` holding no rows, in `state`: what a member shows before its rows are confirmed as
/// its own, and after it left its server.
fn without_rows(snapshot: &KindSnapshot, state: SyncState) -> Arc<KindSnapshot> {
    Arc::new(KindSnapshot {
        rows: Vec::new(),
        state,
        limited_to: None,
        ..snapshot.clone()
    })
}

/// What a member's status note says, when it has something to say.
fn status_note(report: &MemberReport) -> Option<&'static str> {
    if report.paused {
        return Some("Paused");
    }
    match report.status {
        MemberStatus::SignInNeeded { .. } => Some("Sign-in needed"),
        MemberStatus::Reconnecting { .. } => Some("Reconnecting"),
        MemberStatus::Failed { .. } => Some("Failed"),
        _ => None,
    }
}

impl Workspace {
    /// The set actions' checks, and the list actions' while a combined list is open.
    pub(super) fn set_guards(guards: Guards<Self>) -> Guards<Self> {
        fn list_open(ws: &Workspace, _: &App) -> bool {
            ws.list.is_some() || ws.sets.is_open()
        }
        guards
            .when::<FocusFilter>(list_open)
            .when::<SelectPrev>(list_open)
            .when::<SelectNext>(list_open)
            .when::<SelectFirst>(list_open)
            .when::<SelectLast>(list_open)
            .when::<CloseDetail>(list_open)
            .when::<OpenSelectedRow>(list_open)
            .when::<CloseClusterSet>(|ws, _| ws.sets.is_open())
            .when::<OpenPickedClusters>(|ws, _| {
                (MIN_CLUSTER_SET_MEMBERS..=MAX_CLUSTER_SET_MEMBERS).contains(&ws.sets.picked.len())
            })
            .add::<TogglePickedCluster>(|_, _| true, |ws, a, cx| ws.check_pick(a, cx))
            .add::<SaveClusterSet>(
                |ws, _| {
                    (MIN_CLUSTER_SET_MEMBERS..=MAX_CLUSTER_SET_MEMBERS)
                        .contains(&ws.sets.picked.len())
                },
                |_, a, _| match a.name.trim().is_empty() {
                    true => Err(ActionError::rejected::<SaveClusterSet>("the name is empty")),
                    false => Ok(()),
                },
            )
            .add::<OpenClusterSet>(
                |_, _| true,
                |ws, a, _| match ws.sets.saved.iter().any(|s| s.name == a.name) {
                    true => Ok(()),
                    false => Err(ActionError::rejected::<OpenClusterSet>(format!(
                        "no cluster set named {}",
                        a.name
                    ))),
                },
            )
    }

    pub(super) fn bind_set_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleClusterPicking>(Self::toggle_picking)
            .bind::<TogglePickedCluster>(Self::toggle_picked)
            .bind::<OpenPickedClusters>(Self::open_picked)
            .bind::<SaveClusterSet>(Self::save_set)
            .bind::<OpenClusterSet>(Self::open_saved)
            .bind::<CloseClusterSet>(Self::close_set)
    }

    /// Registers the set actions and forwards the open combined list's.
    pub(super) fn register_set_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &Context<Self>,
    ) -> E {
        let element = Self::bind_set_actions(registry::OnView::new(element, cx)).into_element();
        let list = self.sets.running.as_ref().map(|run| &run.list);
        let element =
            CombinedResourceList::bind_actions(Forward::new(element, list)).into_element();
        CombinedResourceList::bind_list_actions(Forward::new(element, list)).into_element()
    }

    /// Reads the saved sets once, when the window opens.
    pub(super) fn load_saved_sets(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = cx.try_global::<SettingsDir>().map(|d| d.0.clone()) else {
            return;
        };
        let read = cx.background_spawn(async move { load_cluster_sets(&dir) });
        self.sets._io = cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |ws, cx| {
                match result {
                    Ok(saved) => ws.sets.saved = saved,
                    Err(e) => ws.sets.error = Some(e.to_string()),
                }
                cx.notify();
            });
        });
    }

    fn check_pick(&self, action: &TogglePickedCluster, cx: &App) -> Result<(), ActionError> {
        let reject = |why: &str| Err(ActionError::rejected::<TogglePickedCluster>(why));
        let Some(summary) = self.cluster_summary(&action.cluster, cx) else {
            return reject("no kubeconfig context by that name");
        };
        if self.sets.is_picked(&action.cluster) {
            return Ok(());
        }
        if summary.info.server.is_none() {
            return reject("the context has no API server");
        }
        if self.sets.picked.len() >= MAX_CLUSTER_SET_MEMBERS {
            return reject("a cluster set holds at most 5 clusters");
        }
        Ok(())
    }

    fn toggle_picking(&mut self, _: &ToggleClusterPicking, _: &mut Window, cx: &mut Context<Self>) {
        self.sets.picking = !self.sets.picking;
        self.sets.picked.clear();
        self.sets.error = None;
        cx.notify();
    }

    fn toggle_picked(
        &mut self,
        action: &TogglePickedCluster,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_pick(action, cx) {
            self.sets.error = Some(e.to_string());
            cx.notify();
            return;
        }
        self.sets.error = None;
        match self.sets.picked.iter().position(|c| *c == action.cluster) {
            Some(at) => {
                self.sets.picked.remove(at);
            }
            None => self.sets.picked.push(action.cluster.clone()),
        }
        cx.notify();
    }

    /// The chosen clusters pinned to the servers they show now, in kubeconfig order.
    fn picked_pins(&self, cx: &App) -> Vec<ClusterPin> {
        self.model
            .read(cx)
            .clusters
            .iter()
            .filter(|c| self.sets.is_picked(&c.info.name))
            .filter_map(|c| {
                let server = c.info.server.as_ref()?;
                Some(ClusterPin::new(c.info.name.clone(), server.clone()))
            })
            .collect()
    }

    fn open_picked(&mut self, _: &OpenPickedClusters, window: &mut Window, cx: &mut Context<Self>) {
        let pins = self.picked_pins(cx);
        let label = format!("{} clusters", pins.len());
        self.open_set(label, pins, true, window, cx);
    }

    fn open_saved(&mut self, action: &OpenClusterSet, window: &mut Window, cx: &mut Context<Self>) {
        let Some(set) = self.sets.saved.iter().find(|s| s.name == action.name) else {
            return;
        };
        let (label, pins) = (set.name.clone(), set.members.clone());
        self.open_set(label, pins, false, window, cx);
    }

    fn save_set(&mut self, action: &SaveClusterSet, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = cx.try_global::<SettingsDir>().map(|d| d.0.clone()) else {
            self.sets.error = Some("Can't save: no settings folder".to_owned());
            cx.notify();
            return;
        };
        let pins = self.picked_pins(cx);
        let name = action.name.clone();
        let write = cx.background_spawn(async move { create_cluster_set(&dir, &name, &pins) });
        let input = self.sets.name_input.clone();
        let handle = window.window_handle();
        self.sets._io = cx.spawn(async move |this, cx| {
            let result = write.await;
            let _ = cx.update_window(handle, |_, window, cx| {
                let _ = this.update(cx, |ws, cx| {
                    match result {
                        Ok(settings) => {
                            ws.sets.saved = settings.cluster_sets;
                            ws.sets.error = None;
                            input.update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        Err(e) => ws.sets.error = Some(e.to_string()),
                    }
                    cx.notify();
                });
            });
        });
    }

    /// Asks the core to run `pins` as the set, and opens their combined list once it has checked
    /// every one. If any is no longer the cluster it was pinned to, nothing starts, whatever is
    /// open stays, and the sidebar says which.
    fn open_set(
        &mut self,
        label: String,
        pins: Vec<ClusterPin>,
        picked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let names = self.cluster_names(cx);
        let refused = pins.iter().find_map(|pin| {
            let summary = self.cluster_summary(&pin.context, cx);
            member_refusal(pin, summary, None, &names.exact(&pin.context))
        });
        if let Some(why) = refused {
            self.sets.error = Some(why);
            cx.notify();
            return;
        }
        self.sets.error = None;
        let token = self.sets.next_opening;
        self.sets.next_opening += 1;
        self.sets.opening = Some(token);
        let admit = self.model.read(cx).core().admit_running_set(pins);
        self.sets._admit = cx.spawn_in(window, async move |this, cx| {
            let admitted = admit.await;
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.finish_open(token, label, picked, admitted, window, cx)
            });
        });
    }

    fn finish_open(
        &mut self,
        token: u64,
        label: String,
        picked: bool,
        admitted: Result<Vec<AdmittedMember>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sets.opening != Some(token) {
            // Closed while the core was admitting it, and nothing newer is on its way.
            if self.sets.opening.is_none() && !self.sets.is_open() && admitted.is_ok() {
                drop(self.model.read(cx).core().admit_running_set(Vec::new()));
            }
            return;
        }
        self.sets.opening = None;
        let members = match admitted {
            Ok(members) => members,
            Err(why) => {
                self.sets.error = Some(why);
                cx.notify();
                return;
            }
        };
        if picked {
            self.sets.picking = false;
            self.sets.picked.clear();
        }
        // The core already runs the new set: closing what was open must not clear it.
        self.sets.running = None;
        self.close_list(cx);
        self.show_set(
            label,
            members,
            KindId::pods(),
            "Pods".to_owned(),
            window,
            cx,
        );
    }

    /// Opens the combined list of `kind` over `members`, feeding it from the core. Rows appear
    /// only once the core confirms, after they were read, that their member is still admitted.
    fn show_set(
        &mut self,
        label: String,
        members: Vec<AdmittedMember>,
        kind: KindId,
        kind_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let core = self.model.read(cx).core().clone();
        let subscriptions: Vec<_> = members
            .iter()
            .map(|member| core.subscribe(member.context(), &kind))
            .collect();
        let snapshots: Vec<Arc<KindSnapshot>> = subscriptions
            .iter()
            .map(|s| without_rows(&s.peek(), SyncState::Loading))
            .collect();
        let source = self.combined_source(&snapshots, cx);
        let title = format!("{kind_label} · {label}");
        let list = cx.new(|cx| CombinedResourceList::new(title, source, Vec::new(), window, cx));
        let events = cx.subscribe_in(&list, window, Self::on_combined_event);
        let feeds = subscriptions
            .into_iter()
            .zip(members.clone())
            .map(|(mut subscription, member)| {
                let core = core.clone();
                cx.spawn_in(window, async move |this, cx| {
                    while subscription.changed().await {
                        let snapshot = subscription.latest();
                        // After the read: rows of a replacement server can only exist once the
                        // core has recorded the move, so this check sees it.
                        if let Some(why) = core.check_member(&member).await {
                            let _ = this.update(cx, |ws, cx| {
                                ws.discard_member(member.context(), why, cx);
                                ws.check_set_row(cx);
                            });
                            break;
                        }
                        let alive = this
                            .update_in(cx, |ws, window, cx| ws.feed_member(snapshot, window, cx));
                        if alive.is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        let reports = cx.spawn(async move |this, cx| {
            loop {
                let reports = core.running_set().await;
                if this
                    .update(cx, |ws, cx| ws.set_member_reports(reports, cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(REPORT_EVERY).await;
            }
        });
        let focus = list.read(cx).focus_handle().clone();
        window.focus(&focus, cx);
        release_unfocused_input(window, cx);
        self.nav
            .update(cx, |nav, cx| nav.set_selected(Some(kind.clone()), cx));
        self.sets.running = Some(RunningSet {
            label,
            members,
            changed: HashSet::new(),
            kind,
            list,
            snapshots,
            reports: Vec::new(),
            building: false,
            rebuild: false,
            _feeds: feeds,
            _build: Task::ready(()),
            _reports: reports,
            _list_events: events,
        });
        cx.notify();
    }

    /// With a set open, a kind chosen in the navigation shows across the set. Returns whether
    /// it did.
    pub(super) fn show_kind_in_set(
        &mut self,
        entry: &NavEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(run) = &self.sets.running else {
            return false;
        };
        if run.kind == entry.id {
            let focus = run.list.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
            return true;
        }
        let (label, members, changed) =
            (run.label.clone(), run.members.clone(), run.changed.clone());
        self.close_set_detail(cx);
        self.show_set(
            label,
            members,
            entry.id.clone(),
            entry.label.clone(),
            window,
            cx,
        );
        if let Some(run) = &mut self.sets.running {
            run.changed = changed;
        }
        true
    }

    fn combined_members(&self, snapshots: &[Arc<KindSnapshot>], cx: &App) -> Vec<CombinedMember> {
        let model = self.model.read(cx);
        let names = model.names();
        snapshots
            .iter()
            .map(|snapshot| CombinedMember {
                snapshot: snapshot.clone(),
                // The whole name: rows of near-identical clusters must not read alike.
                display_name: names.exact(&snapshot.cluster),
                tag: self
                    .cluster_summary(&snapshot.cluster, cx)
                    .and_then(|c| model.cluster_tag(c)),
            })
            .collect()
    }

    fn combined_source(&self, snapshots: &[Arc<KindSnapshot>], cx: &App) -> CombinedSource {
        CombinedSource::new(self.combined_members(snapshots, cx))
            .expect("one snapshot of one kind per cluster")
    }

    /// Takes a member's new list, and refreshes the detail if it shows a row that changed.
    pub(super) fn feed_member(
        &mut self,
        snapshot: Arc<KindSnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = &mut self.sets.running else {
            return;
        };
        if snapshot.kind != run.kind || run.changed.contains(&snapshot.cluster) {
            return;
        }
        let Some(at) = run
            .snapshots
            .iter()
            .position(|s| s.cluster == snapshot.cluster)
        else {
            return;
        };
        let before = std::mem::replace(&mut run.snapshots[at], snapshot.clone());
        if let Some(detail) = self.detail.clone() {
            let object = detail.read(cx).object();
            let version = |s: &KindSnapshot| {
                s.rows
                    .iter()
                    .find(|r| r.uid == object.uid)
                    .map(|r| r.resource_version.clone())
            };
            let changed = object.cluster == snapshot.cluster
                && matches!(
                    (version(&before), version(&snapshot)),
                    (Some(old), Some(new)) if old != new
                );
            if changed {
                detail.update(cx, |detail, cx| detail.refresh(window, cx));
            }
        }
        self.rebuild_combined(cx);
    }

    /// Drops `cluster`'s rows for good: it left the server it was admitted on. Says why once.
    pub(super) fn discard_member(&mut self, cluster: &str, why: String, cx: &mut Context<Self>) {
        let Some(run) = &mut self.sets.running else {
            return;
        };
        if !run.changed.insert(cluster.to_owned()) {
            return;
        }
        if let Some(at) = run.snapshots.iter().position(|s| s.cluster == cluster) {
            run.snapshots[at] = without_rows(&run.snapshots[at], SyncState::Idle);
        }
        self.rebuild_combined(cx);
        self.refuse(why, cx);
    }

    /// Merges the members off the UI thread; changes during a merge are merged once after it.
    fn rebuild_combined(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &self.sets.running else {
            return;
        };
        if run.building {
            if let Some(run) = &mut self.sets.running {
                run.rebuild = true;
            }
            return;
        }
        let members = self.combined_members(&run.snapshots, cx);
        let job = cx.background_spawn(async move { CombinedSource::new(members) });
        let task = cx.spawn(async move |this, cx| {
            let built = job.await;
            let _ = this.update(cx, |ws, cx| {
                let Some(run) = &mut ws.sets.running else {
                    return;
                };
                run.building = false;
                let again = std::mem::take(&mut run.rebuild);
                if let Ok(source) = built {
                    run.list.update(cx, |list, cx| list.set_source(source, cx));
                }
                if again {
                    ws.rebuild_combined(cx);
                }
            });
        });
        if let Some(run) = &mut self.sets.running {
            run.building = true;
            run._build = task;
        }
    }

    pub(super) fn set_member_reports(
        &mut self,
        reports: Vec<MemberReport>,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = &mut self.sets.running else {
            return;
        };
        if run.reports != reports {
            run.reports = reports;
            self.check_set_row(cx);
            cx.notify();
        }
    }

    /// Why a row of `cluster` cannot be opened or acted on now, if so.
    fn set_row_refusal(&self, cluster: &str, cx: &App) -> Option<String> {
        let names = self.cluster_names(cx);
        let shown = names.exact(cluster);
        let run = self.sets.running.as_ref()?;
        let Some(member) = run.members.iter().find(|m| m.context() == cluster) else {
            return Some(format!("{shown} is not in this set"));
        };
        if run.changed.contains(cluster) {
            return Some(format!(
                "{shown} changed since this set opened. Open the set again."
            ));
        }
        let report = run.reports.iter().find(|r| r.cluster == cluster);
        member_refusal(
            member.pin(),
            self.cluster_summary(cluster, cx),
            report,
            &shown,
        )
    }

    /// The member a new session on `cluster` is bound to while a set has it, or why none may
    /// open. Outside a set, or for another cluster, sessions follow the context as usual.
    pub(super) fn session_member(
        &self,
        cluster: &str,
        cx: &App,
    ) -> Result<Option<AdmittedMember>, String> {
        let Some(member) = self.sets.member(cluster) else {
            return Ok(None);
        };
        match self.set_row_refusal(cluster, cx) {
            Some(why) => Err(why),
            None => Ok(Some(member.clone())),
        }
    }

    /// Discards the rows of members that no longer show their admitted server.
    fn sweep_set_members(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &self.sets.running else {
            return;
        };
        let names = self.cluster_names(cx);
        let left: Vec<(ClusterId, String)> = run
            .members
            .iter()
            .filter(|m| !run.changed.contains(m.context()))
            .filter_map(|m| {
                let summary = self.cluster_summary(m.context(), cx);
                let on_pin = summary.is_some_and(|c| {
                    m.same_endpoint(c)
                        && c.info
                            .server
                            .as_deref()
                            .is_some_and(|server| m.pin().matches(m.context(), server))
                });
                if on_pin {
                    return None;
                }
                let shown = names.exact(m.context());
                let why = member_refusal(m.pin(), summary, None, &shown).unwrap_or_else(|| {
                    format!("{shown} changed since this set opened. Open the set again.")
                });
                Some((m.context().to_owned(), why))
            })
            .collect();
        for (cluster, why) in left {
            self.discard_member(&cluster, why, cx);
        }
    }

    /// Closes the open row if its cluster can no longer be acted on, saying why. The write gate
    /// refuses such a change anyway; this keeps the actions from being offered.
    pub(super) fn check_set_row(&mut self, cx: &mut Context<Self>) {
        if !self.sets.is_open() {
            return;
        }
        self.sweep_set_members(cx);
        let Some(detail) = &self.detail else {
            return;
        };
        let cluster = detail.read(cx).object().cluster.clone();
        if let Some(why) = self.set_row_refusal(&cluster, cx) {
            self.close_set_detail(cx);
            self.refuse(why, cx);
        }
    }

    fn close_set_detail(&mut self, cx: &mut Context<Self>) {
        self.detail = None;
        self._detail_subscriptions.clear();
        if let Some(run) = &self.sets.running {
            run.list.update(cx, |list, cx| list.clear_selection(cx));
        }
    }

    fn on_combined_event(
        &mut self,
        list: &Entity<CombinedResourceList>,
        event: &CombinedListEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cleared = matches!(event, CombinedListEvent::Selected(None));
        if cleared {
            self.clear_action_error(cx);
            self.detail = None;
            self._detail_subscriptions.clear();
            let focus = list.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
            release_unfocused_input(window, cx);
            cx.notify();
            return;
        }
        // Only the very row asked for, by cluster and uid: the selection may have moved since.
        if let CombinedListEvent::Reopened(id) = event
            && list.read(cx).selected() != Some(id)
        {
            return;
        }
        let Some(row) = list.read(cx).selected_object() else {
            return;
        };
        if let Some(why) = self.set_row_refusal(&row.cluster, cx) {
            self.close_set_detail(cx);
            self.refuse(why, cx);
            return;
        }
        let Some(run) = &self.sets.running else {
            return;
        };
        let Some(member) = run
            .members
            .iter()
            .find(|m| m.context() == row.cluster)
            .cloned()
        else {
            return;
        };
        let object = ObjectRef {
            cluster: row.cluster,
            kind: run.kind.clone(),
            kind_label: list.read(cx).label.clone(),
            namespace: row.namespace,
            name: row.name,
            uid: row.uid,
        };
        if self
            .detail
            .as_ref()
            .is_some_and(|d| *d.read(cx).object() == object)
        {
            // Asked for again: a narrow window brings it back in front of the list.
            self.list_in_front = false;
            release_unfocused_input(window, cx);
            cx.notify();
            return;
        }
        let tab = self
            .detail
            .as_ref()
            .map_or(DetailTab::Overview, |d| d.read(cx).tab());
        self.clear_action_error(cx);
        let shown = self.cluster_names(cx).exact(member.context());
        // Every read this view makes is bound to the member, in the core.
        let core = self.model.read(cx).core().bound_to(member);
        let detail = cx.new(|cx| {
            let mut view = DetailView::new(core, object, tab, window, cx);
            view.set_row_cluster(shown, cx);
            view
        });
        let list = list.downgrade();
        self._detail_subscriptions = vec![cx.subscribe_in(
            &detail,
            window,
            move |this, _, event, window, cx| match event {
                DetailEvent::Close => {
                    let _ = list.update(cx, |list, cx| list.clear_selection(cx));
                }
                DetailEvent::OpenLogs(request, focus) => {
                    let focus = takes_focus(focus, window, cx);
                    this.open_logs(request.clone(), focus, window, cx)
                }
                DetailEvent::OpenShell(request, focus) => {
                    let focus = takes_focus(focus, window, cx);
                    this.open_shell(request.clone(), focus, window, cx)
                }
                DetailEvent::Refused(why) => this.refuse(why.clone(), cx),
                DetailEvent::ConfirmWrite(review) => this.confirm_detail_write(review, window, cx),
            },
        )];
        self.detail = Some(detail);
        self.list_in_front = false;
        release_unfocused_input(window, cx);
        self.tell_detail_sessions_blocked(cx);
        cx.notify();
    }

    fn close_set(&mut self, _: &CloseClusterSet, window: &mut Window, cx: &mut Context<Self>) {
        self.close_list(cx);
        let focus = self.focus.clone();
        window.focus(&focus, cx);
        release_unfocused_input(window, cx);
        cx.notify();
    }

    /// Stops the running set, if one is open. Called wherever the list closes.
    pub(super) fn leave_set(&mut self, cx: &mut Context<Self>) {
        self.sets.opening = None;
        if self.sets.running.take().is_some() {
            // Through the same queue as admissions, so it can't overtake a later one.
            drop(self.model.read(cx).core().admit_running_set(Vec::new()));
        }
    }

    /// The running set's main area: the combined list beside the selected row's detail, or the
    /// detail alone in a narrow window.
    pub(super) fn render_set_view(
        &self,
        window_width: f32,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let run = self.sets.running.as_ref()?;
        let shown = self
            .detail
            .clone()
            .filter(|_| !self.narrow || !self.list_in_front);
        let body = match shown {
            Some(detail) if self.narrow => div()
                .flex_1()
                .min_h_0()
                .relative()
                .child(detail)
                .children(self.detail_tag_strip(theme, cx))
                .into_any_element(),
            detail => h_flex()
                .flex_1()
                .min_h_0()
                .child(div().flex_1().min_w_0().h_full().child(run.list.clone()))
                .when_some(detail, |el, detail| {
                    el.child(
                        div()
                            .w(px(self.detail_width(window_width)))
                            .h_full()
                            .flex_none()
                            .relative()
                            .border_l_1()
                            .border_color(theme.border)
                            .child(detail)
                            .children(self.detail_tag_strip(theme, cx)),
                    )
                })
                .into_any_element(),
        };
        Some(
            v_flex()
                .id("cluster-set-view")
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.render_set_bar(run, theme, cx))
                .child(body)
                .test_support()
                .into_any_element(),
        )
    }

    /// The set's name, each member's exact name with anything wrong with it, and Close.
    fn render_set_bar(&self, run: &RunningSet, theme: &Theme, cx: &App) -> impl IntoElement {
        let names = self.cluster_names(cx);
        h_flex()
            .flex_none()
            .flex_wrap()
            .gap_2()
            .px_3()
            .py_1()
            .text_xs()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(run.label.clone()),
            )
            .children(run.members.iter().map(|member| {
                let context = member.context();
                let shown = names.exact(context);
                let note = if run.changed.contains(context) {
                    Some("Changed")
                } else {
                    run.reports
                        .iter()
                        .find(|r| r.cluster == context)
                        .and_then(status_note)
                };
                let text = match note {
                    Some(note) => format!("{shown}: {note}"),
                    None => shown,
                };
                div()
                    .id(SharedString::from(format!("set-member-{context}")))
                    .aria_label(text.clone())
                    .when(note.is_some(), |el| el.text_color(theme.warning))
                    .child(text)
                    .test_support()
            }))
            .child(div().flex_1())
            .child(
                Button::new("close-cluster-set")
                    .xsmall()
                    .ghost()
                    .label("Close Set")
                    .on_click(registry::on_click(CloseClusterSet)),
            )
    }

    /// A cluster row's checkbox, while choosing clusters.
    pub(super) fn render_pick_box(&self, cluster: &str) -> Option<impl IntoElement> {
        self.sets.picking.then(|| {
            Checkbox::new(SharedString::from(format!("pick-{cluster}")))
                .checked(self.sets.is_picked(cluster))
                .on_click(registry::on_click_contained(TogglePickedCluster {
                    cluster: cluster.to_owned(),
                }))
        })
    }

    /// The sidebar's foot: saved sets, and the controls for choosing clusters.
    pub(super) fn render_sets_panel(&self, theme: &Theme, cx: &App) -> impl IntoElement {
        let sets = &self.sets;
        let count = sets.picked.len();
        let can_run = (MIN_CLUSTER_SET_MEMBERS..=MAX_CLUSTER_SET_MEMBERS).contains(&count);
        let typed = sets.name_input.read(cx).value().trim().to_string();
        v_flex()
            .id("cluster-sets")
            .flex_none()
            .gap_1()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(theme.sidebar_border)
            .text_xs()
            .children(sets.saved.iter().map(|set| {
                Button::new(SharedString::from(format!("open-set-{}", set.name)))
                    .xsmall()
                    .ghost()
                    .w_full()
                    .label(set.name.clone())
                    .on_click(registry::on_click(OpenClusterSet {
                        name: set.name.clone(),
                    }))
            }))
            .child(
                Button::new("toggle-cluster-picking")
                    .xsmall()
                    .ghost()
                    .w_full()
                    .label(if sets.picking {
                        "Cancel"
                    } else {
                        "Choose Clusters"
                    })
                    .on_click(registry::on_click(ToggleClusterPicking)),
            )
            .when(sets.picking, |el| {
                el.child(
                    div()
                        .id("picked-count")
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{count} of {MIN_CLUSTER_SET_MEMBERS} to {MAX_CLUSTER_SET_MEMBERS} chosen"
                        ))
                        .test_support(),
                )
                .child(
                    Button::new("open-picked-clusters")
                        .xsmall()
                        .primary()
                        .w_full()
                        .disabled(!can_run)
                        .label("Open Together")
                        .on_click(registry::on_click(OpenPickedClusters)),
                )
                .child(Input::new(&sets.name_input).xsmall())
                .child(
                    Button::new("save-cluster-set")
                        .xsmall()
                        .outline()
                        .w_full()
                        .disabled(!can_run || typed.is_empty())
                        .label("Save Set")
                        .on_click(registry::on_click(SaveClusterSet { name: typed })),
                )
            })
            .when_some(sets.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("cluster-sets-error")
                        .text_color(theme.danger)
                        .aria_label(error.clone())
                        .child(error)
                        .test_support(),
                )
            })
    }
}

#[cfg(test)]
#[path = "workspace_sets_tests.rs"]
mod tests;
