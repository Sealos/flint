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
    delete_cluster_set, load_cluster_sets, rename_cluster_set,
};
use flint_core::combined::CombinedMember;
use flint_core::display::{
    CUSTOM_NAME_PUNCTUATION, MAX_CUSTOM_NAME_CHARS, clean_custom_name, cluster_name_key,
};
use flint_core::handle::{AdmittedMember, LeftOutMember, MemberReport, MemberStatus, OpenedSet};
use flint_core::settings::{ClusterPin, Settings, SettingsError};
use flint_core::{ClusterId, ClusterSummary, KindId, KindSnapshot, NavEntry, SyncState};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::confirm::{self, DialogText};
use super::super::dashboard::set::{OpenSetMember, OpenSetProblem, SetDashboardView};
use super::super::detail::{DetailEvent, DetailTab, DetailView, ObjectRef};
use super::super::focus::release_unfocused_input;
use super::super::list::combined::{CombinedListEvent, CombinedResourceList};
use super::super::list_model::combined::CombinedSource;
use super::super::registry::{
    self, ActionError, Bind, CloseClusterSet, CloseDetail, DeleteClusterSet, FocusFilter, Forward,
    Guards, OpenClusterSet, OpenClusterSetDashboard, OpenPickedClusters, OpenSelectedRow,
    RenameClusterSet, SaveClusterSet, SelectFirst, SelectLast, SelectNext, SelectPrev,
    ShowClusterSetList, ToggleClusterPicking, TogglePickedCluster,
};
#[cfg(any(test, feature = "automation"))]
use super::super::view_state::{ClusterSetsState, OpenSetState, SetGroupState, SetMemberState};
use super::{WRITES_PAUSED, Workspace, takes_focus, writes_paused_reason};

/// How often the running set's members are asked for what the connection state leaves out.
const REPORT_EVERY: Duration = Duration::from_secs(1);

/// Where `settings.toml` lives, for saving sets.
pub(in crate::ui) struct SettingsDir(pub PathBuf);

impl Global for SettingsDir {}

/// The sidebar's choice, the saved sets and the running set's view.
pub(super) struct ClusterSets {
    picking: bool,
    /// The chosen clusters, each on the server it showed when it was chosen.
    picked: Vec<ClusterPin>,
    saved: Vec<ClusterSet>,
    name_input: Entity<InputState>,
    error: Option<String>,
    pub(super) running: Option<RunningSet>,
    /// The newest admission still waiting for the core's answer.
    opening: Option<PendingOpen>,
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
    /// The saved set this view shows, which a rename or delete of it carries over to this view.
    set_id: Option<String>,
    members: Vec<AdmittedMember>,
    /// Members that left their admitted server: their rows are gone and stay gone.
    changed: HashSet<ClusterId>,
    /// Members not subscribed to `kind`, because their discovery can't vouch for it yet.
    held: HashSet<ClusterId>,
    /// The window its feeds draw in, for a held member that joins later.
    window: AnyWindowHandle,
    /// Saved members the core did not admit when the set opened, and why.
    left_out: Vec<LeftOutMember>,
    kind: KindId,
    kind_label: String,
    pub(super) list: Entity<CombinedResourceList>,
    /// The set's dashboard, shown in place of the list while open.
    dashboard: Option<Entity<SetDashboardView>>,
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

    /// The kind the open set lists.
    pub(super) fn running_kind(&self) -> Option<&KindId> {
        self.running.as_ref().map(|run| &run.kind)
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
        self.picked.iter().any(|pin| pin.context == cluster)
    }
}

/// An admission on its way to the core.
struct PendingOpen {
    token: u64,
    /// The saved set it opens, if any.
    set_id: Option<String>,
}

/// A saved set being opened.
#[derive(Clone)]
struct SavedOpening {
    name: String,
    id: String,
}

/// The name field of the Rename Cluster Set dialog.
struct SetNameField {
    input: Entity<InputState>,
    error: Option<String>,
}

impl Render for SetNameField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        v_flex()
            .gap_1()
            .child(Input::new(&self.input).id("rename-set-input").small())
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("rename-set-error")
                        .aria_label(error.clone())
                        .text_xs()
                        .text_color(danger)
                        .child(error)
                        .test_support(),
                )
            })
    }
}

/// Whether a click on a cluster row chooses it rather than connecting to it.
pub(super) fn click_chooses(picking: bool, modifiers: Modifiers, cx: &App) -> bool {
    picking || modifiers.secondary(cx)
}

/// Why the set `id` cannot be called `typed`. It may change the case of its own name.
fn rename_problem(saved: &[ClusterSet], id: &str, typed: &str) -> Option<String> {
    let Some(cleaned) = clean_custom_name(typed) else {
        return Some(format!(
            "A set name has 1 to {MAX_CUSTOM_NAME_CHARS} letters, digits or any of \
             {CUSTOM_NAME_PUNCTUATION}"
        ));
    };
    let key = cluster_name_key(&cleaned);
    saved
        .iter()
        .any(|set| set.id != id && cluster_name_key(&set.name) == key)
        .then(|| "Another cluster set already goes by this name.".to_owned())
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
    if summary.paused || report.is_some_and(|r| r.paused) {
        return Some(paused_text(shown));
    }
    match &report?.status {
        MemberStatus::SignInNeeded { .. } => {
            Some(format!("{shown} needs sign-in: select it in the sidebar"))
        }
        _ => None,
    }
}

fn paused_text(shown: &str) -> String {
    format!("{shown} is paused: a kubeconfig file defining it can't be read")
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

/// What a member's status note says, when it has something to say. A paused member starts
/// nothing, so that says it all; otherwise paused writes never hide the connection's state.
fn member_note(
    report: Option<&MemberReport>,
    sessions_paused: bool,
    writes_paused: bool,
) -> Option<String> {
    if sessions_paused || report.is_some_and(|r| r.paused) {
        return Some("Paused".to_owned());
    }
    let status = report.and_then(|r| match r.status {
        MemberStatus::SignInNeeded { .. } => Some("Sign-in needed"),
        MemberStatus::Reconnecting { .. } => Some("Reconnecting"),
        MemberStatus::Failed { .. } => Some("Failed"),
        _ => None,
    });
    match (status, writes_paused) {
        (Some(status), true) => Some(format!("{status}, writes paused")),
        (Some(status), false) => Some(status.to_owned()),
        (None, true) => Some(WRITES_PAUSED.to_owned()),
        (None, false) => None,
    }
}

impl Workspace {
    /// The set actions' checks, and the list actions' while a combined list is open.
    pub(super) fn set_guards(guards: Guards<Self>) -> Guards<Self> {
        // The combined list kept behind the dashboard takes no keys.
        fn list_open(ws: &Workspace, _: &App) -> bool {
            ws.list.is_some()
                || ws
                    .sets
                    .running
                    .as_ref()
                    .is_some_and(|r| r.dashboard.is_none())
        }
        fn dashboard_open(ws: &Workspace, _: &App) -> bool {
            ws.set_dashboard().is_some()
        }
        guards
            .when::<OpenClusterSetDashboard>(|ws, _| ws.sets.is_open())
            .when::<ShowClusterSetList>(dashboard_open)
            .when::<OpenSetMember>(dashboard_open)
            .when::<OpenSetProblem>(dashboard_open)
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
                |ws, a, _| ws.check_saved::<OpenClusterSet>(&a.name),
            )
            .add::<RenameClusterSet>(
                |ws, _| !ws.sets.saved.is_empty(),
                |ws, a, _| ws.check_saved::<RenameClusterSet>(&a.name),
            )
            .add::<DeleteClusterSet>(
                |ws, _| !ws.sets.saved.is_empty(),
                |ws, a, _| ws.check_saved::<DeleteClusterSet>(&a.name),
            )
    }

    fn check_saved<A: Action>(&self, name: &str) -> Result<(), ActionError> {
        match self.sets.saved.iter().any(|s| s.name == name) {
            true => Ok(()),
            false => Err(ActionError::rejected::<A>(format!(
                "no cluster set named {name}"
            ))),
        }
    }

    pub(super) fn bind_set_actions<B: Bind<Self>>(bind: B) -> B {
        Self::bind_set_dashboard_actions(
            bind.bind::<ToggleClusterPicking>(Self::toggle_picking)
                .bind::<TogglePickedCluster>(Self::toggle_picked)
                .bind::<OpenPickedClusters>(Self::open_picked)
                .bind::<SaveClusterSet>(Self::save_set)
                .bind::<OpenClusterSet>(Self::open_saved)
                .bind::<RenameClusterSet>(Self::ask_rename_set)
                .bind::<DeleteClusterSet>(Self::ask_delete_set)
                .bind::<CloseClusterSet>(Self::close_set),
        )
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
                    Err(e) => ws.sets.error = Some(e.safe_reason()),
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
        // A Cmd-click starts choosing from outside the choosing mode.
        self.sets.picking = true;
        let at = self
            .sets
            .picked
            .iter()
            .position(|pin| pin.context == action.cluster);
        match at {
            Some(at) => {
                self.sets.picked.remove(at);
            }
            // `check_pick` passed, so it has a server: the one this choice stands for.
            None => {
                let server = self
                    .cluster_summary(&action.cluster, cx)
                    .and_then(|c| c.info.server.clone());
                if let Some(server) = server {
                    self.sets
                        .picked
                        .push(ClusterPin::new(action.cluster.clone(), server));
                }
            }
        }
        cx.notify();
    }

    /// The chosen clusters on the servers they were chosen on, in kubeconfig order. One no
    /// longer in kubeconfig comes last, for the checks that follow to refuse.
    fn picked_pins(&self, cx: &App) -> Vec<ClusterPin> {
        let clusters = &self.model.read(cx).clusters;
        let mut pins = self.sets.picked.clone();
        pins.sort_by_key(|pin| {
            clusters
                .iter()
                .position(|c| c.info.name == pin.context)
                .unwrap_or(usize::MAX)
        });
        pins
    }

    fn open_picked(&mut self, _: &OpenPickedClusters, window: &mut Window, cx: &mut Context<Self>) {
        let pins = self.picked_pins(cx);
        self.open_set(pins, None, window, cx);
    }

    /// Runs `write` on the settings file after any earlier one, then `done` with what it wrote.
    /// Chained so that a later change neither cancels an earlier one nor lands before it.
    fn write_sets(
        &mut self,
        write: impl FnOnce(&std::path::Path) -> Result<Settings, SettingsError> + Send + 'static,
        done: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = cx.try_global::<SettingsDir>().map(|d| d.0.clone()) else {
            self.sets.error = Some("Can't save: no settings folder".to_owned());
            cx.notify();
            return;
        };
        let earlier = std::mem::replace(&mut self.sets._io, Task::ready(()));
        self.sets._io = cx.spawn_in(window, async move |this, cx| {
            earlier.await;
            let result = cx.background_spawn(async move { write(&dir) }).await;
            let _ = this.update_in(cx, |ws, window, cx| {
                match result {
                    Ok(settings) => {
                        ws.sets.saved = settings.cluster_sets;
                        ws.sets.error = None;
                        done(ws, window, cx);
                    }
                    Err(e) => ws.sets.error = Some(e.safe_reason()),
                }
                cx.notify();
            });
        });
    }

    fn ask_rename_set(
        &mut self,
        action: &RenameClusterSet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_saved::<RenameClusterSet>(&action.name) {
            tracing::warn!("{e}");
            return;
        }
        let Some(id) = self.saved_id(&action.name) else {
            return;
        };
        let name = action.name.clone();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name.clone()));
        let field = cx.new(|_| SetNameField {
            input: input.clone(),
            error: None,
        });
        let dialog = DialogText {
            title: format!("Rename {name}"),
            text: "A new name for this saved set. Its clusters stay the same.".to_owned(),
            ok: "Rename".into(),
            cancel: "Cancel".into(),
            id: "rename-set-description",
            default_cancel: false,
        };
        let view = cx.weak_entity();
        let content = field.clone().into();
        confirm::open_with(window, cx, dialog, Some(content), move |window, cx| {
            let typed = field.read(cx).input.read(cx).value().trim().to_owned();
            let Ok(problem) =
                view.read_with(cx, |ws, _| rename_problem(&ws.sets.saved, &id, &typed))
            else {
                return true;
            };
            if let Some(error) = problem {
                field.update(cx, |field, cx| {
                    field.error = Some(error);
                    cx.notify();
                });
                return false;
            }
            let id = id.clone();
            let _ = view.update(cx, |ws, cx| ws.rename_set(id, typed, window, cx));
            true
        });
        input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The id of the saved set shown as `name`.
    fn saved_id(&self, name: &str) -> Option<String> {
        let set = self.sets.saved.iter().find(|s| s.name == name)?;
        Some(set.id.clone())
    }

    fn rename_set(
        &mut self,
        id: String,
        new_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = id.clone();
        self.write_sets(
            move |dir| rename_cluster_set(dir, &target, &new_name),
            move |ws, _, cx| ws.relabel_running(&id, cx),
            window,
            cx,
        );
    }

    /// Carries a saved set's name, as just written, to its open view.
    fn relabel_running(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(new) = self
            .sets
            .saved
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
        else {
            return;
        };
        let Some(run) = self
            .sets
            .running
            .as_mut()
            .filter(|r| r.set_id.as_deref() == Some(id))
        else {
            return;
        };
        run.label = new.clone();
        let title = format!("{} · {new}", run.kind_label);
        run.list.update(cx, |list, cx| {
            list.label = title;
            cx.notify();
        });
    }

    fn ask_delete_set(
        &mut self,
        action: &DeleteClusterSet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_saved::<DeleteClusterSet>(&action.name) {
            tracing::warn!("{e}");
            return;
        }
        let Some(id) = self.saved_id(&action.name) else {
            return;
        };
        let name = action.name.clone();
        let dialog = DialogText {
            title: format!("Delete {name}?"),
            text: "This removes the saved set and closes it if it is open. Its clusters and \
                   kubeconfig are not changed."
                .to_owned(),
            ok: "Delete".into(),
            cancel: "Cancel".into(),
            id: "delete-set-description",
            default_cancel: true,
        };
        let view = cx.weak_entity();
        confirm::open(window, cx, dialog, move |window, cx| {
            let id = id.clone();
            let _ = view.update(cx, |ws, cx| ws.delete_set(id, window, cx));
        });
    }

    fn delete_set(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let target = id.clone();
        self.write_sets(
            move |dir| delete_cluster_set(dir, &target),
            move |ws, window, cx| ws.close_deleted_set(&id, window, cx),
            window,
            cx,
        );
    }

    /// Closes the view of a set just deleted. An open of it still on its way is called off. The
    /// core may already run its members in place of another open set, so that set closes too;
    /// with none open, the answer to the admission stops them and any other list stays.
    fn close_deleted_set(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let is_it = |set_id: &Option<String>| set_id.as_deref() == Some(id);
        let open = self.sets.running.as_ref().is_some_and(|r| is_it(&r.set_id));
        let pending = self.sets.opening.as_ref().is_some_and(|o| is_it(&o.set_id));
        if open || (pending && self.sets.is_open()) {
            self.close_set(&CloseClusterSet, window, cx);
        } else if pending {
            self.sets.opening = None;
        }
    }

    fn open_saved(&mut self, action: &OpenClusterSet, window: &mut Window, cx: &mut Context<Self>) {
        let Some(set) = self.sets.saved.iter().find(|s| s.name == action.name) else {
            return;
        };
        let pins = set.members.clone();
        let saved = SavedOpening {
            name: set.name.clone(),
            id: set.id.clone(),
        };
        self.open_set(pins, Some(saved), window, cx);
    }

    fn save_set(&mut self, action: &SaveClusterSet, window: &mut Window, cx: &mut Context<Self>) {
        let pins = self.picked_pins(cx);
        // A set saved now must name the clusters that were chosen, on the servers they were.
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
        let name = action.name.clone();
        let input = self.sets.name_input.clone();
        self.write_sets(
            move |dir| create_cluster_set(dir, &name, &pins),
            move |_, window, cx| input.update(cx, |input, cx| input.set_value("", window, cx)),
            window,
            cx,
        );
    }

    /// Asks the core to run `pins` as the set, and opens their combined list with the members it
    /// admitted. The core decides who joins and why others can't, in one step. Too few starts
    /// nothing: whatever is open stays, and the sidebar says why.
    fn open_set(
        &mut self,
        pins: Vec<ClusterPin>,
        saved: Option<SavedOpening>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // As asked for: opening again after a question must open the same set.
        let again = (pins.clone(), saved.clone());
        if self.asks_before_leaving(window, cx, move |ws, window, cx| {
            let (pins, saved) = again.clone();
            ws.open_set(pins, saved, window, cx);
        }) {
            return;
        }
        self.sets.error = None;
        let token = self.sets.next_opening;
        self.sets.next_opening += 1;
        self.sets.opening = Some(PendingOpen {
            token,
            set_id: saved.as_ref().map(|s| s.id.clone()),
        });
        let open = self.model.read(cx).core().open_running_set(pins);
        self.sets._admit = cx.spawn_in(window, async move |this, cx| {
            let opened = open.await;
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.finish_open(token, saved, opened, window, cx)
            });
        });
    }

    /// Puts the core back on what the view shows, after a refused open that replaced one the
    /// core may already have admitted. Admission checks every member again.
    fn restore_core_set(&self, cx: &App) {
        let pins = self.sets.running.as_ref().map_or_else(Vec::new, |run| {
            run.members.iter().map(|m| m.pin().clone()).collect()
        });
        drop(self.model.read(cx).core().admit_running_set(pins));
    }

    fn too_few_text(&self, left_out: &[LeftOutMember], cx: &App) -> String {
        let names = self.cluster_names(cx);
        let shown: Vec<String> = left_out
            .iter()
            .map(|m| {
                format!(
                    "{} ({})",
                    names.exact(&m.context),
                    m.reason.label().to_lowercase()
                )
            })
            .collect();
        format!(
            "{} can't join, which leaves too few clusters to open: a set runs at least \
             {MIN_CLUSTER_SET_MEMBERS}",
            shown.join(", ")
        )
    }

    fn finish_open(
        &mut self,
        token: u64,
        saved: Option<SavedOpening>,
        opened: Result<OpenedSet, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sets.opening.as_ref().map(|o| o.token) != Some(token) {
            // Closed while the core was admitting it, and nothing newer is on its way.
            let admitted = opened.is_ok_and(|set| !set.members.is_empty());
            if self.sets.opening.is_none() && !self.sets.is_open() && admitted {
                drop(self.model.read(cx).core().admit_running_set(Vec::new()));
            }
            return;
        }
        self.sets.opening = None;
        let refusal = match &opened {
            Ok(set) if set.members.is_empty() => Some(self.too_few_text(&set.left_out, cx)),
            Ok(_) => None,
            Err(why) => Some(why.clone()),
        };
        if let Some(why) = refusal {
            self.sets.error = Some(why);
            self.restore_core_set(cx);
            cx.notify();
            return;
        }
        let Ok(OpenedSet { members, left_out }) = opened else {
            return;
        };
        let (label, set_id) = match saved {
            Some(saved) => (saved.name, Some(saved.id)),
            None => (format!("{} clusters", members.len()), None),
        };
        if set_id.is_none() {
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
        if let Some(run) = &mut self.sets.running {
            run.set_id = set_id;
            run.left_out = left_out;
        }
        self.refresh_set_kinds(cx);
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
        let evidence = self.member_kinds(&members, cx);
        let mut held = HashSet::new();
        let mut subscriptions = Vec::new();
        let mut snapshots: Vec<Arc<KindSnapshot>> = Vec::with_capacity(members.len());
        for (member, found) in members.iter().zip(&evidence) {
            if found.may_subscribe(&kind) {
                // Bound: the core starts no LIST or watch for a member that has changed.
                let subscription = core
                    .bound_to(member.clone())
                    .subscribe(member.context(), &kind);
                snapshots.push(without_rows(&subscription.peek(), SyncState::Loading));
                subscriptions.push((subscription, member.clone()));
            } else {
                held.insert(member.context().to_owned());
                snapshots.push(Arc::new(KindSnapshot {
                    cluster: member.context().to_owned(),
                    kind: kind.clone(),
                    ..KindSnapshot::default()
                }));
            }
        }
        let window_handle = window.window_handle();
        let source = self.combined_source(&snapshots, cx);
        let title = format!("{kind_label} · {label}");
        let shown_kind = kind_label;
        let list = cx.new(|cx| CombinedResourceList::new(title, source, Vec::new(), window, cx));
        let events = cx.subscribe_in(&list, window, Self::on_combined_event);
        let feeds = subscriptions
            .into_iter()
            .map(|(subscription, member)| {
                Self::member_feed(core.clone(), subscription, member, window_handle, cx)
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
        self.sets.running = Some(RunningSet {
            label,
            set_id: None,
            members,
            changed: HashSet::new(),
            held,
            window: window_handle,
            left_out: Vec::new(),
            kind: kind.clone(),
            kind_label: shown_kind,
            list,
            dashboard: None,
            snapshots,
            reports: Vec::new(),
            building: false,
            rebuild: false,
            _feeds: feeds,
            _build: Task::ready(()),
            _reports: reports,
            _list_events: events,
        });
        self.nav
            .update(cx, |nav, cx| nav.set_selected(Some(kind), cx));
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
        if !self.sets.is_open() {
            return false;
        }
        self.show_set_kind(entry.id.clone(), entry.label.clone(), window, cx);
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
        self.refresh_set_kinds(cx);
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
            self.refresh_set_kinds(cx);
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
        let summary = self.cluster_summary(cluster, cx);
        member_refusal(member.pin(), summary, report, &shown).or_else(|| {
            // The core's published state: never behind what the gate decides.
            summary
                .is_some_and(|c| self.model.read(cx).sessions_paused(c))
                .then(|| paused_text(&shown))
        })
    }

    /// Why `cluster` may be read but not written now: the core refuses writes on its definition
    /// while a kubeconfig file can't be read. Only says what the core decides.
    pub(in crate::ui) fn writes_paused_why(&self, cluster: &str, cx: &App) -> Option<String> {
        let summary = self.cluster_summary(cluster, cx)?;
        self.model
            .read(cx)
            .writes_paused(summary)
            .then(|| writes_paused_reason(&self.cluster_names(cx).exact(cluster)))
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

    /// The core a session for `member` launches through: bound to it, so the launch itself
    /// refuses a context that is no longer the definition admitted.
    pub(super) fn session_core(
        &self,
        member: Option<&AdmittedMember>,
        cx: &App,
    ) -> flint_core::CoreHandle {
        let core = self.model.read(cx).core();
        member.map_or_else(|| core.clone(), |member| core.bound_to(member.clone()))
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
        self.refresh_set_dashboard(cx);
        let Some(detail) = &self.detail else {
            return;
        };
        // An unapplied edit stays on show: the write gate refuses it while the member is changed.
        if detail.read(cx).has_unapplied_edit(cx) {
            return;
        }
        let cluster = detail.read(cx).object().cluster.clone();
        if let Some(why) = self.set_row_refusal(&cluster, cx) {
            self.close_set_detail(cx);
            self.refuse(why, cx);
        }
    }

    fn close_set_detail(&mut self, cx: &mut Context<Self>) {
        // The write gate refuses what can no longer be sent; the text stays.
        if self.unapplied_edit(cx).is_some() {
            return;
        }
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
        if cleared && self.unapplied_edit(cx).is_some() {
            // A row that left the list has nothing to undo: its edit simply stays on show.
            if list.update(cx, |list, cx| list.undo_selection(cx)) {
                let list = list.clone();
                self.asks_before_leaving(window, cx, move |_, _, cx| {
                    list.update(cx, |list, cx| list.clear_selection(cx));
                });
            }
            return;
        }
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
        if let Some(detail) = self.unapplied_edit(cx) {
            let shown = detail.read(cx).object();
            let same = shown.cluster == row.cluster
                && shown.namespace == row.namespace
                && shown.name == row.name
                && shown.uid == row.uid;
            if !same {
                let wanted = list.read(cx).selected().cloned();
                list.update(cx, |list, cx| list.undo_selection(cx));
                let list = list.clone();
                self.asks_before_leaving(window, cx, move |_, window, cx| {
                    if let Some(wanted) = &wanted {
                        list.update(cx, |list, cx| list.reselect(wanted, window, cx));
                    }
                });
                return;
            }
            // The edited object itself, whatever its label says now or however its member
            // stands: it comes back in front, and the write gate still judges the edit.
            self.list_in_front = false;
            release_unfocused_input(window, cx);
            cx.notify();
            return;
        }
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
        let server = member.server().to_owned();
        // Every read this view makes is bound to the member, in the core.
        let core = self.model.read(cx).core().bound_to(member);
        let detail = cx.new(|cx| {
            let mut view = DetailView::new(core, object, tab, window, cx);
            view.set_row_cluster(shown, server, cx);
            view
        });
        self._detail_subscriptions = vec![cx.subscribe_in(
            &detail,
            window,
            move |this, _, event, window, cx| match event {
                DetailEvent::Close => this.close_detail(window, cx),
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
        if self.asks_before_leaving(window, cx, |ws, window, cx| {
            ws.close_set(&CloseClusterSet, window, cx)
        }) {
            return;
        }
        self.close_list(cx);
        let focus = self.focus.clone();
        window.focus(&focus, cx);
        release_unfocused_input(window, cx);
        cx.notify();
    }

    /// Stops the running set, if one is open. Called wherever the list closes.
    pub(super) fn leave_set(&mut self, cx: &mut Context<Self>) {
        self.sets.opening = None;
        if let Some(run) = self.sets.running.take() {
            if run.dashboard.is_some() {
                self.nav.update(cx, |nav, cx| nav.set_overview(false, cx));
            }
            // Through the same queue as admissions, so it can't overtake a later one.
            drop(self.model.read(cx).core().admit_running_set(Vec::new()));
        }
        self.refresh_set_kinds(cx);
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
            _ if run.dashboard.is_some() => div()
                .flex_1()
                .min_h_0()
                .children(run.dashboard.clone())
                .into_any_element(),
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

    /// What the set's bar says about `context`, when something is wrong with it.
    fn set_member_note(&self, run: &RunningSet, context: &str, cx: &App) -> Option<String> {
        if run.changed.contains(context) {
            return Some("Changed".to_owned());
        }
        let summary = self.cluster_summary(context, cx);
        member_note(
            run.reports.iter().find(|r| r.cluster == context),
            summary.is_some_and(|c| self.model.read(cx).sessions_paused(c)),
            self.writes_paused_why(context, cx).is_some(),
        )
    }

    /// The sidebar's choice, the saved sets and the running set, as data.
    #[cfg(any(test, feature = "automation"))]
    pub(super) fn sets_view_state(&self, cx: &App) -> ClusterSetsState {
        let sets = &self.sets;
        let open = sets.running.as_ref().map(|run| {
            let list = run.list.read(cx);
            let shown = list.shown.list.members();
            OpenSetState {
                label: run.label.clone(),
                members: run
                    .members
                    .iter()
                    .map(|member| {
                        let cluster = member.context();
                        SetMemberState {
                            cluster: cluster.to_owned(),
                            note: self.set_member_note(run, cluster, cx),
                            tag: shown
                                .iter()
                                .find(|m| m.cluster() == cluster)
                                .and_then(|m| m.tag()),
                        }
                    })
                    .collect(),
                groups: list
                    .view
                    .groups()
                    .iter()
                    .map(|group| SetGroupState {
                        cluster: shown[group.member].cluster().to_owned(),
                        count: group.count,
                        collapsed: group.collapsed,
                    })
                    .collect(),
                left_out: run.left_out.iter().map(|m| m.context.clone()).collect(),
                list: list.view_state(),
            }
        });
        ClusterSetsState {
            picking: sets.picking,
            picked: sets.picked.iter().map(|p| p.context.clone()).collect(),
            saved: sets.saved.iter().map(|s| s.name.clone()).collect(),
            error: sets.error.clone(),
            open,
        }
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
                let note = self.set_member_note(run, context, cx);
                let noted = note.is_some();
                let text = match note {
                    Some(note) => format!("{shown}: {note}"),
                    None => shown,
                };
                div()
                    .id(SharedString::from(format!("set-member-{context}")))
                    .aria_label(text.clone())
                    .when(noted, |el| el.text_color(theme.warning))
                    .child(text)
                    .test_support()
            }))
            .children(
                run.left_out
                    .iter()
                    .map(|LeftOutMember { context, reason }| {
                        let text =
                            format!("{}: {}, left out", names.exact(context), reason.label());
                        div()
                            .id(SharedString::from(format!("set-member-{context}")))
                            .aria_label(text.clone())
                            .text_color(theme.warning)
                            .child(text)
                            .test_support()
                    }),
            )
            .child(div().flex_1())
            .child(if run.dashboard.is_some() {
                Button::new("show-set-list")
                    .xsmall()
                    .ghost()
                    .label("Show List")
                    .on_click(registry::on_click(ShowClusterSetList))
            } else {
                Button::new("open-set-dashboard")
                    .xsmall()
                    .ghost()
                    .label("Dashboard")
                    .on_click(registry::on_click(OpenClusterSetDashboard))
            })
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
                let name = set.name.clone();
                div()
                    .id(SharedString::from(format!("set-menu-{name}")))
                    .w_full()
                    .child(
                        Button::new(SharedString::from(format!("open-set-{name}")))
                            .xsmall()
                            .ghost()
                            .w_full()
                            .label(name.clone())
                            .on_click(registry::on_click(OpenClusterSet { name: name.clone() })),
                    )
                    .context_menu(move |menu, _, _| {
                        menu.item(PopupMenuItem::new("Rename…").on_click(registry::on_click(
                            RenameClusterSet { name: name.clone() },
                        )))
                        .item(PopupMenuItem::new("Delete…").on_click(registry::on_click(
                            DeleteClusterSet { name: name.clone() },
                        )))
                    })
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

#[path = "workspace_set_dashboard.rs"]
mod dashboard;
#[path = "workspace_set_kinds.rs"]
mod kinds;

#[cfg(test)]
#[path = "workspace_sets_tests.rs"]
mod tests;
