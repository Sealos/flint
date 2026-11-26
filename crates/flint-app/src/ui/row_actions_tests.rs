//! View tests for Logs, Previous Logs and Shell on pod and workload rows (UP-47): from a row's
//! context menu, from the keys and through the registry, over the whole window
//! (`support_tests`). The test servers never answer, so the tests hand the detail pane its
//! object and feed log tabs their lines; a workload's pods are listed through [`FakePods`],
//! which the tests answer by hand, so the pane's own list requests run.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use flint_core::logs::LogBuffer;
use flint_core::{
    Cell, Column, ColumnRole, ColumnType, ConnectionState, Freshness, KindId, KindSnapshot,
    LineSource, LogLine, LogSnapshot, LogState, NavEntry, ObjectDetail, PodSet, PodSetStatus, Row,
    SafeErrorText, StreamStatus, SyncState, WorkloadPod,
};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, DismissEvent, Focusable as _, SharedString, TestAppContext};
use serde_json::{Value, json};

use crate::ui::detail::{PENDING_TIMEOUT, TestPodLister};
use crate::ui::dock::DockTab;
use crate::ui::list::{ListEvent, Remembered, ResourceList, Selected};
use crate::ui::registry::{self, ActionError};
use crate::ui::row_menu::{OpenRowMenu, RowMenu};
use crate::ui::support_tests::{TestWorkspace, WAIT, open_empty_workspace, open_workspace, pods};
use crate::ui::view_state::{ActionAvailability, AppState, Focus};

pub(super) async fn activate(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &'static str) {
    dispatch(
        cx,
        t,
        "workspace::SelectCluster",
        json!({ "cluster": cluster }),
    )
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some(cluster)
    })
    .await;
}

pub(super) fn dispatch(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    name: &str,
    args: Value,
) -> Result<(), ActionError> {
    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch(name, args, window, cx)
    })
    .unwrap()
}

pub(super) fn draw(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.run_until_parked();
    cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

/// Opens a list of `entry`'s kind on the active cluster with `snapshot`, fed by hand, and waits
/// for `first`'s row.
pub(super) async fn open_list(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    entry: NavEntry,
    snapshot: Arc<KindSnapshot>,
    first: &str,
) {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| {
            ws.open_kind(&entry, window, cx);
            let list = ws.list.clone().unwrap();
            list.update(cx, |list, cx| {
                list.stop_watching();
                list.set_snapshot(snapshot, cx);
            });
        })
    })
    .unwrap();
    let row = SharedString::from(format!("row-uid-{first}"));
    cx.wait_for(t.window, WAIT, move |window, _| {
        window.try_find(row.clone()).is_some()
    })
    .await;
}

pub(super) fn pods_entry() -> NavEntry {
    NavEntry {
        id: KindId::pods(),
        label: "Pods".into(),
        kind: "Pod".into(),
        api_version: "v1".into(),
        namespaced: true,
        listable: true,
        watchable: true,
    }
}

fn deployments_entry() -> NavEntry {
    NavEntry {
        id: KindId::new("apps", "deployments"),
        label: "Deployments".into(),
        kind: "Deployment".into(),
        api_version: "apps/v1".into(),
        namespaced: true,
        listable: true,
        watchable: true,
    }
}

/// A Deployments list of `names` in `demo`, synced.
fn deployments(names: &[&str]) -> Arc<KindSnapshot> {
    let column = |name: &str, role| Column {
        name: name.into(),
        ty: ColumnType::String,
        format: "".into(),
        description: "".into(),
        priority: 0,
        role,
    };
    let rows = names
        .iter()
        .map(|name| {
            Arc::new(Row {
                uid: format!("uid-{name}").into(),
                name: (*name).into(),
                namespace: Some("demo".into()),
                resource_version: "1".into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: Box::new([Cell::Name, Cell::Namespace]),
            })
        })
        .collect();
    Arc::new(KindSnapshot {
        cluster: "kind-alpha".into(),
        kind: KindId::new("apps", "deployments"),
        namespaced: true,
        columns: vec![
            column("Name", ColumnRole::Name),
            column("Namespace", ColumnRole::Namespace),
        ]
        .into(),
        rows,
        state: SyncState::Synced,
        version: 1,
        ..Default::default()
    })
}

/// Pod `name` as fetched: containers `app` (restarted once, so it has previous logs) and
/// `sidecar`.
pub(super) fn pod_detail(name: &str) -> ObjectDetail {
    detail(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {"name": name, "namespace": "demo", "uid": format!("uid-{name}")},
        "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
        "status": {"containerStatuses": [
            {"name": "app", "restartCount": 1, "lastState": {"terminated": {"exitCode": 1}}},
            {"name": "sidecar", "restartCount": 0}
        ]}
    }))
}

fn detail(object: Value) -> ObjectDetail {
    ObjectDetail {
        object,
        yaml: String::new(),
        managed_fields: None,
        events: Vec::new(),
        events_truncated: false,
        events_error: None,
    }
}

/// Shows `object` in the detail pane, as if its fetch had returned.
pub(super) fn show_fetched(cx: &mut TestAppContext, t: &TestWorkspace, object: ObjectDetail) {
    let detail = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    cx.update_window(t.window, |_, window, cx| {
        detail.update(cx, |d, cx| d.show_fetched(object, window, cx))
    })
    .unwrap();
    draw(cx, t);
}

/// Shows the detail pane's object as still loading, as on a slow server.
fn show_loading(cx: &mut TestAppContext, t: &TestWorkspace) {
    let detail = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail.update(cx, |d, cx| d.show_loading(cx));
    draw(cx, t);
}

type Listed = Result<Vec<WorkloadPod>, String>;

/// Pod lists the detail pane asks for, answered by hand.
#[derive(Clone, Default)]
struct FakePods(Rc<RefCell<Vec<Asked>>>);

struct Asked {
    pods: PodSet,
    freshness: Freshness,
    /// `None` once answered.
    reply: Option<async_channel::Sender<Listed>>,
}

impl Asked {
    fn waits(&self) -> bool {
        self.reply.as_ref().is_some_and(|r| r.receiver_count() > 0)
    }
}

impl FakePods {
    /// Answers the pane's pod lists from now on.
    fn install(cx: &mut TestAppContext) -> Self {
        let fake = FakePods::default();
        let asked = fake.0.clone();
        cx.update(|cx| {
            cx.set_global(TestPodLister(Rc::new(move |_, _, pods, freshness| {
                let (reply, answer) = async_channel::bounded(1);
                asked.borrow_mut().push(Asked {
                    pods,
                    freshness,
                    reply: Some(reply),
                });
                Box::pin(async move {
                    answer
                        .recv()
                        .await
                        .unwrap_or_else(|_| Err("not answered".into()))
                })
            })))
        });
        fake
    }

    /// The lists asked for and not answered yet, oldest first, whose asker still waits.
    fn waiting(&self) -> Vec<Freshness> {
        self.0
            .borrow()
            .iter()
            .filter(|a| a.waits())
            .map(|a| a.freshness)
            .collect()
    }

    /// How many lists were asked for in all.
    fn asked(&self) -> usize {
        self.0.borrow().len()
    }

    /// What the last list asked for selected.
    fn last_pods(&self) -> PodSet {
        self.0.borrow().last().unwrap().pods.clone()
    }

    /// Answers the oldest list still waiting, and draws.
    fn answer(&self, cx: &mut TestAppContext, t: &TestWorkspace, listed: Listed) {
        let ix = self
            .0
            .borrow()
            .iter()
            .position(Asked::waits)
            .expect("a list is waiting");
        let reply = self.0.borrow_mut()[ix].reply.take().unwrap();
        reply.try_send(listed).unwrap();
        draw(cx, t);
    }
}

/// Lets the delay pass after which a selected workload's pods are listed for showing.
fn let_pods_list(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.executor()
        .advance_clock(crate::ui::detail::SHOW_PODS_DELAY);
    draw(cx, t);
}

/// The window's state, as the automation channel reports it.
pub(super) fn app_state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

/// The detail pane's Logs, Previous Logs and Shell, as the state reports them.
fn actions(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<ActionAvailability> {
    app_state(cx, t).detail.unwrap().actions
}

fn available(action: &'static str) -> ActionAvailability {
    ActionAvailability {
        action,
        enabled: true,
        reason: None,
    }
}

fn unavailable(action: &'static str, reason: &str) -> ActionAvailability {
    ActionAvailability {
        action,
        enabled: false,
        reason: Some(reason.into()),
    }
}

/// Why the last Logs or Shell did not open, as shown over the dock.
fn refusal(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    app_state(cx, t)
        .banners
        .into_iter()
        .find(|b| b.id == "action-error")
        .map(|b| b.text)
}

/// Whether the detail pane's Shell button is drawn disabled (`None`: not drawn).
fn shell_button_disabled(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<bool> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        window
            .try_find("detail-shell")
            .map(|b| b.disabled() == Some(true))
    })
    .unwrap()
}

fn workload_pod(name: &str, ready: bool, containers: &[&str]) -> WorkloadPod {
    WorkloadPod {
        name: name.into(),
        ready,
        containers: containers.iter().map(|c| (*c).to_owned()).collect(),
        default_container: containers.first().map(|c| (*c).to_owned()),
    }
}

/// Right-clicks the row of `name` and waits for its menu.
pub(super) async fn right_click(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) {
    let row = SharedString::from(format!("row-uid-{name}"));
    cx.update_window(t.window, |_, window, cx| window.right_click(row, cx))
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("popup-menu").is_some()
    })
    .await;
}

/// The open menu's rows: its entries and the labels under disabled ones that say why. (A
/// menu row does not report whether it is disabled: the tests click a disabled entry and
/// check that nothing opened and nothing was refused, and read the state's `actions`.)
pub(super) fn menu(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        let mut menu = window.within("popup-menu");
        let mut rows = Vec::new();
        for ix in 0usize.. {
            let Some(item) = menu.try_find(ix) else {
                break;
            };
            // A note that wraps carries its text as the value of the label inside the item.
            let label = item.label().map(str::to_owned).or_else(|| {
                let note = menu.within(ix).try_find(crate::ui::menu_fit::MENU_NOTE)?;
                note.value().map(str::to_owned)
            });
            rows.push(label.unwrap_or_default());
        }
        rows
    })
    .unwrap()
}

/// Opens the context menu of `name`'s row and returns its rows.
async fn open_menu(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) -> Vec<String> {
    right_click(cx, t, name).await;
    menu(cx, t)
}

/// Clicks the open menu's entry `ix`.
fn click_entry(cx: &mut TestAppContext, t: &TestWorkspace, ix: usize) {
    cx.update_window(t.window, |_, window, cx| {
        window.within("popup-menu").click(ix, cx)
    })
    .unwrap();
    draw(cx, t);
}

/// The open submenu's entries, after its "Container" label.
fn submenu(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        let mut sub = window.within("submenu");
        let mut rows = Vec::new();
        for ix in 1usize.. {
            let Some(item) = sub.try_find(ix) else {
                break;
            };
            // A row that wraps carries its text as the value of the label inside the item.
            let label = item.label().map(str::to_owned).or_else(|| {
                let text = sub.within(ix).try_find(crate::ui::menu_fit::MENU_NOTE)?;
                text.value().map(str::to_owned)
            });
            rows.push(label.unwrap_or_default());
        }
        rows
    })
    .unwrap()
}

/// Clicks the open submenu's entry `ix` (1 is the first container).
fn click_submenu(cx: &mut TestAppContext, t: &TestWorkspace, ix: usize) {
    cx.update_window(t.window, |_, window, cx| {
        window.within("submenu").click(ix, cx)
    })
    .unwrap();
    draw(cx, t);
}

/// The dock: its tabs' titles, left to right, and the active one's position.
fn dock_tabs(cx: &mut TestAppContext, t: &TestWorkspace) -> (Vec<String>, Option<usize>) {
    t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        let ids = dock.tab_ids();
        let titles = ids
            .iter()
            .map(|id| dock.tab(*id).unwrap().item(cx).tab_title().to_string())
            .collect();
        let active = ids.iter().position(|id| Some(*id) == dock.active_id());
        (titles, active)
    })
}

pub(super) fn press(cx: &mut TestAppContext, t: &TestWorkspace, key: &str) {
    cx.update_window(t.window, |_, window, cx| window.press(key, cx))
        .unwrap();
    cx.run_until_parked();
}

fn focus_list(cx: &mut TestAppContext, t: &TestWorkspace) {
    let focus = t.workspace.read_with(cx, |ws, cx| {
        ws.list.as_ref().unwrap().read(cx).focus_handle().clone()
    });
    cx.update_window(t.window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    cx.run_until_parked();
}

// covers UP-47, UP-40, UP-41: a pod row's context menu selects the row and offers Logs,
// Previous Logs and Shell; chosen before the pod has loaded, an entry waits for it; once it
// has, Shell becomes a container picker. Repeating Logs shows the open tab.
#[gpui_kit::test]
async fn a_pod_rows_menu_opens_logs_previous_logs_and_a_shell(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;

    // No row selected: nothing to act on.
    assert_eq!(
        dispatch(cx, &t, "detail::OpenLogs", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenLogs".into()))
    );

    right_click(cx, &t, "web").await;
    let selected = t.workspace.read_with(cx, |ws, cx| {
        ws.list.as_ref().unwrap().read(cx).selected_name()
    });
    assert_eq!(selected.as_deref(), Some("web"), "a right-click selects");
    press(cx, &t, "escape");
    // The pod is still loading (on a slow server): the entries wait for it, but a forward needs
    // its uid first.
    show_loading(cx, &t);
    right_click(cx, &t, "web").await;
    assert_eq!(
        menu(cx, &t),
        [
            "Logs",
            "Previous Logs",
            "Shell",
            "Forward Port",
            "web is still loading"
        ]
    );
    click_entry(cx, &t, 0);
    assert_eq!(
        dock_tabs(cx, &t).0,
        Vec::<String>::new(),
        "waits for the pod"
    );
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t), (vec!["web · app".to_owned()], Some(0)));

    // Loaded, with two containers: each entry picks one, the default marked. Previous Logs
    // lists the one that restarted.
    right_click(cx, &t, "web").await;
    assert_eq!(
        menu(cx, &t),
        ["Logs", "Previous Logs", "Shell", "Forward Port"]
    );
    click_entry(cx, &t, 1);
    assert_eq!(submenu(cx, &t), ["app (default)"]);
    click_submenu(cx, &t, 1);
    assert_eq!(
        dock_tabs(cx, &t),
        (
            vec!["web · app".to_owned(), "web · app (previous)".to_owned()],
            Some(1)
        )
    );
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 2);
    assert_eq!(submenu(cx, &t), ["app (default)", "sidecar"]);
    click_submenu(cx, &t, 2);
    assert_eq!(dock_tabs(cx, &t).0[2], "Shell: web · sidecar");

    // Logs again from the menu shows the open tab, not a second one; so does Previous Logs
    // (Shift-Cmd-L) from the list.
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 0);
    assert_eq!(submenu(cx, &t), ["app (default)", "sidecar"]);
    click_submenu(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).1, Some(0));
    assert_eq!(dock_tabs(cx, &t).0.len(), 3);
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-l");
    let (titles, active) = dock_tabs(cx, &t);
    assert_eq!((titles.len(), active), (3, Some(1)));
}

// covers UP-47, UP-40, UP-41: on a pod with several containers the menu picks one, the default
// marked, while the keys open the default container at once: the one the
// `kubectl.kubernetes.io/default-container` annotation names, here the second.
#[gpui_kit::test]
async fn keys_open_the_default_container_and_the_menu_picks(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(
        cx,
        &t,
        detail(json!({
            "kind": "Pod",
            "metadata": {
                "name": "web", "namespace": "demo", "uid": "uid-web",
                "annotations": {"kubectl.kubernetes.io/default-container": "sidecar"}
            },
            "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
            "status": {"containerStatuses": [
                {"name": "app", "restartCount": 2, "lastState": {"terminated": {"exitCode": 1}}},
                {"name": "sidecar", "restartCount": 0}
            ]}
        })),
    );
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-e");
    assert_eq!(
        dock_tabs(cx, &t).0,
        ["web · sidecar", "Shell: web · sidecar"]
    );
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 0);
    assert_eq!(submenu(cx, &t), ["app", "sidecar (default)"]);
    click_submenu(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0[2], "web · app");
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 2);
    assert_eq!(submenu(cx, &t), ["app", "sidecar (default)"]);
    click_submenu(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0[3], "Shell: web · app");
    // The default container never restarted: the key says so, while the menu offers the one
    // that did.
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-l");
    assert_eq!(
        refusal(cx, &t).as_deref(),
        Some(
            "Can't open the previous logs: container sidecar has not restarted, so it has no \
             previous logs."
        )
    );
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 1);
    assert_eq!(submenu(cx, &t), ["app"]);
    click_submenu(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0[4], "web · app (previous)");
}

// covers UP-47, UP-40: keys and the registry act on the selected row too; an entry that does
// not apply is disabled.
#[gpui_kit::test]
async fn keys_act_on_the_selected_pod_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(
        cx,
        &t,
        pods_entry(),
        pods(&[("demo", "api", 0), ("demo", "web", 1)]),
        "api",
    )
    .await;
    let api = || {
        detail(json!({
            "kind": "Pod",
            "metadata": {"name": "api", "namespace": "demo", "uid": "uid-api"},
            "spec": {"containers": [{"name": "app"}]},
            "status": {"containerStatuses": [{"name": "app", "restartCount": 0}]}
        }))
    };
    focus_list(cx, &t);
    press(cx, &t, "down");
    show_loading(cx, &t);
    // It never restarted: Previous Logs in its menu is disabled, saying why.
    right_click(cx, &t, "api").await;
    show_fetched(cx, &t, api());
    let why = "container app has not restarted, so it has no previous logs";
    assert_eq!(
        menu(cx, &t),
        [
            "Logs".to_owned(),
            "Previous Logs".to_owned(),
            why.to_owned(),
            "Shell".to_owned(),
            "Forward Port".to_owned()
        ]
    );
    assert_eq!(
        actions(cx, &t),
        [
            available("Logs"),
            unavailable("Previous Logs", why),
            available("Shell")
        ]
    );
    click_entry(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
    assert_eq!(refusal(cx, &t), None, "a disabled entry does not run");
    press(cx, &t, "escape");
    // Cmd-L before the pod has loaded opens its logs once it has.
    show_loading(cx, &t);
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
    show_fetched(cx, &t, api());
    assert_eq!(dock_tabs(cx, &t).0, ["api · app"]);
    assert_eq!(
        dispatch(cx, &t, "detail::OpenPreviousLogs", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenPreviousLogs".into()))
    );
    // Shift-Cmd-E opens a shell in the only container.
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-e");
    assert_eq!(dock_tabs(cx, &t).0, ["api · app", "Shell: api · app"]);
    // Keys reach the action even when it cannot run: it says why.
    assert_eq!(refusal(cx, &t), None);
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-l");
    // The strip names what was refused, then why, as a sentence.
    assert_eq!(
        refusal(cx, &t),
        Some(format!("Can't open the previous logs: {why}."))
    );
    assert_eq!(dock_tabs(cx, &t).0.len(), 2);
}

// covers UP-45, UP-47: Logs on a Deployment row opens one tab for every pod, titled with the
// kind and name; it shows each pod's lines prefixed and counts the streams. Asking again shows
// the same tab.
#[gpui_kit::test]
async fn a_workload_rows_logs_merge_its_pods(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    // One wording for Logs, on pods and workloads alike: the menu, the pane and the palette.
    assert_eq!(
        open_menu(cx, &t, "web").await,
        ["Logs", "Shell", "", "Scale…", "Rollout Restart…"]
    );
    assert_eq!(registry::label::<registry::OpenLogs>(), "Open Logs");
    click_entry(cx, &t, 0);
    // The pane's button, drawn once the menu has closed.
    draw(cx, &t);
    let button = cx
        .update_window(t.window, |_, window, _| {
            window.find("detail-logs").label().map(str::to_owned)
        })
        .unwrap();
    assert_eq!(button.as_deref(), Some("Logs"));
    assert_eq!(
        dock_tabs(cx, &t),
        (vec!["web (Deployment)".to_owned()], Some(0))
    );

    // The tab's session, fed by hand: two pods' lines, merged.
    let view = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).active_log().unwrap().clone());
    let mut buffer = LogBuffer::new(100);
    for (pod, text) in [
        ("web-1", "GET /a"),
        ("web-2", "GET /b"),
        ("web-1", "GET /c"),
    ] {
        let mut line = LogLine::parse(&format!("2024-05-01T12:00:00Z {text}"), false);
        line.source = Some(Arc::new(LineSource::new(pod, "app")));
        buffer.push(line);
    }
    let stream = |pod: &str| StreamStatus {
        source: Arc::new(LineSource::new(pod, "app")),
        state: LogState::Streaming,
    };
    let status = PodSetStatus {
        selector: Some("app=web".into()),
        pods: 2,
        streams: vec![stream("web-1"), stream("web-2")],
        max_streams: 32,
        ..Default::default()
    };
    view.update(cx, |v, cx| {
        v.stop_streaming();
        v.set_snapshot(
            Arc::new(LogSnapshot {
                lines: buffer.view(),
                state: LogState::Streaming,
                paused: false,
                held_back: 0,
                version: 1,
                pods: Some(Arc::new(status)),
                trimmed: false,
            }),
            cx,
        );
    });
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("log-streams").is_some() && window.try_find(("log-line", 2usize)).is_some()
    })
    .await;
    press(cx, &t, "secondary-a");
    press(cx, &t, "secondary-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(
        copied,
        "[web-1/app] GET /a\n[web-2/app] GET /b\n[web-1/app] GET /c"
    );

    // Logs again, from the menu or Cmd-L in the list: the same tab.
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 0);
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    assert_eq!(
        dock_tabs(cx, &t),
        (vec!["web (Deployment)".to_owned()], Some(0))
    );
    // A workload has no previous logs of its own.
    assert_eq!(
        dispatch(cx, &t, "detail::OpenPreviousLogs", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenPreviousLogs".into()))
    );
}

// covers UP-47, UP-41: Shell on a workload opens in the first ready pod (by name) of a list
// it asks for when chosen, never of the one shown; with no ready pod it is disabled, saying
// why, in the menu, the state and the pane. A menu opened before the pods are listed lists
// them and is rebuilt once they are, naming the pod.
#[gpui_kit::test]
async fn a_workload_rows_shell_opens_in_a_ready_pod(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    let_pods_list(cx, &t);
    assert_eq!(pods.waiting(), [Freshness::Cached]);
    pods.answer(cx, &t, Ok(vec![workload_pod("web-a", false, &["app"])]));
    let why = "no pod of Deployment web is ready";
    assert_eq!(
        dispatch(cx, &t, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into())),
    );
    assert_eq!(
        actions(cx, &t),
        [available("Logs"), unavailable("Shell", why)]
    );
    assert_eq!(shell_button_disabled(cx, &t), Some(true));
    assert_eq!(
        open_menu(cx, &t, "web").await,
        ["Logs", "Shell", why, "", "Scale…", "Rollout Restart…"]
    );
    assert_eq!(pods.asked(), 1, "the pods listed still hold");
    click_entry(cx, &t, 1);
    assert_eq!(refusal(cx, &t), None, "a disabled entry does not run");
    assert_eq!(pods.waiting(), Vec::<Freshness>::new());
    press(cx, &t, "escape");

    // The Deployment changed: the pods listed no longer hold, so Shell is enabled again; they
    // are listed again once its menu opens, and the menu is rebuilt when they are.
    let mut changed = (*deployments(&["web"])).clone();
    let mut row = (*changed.rows[0]).clone();
    row.resource_version = "2".into();
    changed.rows = vec![Arc::new(row)];
    feed(cx, &t, Arc::new(changed)).await;
    assert_eq!(
        pods.waiting(),
        Vec::<Freshness>::new(),
        "not on every change"
    );
    assert_eq!(
        open_menu(cx, &t, "web").await,
        ["Logs", "Shell", "", "Scale…", "Rollout Restart…"]
    );
    assert_eq!(pods.waiting(), [Freshness::Cached]);
    pods.answer(
        cx,
        &t,
        Ok(vec![
            workload_pod("web-a", false, &["app"]),
            workload_pod("web-c", true, &["app"]),
            workload_pod("web-b", true, &["app"]),
        ]),
    );
    assert_eq!(
        menu(cx, &t),
        ["Logs", "Shell in web-b", "", "Scale…", "Rollout Restart…"]
    );
    assert_eq!(shell_button_disabled(cx, &t), Some(false));

    // Chosen, Shell lists the pods once more, as they are now: web-b went meanwhile.
    click_entry(cx, &t, 1);
    assert_eq!(pods.waiting(), [Freshness::Current]);
    assert_eq!(
        dock_tabs(cx, &t).0,
        Vec::<String>::new(),
        "waits for the list"
    );
    pods.answer(
        cx,
        &t,
        Ok(vec![workload_pod("web-c", true, &["app", "proxy"])]),
    );
    assert_eq!(dock_tabs(cx, &t).0, ["Shell: web-c · app"]);
    let request = t.workspace.read_with(cx, |ws, cx| {
        ws.dock
            .read(cx)
            .active_terminal()
            .unwrap()
            .read(cx)
            .request()
            .clone()
    });
    assert_eq!(
        (request.namespace.as_str(), request.pod.as_str()),
        ("demo", "web-c")
    );

    // A container, through the registry: it waits for the list too.
    dispatch(
        cx,
        &t,
        "detail::OpenContainerShell",
        json!({"container": "proxy"}),
    )
    .unwrap();
    assert_eq!(pods.waiting(), [Freshness::Current]);
    pods.answer(
        cx,
        &t,
        Ok(vec![workload_pod("web-c", true, &["app", "proxy"])]),
    );
    assert_eq!(
        dock_tabs(cx, &t).0,
        ["Shell: web-c · app", "Shell: web-c · proxy"]
    );
    // One the pod lacks is refused at once.
    assert_eq!(
        dispatch(
            cx,
            &t,
            "detail::OpenContainerShell",
            json!({"container": "nope"})
        ),
        Err(ActionError::Rejected {
            action: "detail::OpenContainerShell".into(),
            reason: "pod web-c has no app container nope".into()
        })
    );
    // One gone by the time the pods are listed again is refused then, saying why.
    dispatch(
        cx,
        &t,
        "detail::OpenContainerShell",
        json!({"container": "proxy"}),
    )
    .unwrap();
    pods.answer(cx, &t, Ok(vec![workload_pod("web-c", true, &["app"])]));
    assert_eq!(dock_tabs(cx, &t).0.len(), 2);
    assert_eq!(
        refusal(cx, &t).as_deref(),
        Some("Can't open a shell: pod web-c has no app container proxy.")
    );
}

// covers UP-47: a workload's pods are listed lazily and once at a time: not when it is
// selected in passing, not on each change, one list joining another, and a list nobody waits
// for any more is dropped (which aborts its request). The selector comes from the fetched
// workload when there is one.
#[gpui_kit::test]
async fn pod_lists_are_lazy_joined_and_dropped(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(
        cx,
        &t,
        deployments_entry(),
        deployments(&["api", "db", "web"]),
        "api",
    )
    .await;
    // Moving through the rows (key repeat) lists nothing for the rows passed over.
    for name in ["api", "db", "web"] {
        select_row(cx, &t, name);
    }
    assert_eq!(pods.asked(), 0);
    let_pods_list(cx, &t);
    assert_eq!(pods.waiting(), [Freshness::Cached]);
    assert_eq!(
        pods.last_pods(),
        PodSet::Workload {
            kind: flint_core::WorkloadKind::Deployment,
            name: "web".into()
        },
        "not fetched yet: the workload is read for its selector"
    );
    // The menu and Shell join the list running; Shell's current list starts after it.
    right_click(cx, &t, "web").await;
    press(cx, &t, "escape");
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    assert_eq!(pods.waiting(), [Freshness::Cached]);
    pods.answer(cx, &t, Ok(vec![workload_pod("web-a", true, &["app"])]));
    assert_eq!(pods.waiting(), [Freshness::Current]);
    // Another row: the list for web is dropped.
    select_row(cx, &t, "db");
    assert_eq!(pods.waiting(), Vec::<Freshness>::new());
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());

    // Fetched, the workload's own selector is used.
    show_fetched(
        cx,
        &t,
        detail(json!({
            "kind": "Deployment",
            "metadata": {"name": "db", "namespace": "demo"},
            "spec": {"selector": {"matchLabels": {"app": "db"}}}
        })),
    );
    let_pods_list(cx, &t);
    assert_eq!(pods.last_pods(), PodSet::Selector("app=db".into()));
}

// covers UP-47, UP-41: a list that fails when Shell asks for it refuses Shell with its error;
// Shell never falls back on the pods listed before. Shell stays enabled (the next one lists
// again), and the pane's button says the list failed.
#[gpui_kit::test]
async fn a_failed_pod_list_refuses_shell_saying_why(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    let_pods_list(cx, &t);
    pods.answer(cx, &t, Ok(vec![workload_pod("web-a", true, &["app"])]));
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    pods.answer(cx, &t, Err("pods is forbidden".into()));
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new(), "not into web-a");
    let why = "the pods of Deployment web could not be listed: pods is forbidden";
    assert_eq!(refusal(cx, &t), Some(format!("Can't open a shell: {why}.")));
    assert_eq!(actions(cx, &t), [available("Logs"), available("Shell")]);
    assert_eq!(shell_button_disabled(cx, &t), Some(false));
}

// covers UP-06, UP-47: a pod list that failed is listed again when the kind's same rows are
// published again, as they are when a pause of the cluster ends, and not on a change of rows.
#[gpui_kit::test]
async fn a_failed_pod_list_is_retried_when_the_same_rows_are_published(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    let kind = deployments(&[]).kind.clone();
    let publish = |revision| {
        core.store_for_tests(flint_core::KindSnapshot {
            cluster: "kind-alpha".into(),
            kind: kind.clone(),
            state: flint_core::SyncState::Synced,
            revision,
            ..Default::default()
        })
    };
    let _first = publish(7);
    draw(cx, &t);
    show_fetched(
        cx,
        &t,
        detail(json!({
            "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "demo"},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}
        })),
    );
    let_pods_list(cx, &t);
    pods.answer(cx, &t, Err("\"kind-alpha\" is not connected".into()));
    assert!(pods.waiting().is_empty());

    let _changed = publish(8);
    draw(cx, &t);
    cx.executor()
        .advance_clock(crate::ui::detail::SHOW_PODS_DELAY * 10);
    draw(cx, &t);
    assert!(pods.waiting().is_empty(), "a change of rows does not retry");

    let _again = publish(8);
    draw(cx, &t);
    cx.executor()
        .advance_clock(crate::ui::detail::SHOW_PODS_DELAY * 10);
    draw(cx, &t);
    assert_eq!(pods.waiting(), [Freshness::Cached], "listed once more");
    pods.answer(cx, &t, Err("\"kind-alpha\" is not connected".into()));
    cx.executor()
        .advance_clock(crate::ui::detail::SHOW_PODS_DELAY * 40);
    draw(cx, &t);
    assert!(pods.waiting().is_empty(), "a failure does not loop");
}

// covers UP-06, UP-47: a namespace-limited list has no revision of its own, and its republish
// retries a failed pod list too.
#[gpui_kit::test]
async fn a_failed_pod_list_is_retried_on_a_limited_list_republish(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    let core = t.workspace.read_with(cx, |ws, cx| ws.core(cx).clone());
    let kind = deployments(&[]).kind.clone();
    let publish = |revision| {
        core.store_for_tests(flint_core::KindSnapshot {
            cluster: "kind-alpha".into(),
            kind: kind.clone(),
            state: flint_core::SyncState::Synced,
            limited_to: Some(Arc::new(flint_core::NamespaceLimit {
                listed: vec!["demo".into()],
                revisions: vec![revision],
                ..Default::default()
            })),
            ..Default::default()
        })
    };
    let _first = publish(7);
    draw(cx, &t);
    show_fetched(
        cx,
        &t,
        detail(json!({
            "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "demo"},
            "spec": {"selector": {"matchLabels": {"app": "web"}}}
        })),
    );
    let_pods_list(cx, &t);
    pods.answer(cx, &t, Err("\"kind-alpha\" is not connected".into()));

    let _changed = publish(8);
    draw(cx, &t);
    cx.executor()
        .advance_clock(crate::ui::detail::SHOW_PODS_DELAY * 10);
    draw(cx, &t);
    assert!(pods.waiting().is_empty(), "a change of rows does not retry");

    let _again = publish(8);
    draw(cx, &t);
    cx.executor()
        .advance_clock(crate::ui::detail::SHOW_PODS_DELAY * 10);
    draw(cx, &t);
    assert_eq!(pods.waiting(), [Freshness::Cached], "listed once more");
}

// covers UP-47, UP-41: a container shell asked for before the pod has loaded waits for it, as
// Shell does; the guard and the handler agree on a deleted pod.
#[gpui_kit::test]
async fn a_container_shell_waits_for_the_pod(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_loading(cx, &t);
    dispatch(
        cx,
        &t,
        "detail::OpenContainerShell",
        json!({"container": "sidecar"}),
    )
    .unwrap();
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t).0, ["Shell: web · sidecar"]);

    feed(cx, &t, pods(&[])).await;
    assert_eq!(
        dispatch(
            cx,
            &t,
            "detail::OpenContainerShell",
            json!({"container": "app"})
        ),
        Err(ActionError::NotAvailable(
            "detail::OpenContainerShell".into()
        ))
    );
}

// covers UP-47, UP-41: a pod that finished, or is being deleted, takes no shell: Shell is
// disabled, saying why.
#[gpui_kit::test]
async fn a_finished_pod_has_no_shell(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "job-1", 0)]), "job-1").await;
    select_row(cx, &t, "job-1");
    let pod = |extra: Value| {
        let mut object = json!({
            "kind": "Pod",
            "metadata": {"name": "job-1", "namespace": "demo"},
            "spec": {"containers": [{"name": "app"}]},
            "status": {"containerStatuses": [{"name": "app", "restartCount": 0}]}
        });
        for (path, value) in extra.as_object().unwrap() {
            let (section, field) = path.split_once('.').unwrap();
            object[section][field] = value.clone();
        }
        detail(object)
    };
    for (extra, why) in [
        (
            json!({"status.phase": "Succeeded"}),
            "pod job-1 has finished (Succeeded)",
        ),
        (
            json!({"status.phase": "Failed"}),
            "pod job-1 has finished (Failed)",
        ),
        (
            json!({"metadata.deletionTimestamp": "2024-05-01T12:00:00Z"}),
            "pod job-1 is being deleted",
        ),
    ] {
        show_fetched(cx, &t, pod(extra));
        assert_eq!(
            dispatch(cx, &t, "detail::OpenShell", Value::Null),
            Err(ActionError::NotAvailable("detail::OpenShell".into()))
        );
        assert_eq!(actions(cx, &t)[2], unavailable("Shell", why));
        assert_eq!(shell_button_disabled(cx, &t), Some(true));
    }
    show_fetched(cx, &t, pod(json!({"status.phase": "Running"})));
    assert_eq!(actions(cx, &t)[2], available("Shell"));
}

// covers UP-47: an open menu follows what it shows: built again whenever the row's object
// changes, not only once after loading.
#[gpui_kit::test]
async fn an_open_menu_follows_its_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(
        open_menu(cx, &t, "web").await,
        ["Logs", "Previous Logs", "Shell", "Forward Port"]
    );
    // The pod finished while the menu was open.
    let mut finished = pod_detail("web");
    finished.object["status"]["phase"] = json!("Succeeded");
    show_fetched(cx, &t, finished);
    assert_eq!(
        menu(cx, &t),
        [
            "Logs",
            "Previous Logs",
            "Shell",
            "pod web has finished (Succeeded)",
            "Forward Port",
            "pod web has finished (Succeeded)"
        ]
    );
}

// covers UP-47: right-clicking a row other than the selected one selects it, and the menu acts
// on it.
#[gpui_kit::test]
async fn right_clicking_another_row_acts_on_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(
        cx,
        &t,
        pods_entry(),
        pods(&[("demo", "api", 0), ("demo", "web", 1)]),
        "api",
    )
    .await;
    select_row(cx, &t, "api");
    show_fetched(cx, &t, pod_detail("api"));
    right_click(cx, &t, "web").await;
    let selected = t.workspace.read_with(cx, |ws, cx| {
        ws.list.as_ref().unwrap().read(cx).selected_name()
    });
    assert_eq!(selected.as_deref(), Some("web"));
    show_fetched(cx, &t, pod_detail("web"));
    click_entry(cx, &t, 0);
    click_submenu(cx, &t, 1);
    assert_eq!(dock_tabs(cx, &t).0, ["web · app"]);
}

// covers #496: the first right-click opens the detail pane beside the list, and the object is
// still loading. The menu stays open through those frames until it is dismissed.
#[gpui_kit::test]
async fn a_row_menu_stays_open_while_its_object_loads(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(
        cx,
        &t,
        deployments_entry(),
        deployments(&["api", "web"]),
        "web",
    )
    .await;
    draw(cx, &t);
    // Near the row's right edge, where the detail pane opens.
    cx.update_window(t.window, |_, window, cx| {
        let row = window.find("row-uid-web").bounds();
        let at = gpui_kit::point(row.size.width - gpui_kit::px(8.), row.size.height / 2.);
        window.click_with_options(
            "row-uid-web",
            gpui_kit::test::ClickOptions::new()
                .with_offset(at)
                .with_button(gpui_kit::MouseButton::Right),
            cx,
        );
    })
    .unwrap();
    show_loading(cx, &t);
    for frame in 0..4 {
        draw(cx, &t);
        let open = cx
            .update_window(t.window, |_, window, _| {
                window.try_find("popup-menu").is_some()
            })
            .unwrap();
        assert!(open, "the menu closed by frame {frame}");
    }
    press(cx, &t, "escape");
    draw(cx, &t);
    let open = cx
        .update_window(t.window, |_, window, _| {
            window.try_find("popup-menu").is_some()
        })
        .unwrap();
    assert!(!open, "escape dismisses the menu");
}

// covers #399: the arrow keys move the highlight, and a menu rebuilt by a live update keeps it on
// the same entry, so Enter still acts on it.
#[gpui_kit::test]
async fn a_rebuilt_row_menu_keeps_its_highlighted_entry(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_loading(cx, &t);
    right_click(cx, &t, "web").await;
    assert_eq!(
        menu(cx, &t),
        [
            "Logs",
            "Previous Logs",
            "Shell",
            "Forward Port",
            "web is still loading"
        ]
    );
    // Logs, Previous Logs, Shell.
    for _ in 0..3 {
        press(cx, &t, "down");
    }
    let rebuilds = crate::ui::row_menu::tests::REBUILDS.with(|n| n.get());
    show_fetched(cx, &t, pod_detail("web"));
    draw(cx, &t);
    assert_ne!(
        crate::ui::row_menu::tests::REBUILDS.with(|n| n.get()),
        rebuilds,
        "the menu was rebuilt"
    );
    // Shell is a container picker now: Enter opens it.
    press(cx, &t, "enter");
    assert_eq!(
        submenu(cx, &t),
        ["app (default)", "sidecar"],
        "Enter acted on Shell"
    );
}

fn menu_open(cx: &mut TestAppContext, t: &TestWorkspace) -> bool {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        window.try_find("popup-menu").is_some()
    })
    .unwrap()
}

/// A pods list of `api` and `web` with `web` loaded and the list focused.
async fn focused_pods(cx: &mut TestAppContext, t: &TestWorkspace) {
    activate(cx, t, "kind-alpha").await;
    open_list(
        cx,
        t,
        pods_entry(),
        pods(&[("demo", "api", 0), ("demo", "web", 1)]),
        "api",
    )
    .await;
    select_row(cx, t, "web");
    show_fetched(cx, t, pod_detail("web"));
    focus_list(cx, t);
    draw(cx, t);
}

// covers #496: Escape closes the menu and focus returns to where it was.
#[gpui_kit::test]
async fn escape_returns_focus_from_a_row_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    focused_pods(cx, &t).await;
    right_click(cx, &t, "web").await;
    assert_ne!(app_state(cx, &t).focus, Focus::List, "the menu has focus");
    press(cx, &t, "escape");
    assert!(!menu_open(cx, &t));
    assert_eq!(app_state(cx, &t).focus, Focus::List);
}

// covers #496: a click outside closes the menu.
#[gpui_kit::test]
async fn a_click_outside_closes_a_row_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    focused_pods(cx, &t).await;
    right_click(cx, &t, "web").await;
    cx.update_window(t.window, |_, window, cx| window.click("row-uid-api", cx))
        .unwrap();
    assert!(!menu_open(cx, &t));
}

// covers #496: a right-click on another row replaces the menu and dismissal returns focus to
// the list.
#[gpui_kit::test]
async fn right_clicking_another_row_replaces_its_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    focused_pods(cx, &t).await;
    right_click(cx, &t, "web").await;
    right_click(cx, &t, "api").await;
    draw(cx, &t);
    assert_eq!(
        app_state(cx, &t).menu.unwrap().owner.as_deref(),
        Some("row-menu-uid-api")
    );
    press(cx, &t, "escape");
    assert!(!menu_open(cx, &t));
    assert_eq!(app_state(cx, &t).focus, Focus::List);
}

#[gpui_kit::test]
async fn a_replaced_row_menu_returns_focus_to_the_original_view(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, workspace, model) = open_empty_workspace(cx, dir.path()).await;
    let sidebar_focus = workspace.read_with(cx, |ws, _| ws.sidebar_focus.clone());
    let core = model.read_with(cx, |model, _| model.core().clone());
    let list = cx
        .update_window(window, |_, window, cx| {
            window.focus(&sidebar_focus, cx);
            cx.new(|cx| {
                ResourceList::new(
                    core,
                    "test".into(),
                    &pods_entry(),
                    vec![],
                    Remembered::default(),
                    window,
                    cx,
                )
            })
        })
        .unwrap();

    // Open the menus directly so selecting a row does not move focus to the list first.
    let build: RowMenu = Rc::new(|menu, _, _| menu.item(PopupMenuItem::new("Logs")));
    let _replacement = cx
        .update_window(window, |_, window, cx| {
            assert!(sidebar_focus.is_focused(window));
            let replacement = list.update(cx, |_, cx| {
                let mut open = |uid: &str, replaced| {
                    let back_to = OpenRowMenu::focus_before(replaced, window, cx).unwrap();
                    let menu = OpenRowMenu::open(
                        &build,
                        uid.into(),
                        Default::default(),
                        back_to,
                        window,
                        cx,
                    );
                    assert!(menu.menu().read(cx).focus_handle(cx).is_focused(window));
                    menu
                };
                let first = open("uid-web", None);
                open("uid-api", Some(first))
            });
            replacement.menu().update(cx, |_, cx| cx.emit(DismissEvent));
            replacement
        })
        .unwrap();

    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.focused(cx),
            Some(sidebar_focus),
            "dismissing the replacement returns focus to the sidebar"
        );
    })
    .unwrap();
}

// covers #496: the menu closes when its row leaves the list, since it acts on that row.
#[gpui_kit::test]
async fn a_row_menu_closes_when_its_row_goes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    focused_pods(cx, &t).await;
    right_click(cx, &t, "web").await;
    let list = t
        .workspace
        .read_with(cx, |ws, _| ws.list.as_ref().unwrap().clone());
    list.update(cx, |list, cx| {
        list.set_snapshot(pods(&[("demo", "api", 0)]), cx)
    });
    // The rows are recomputed off the main thread and the dismissal is deferred to the draw after
    // that, so wait for the menu itself to go.
    cx.wait_for(t.window, WAIT, |window, _| {
        window.try_find("popup-menu").is_none()
    })
    .await;
    assert!(!menu_open(cx, &t));
    assert!(!menu_open(cx, &t), "it stays closed");
    assert_eq!(app_state(cx, &t).focus, Focus::List);
}

// covers #251: a pod inserted above the right-clicked row moves the row, and its menu stays open
// on that row.
#[gpui_kit::test]
async fn a_row_menu_stays_open_when_the_list_reorders(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    focused_pods(cx, &t).await;
    right_click(cx, &t, "web").await;
    let list = t
        .workspace
        .read_with(cx, |ws, _| ws.list.as_ref().unwrap().clone());
    list.update(cx, |list, cx| {
        list.set_snapshot(
            pods(&[("demo", "aaa", 0), ("demo", "api", 0), ("demo", "web", 1)]),
            cx,
        )
    });
    cx.wait_for(t.window, WAIT, |window, _| {
        let tops: Vec<_> = ["aaa", "api", "web"]
            .iter()
            .filter_map(|name| window.try_find(format!("row-uid-{name}")))
            .map(|row| f32::from(row.bounds().origin.y))
            .collect();
        tops.len() == 3 && tops.is_sorted_by(|a, b| a < b)
    })
    .await;
    assert!(menu_open(cx, &t));
    assert_eq!(
        app_state(cx, &t).menu.unwrap().owner.as_deref(),
        Some("row-menu-uid-web")
    );
}

/// Pod `web` with a restarted sidecar, as fetched with `uid`.
fn web_with_sidecar(uid: &str) -> ObjectDetail {
    detail(json!({
        "kind": "Pod",
        "metadata": {"name": "web", "namespace": "demo", "uid": uid},
        "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
        "status": {"containerStatuses": [
            {"name": "app", "restartCount": 0, "state": {"running": {}}},
            {"name": "sidecar", "restartCount": 3,
             "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
        ]}
    }))
}

// A refresh can find that the pod's name now belongs to a replacement before the list drops the
// old row. Logs go by pod name, so a menu already open drops its picker and says why.
#[gpui_kit::test]
async fn a_replaced_pods_open_menu_offers_no_logs(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(cx, &t, web_with_sidecar("uid-web"));
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 1);
    assert_eq!(submenu(cx, &t), ["sidecar"]);
    show_fetched(cx, &t, web_with_sidecar("uid-replacement"));
    assert_eq!(
        menu(cx, &t)[..4],
        [
            "Logs",
            "web was deleted",
            "Previous Logs",
            "web was deleted"
        ]
    );
    click_entry(cx, &t, 2);
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
}

// The same replaced pod from the keys, the pane's state and the registry: nothing opens, and
// each says why.
#[gpui_kit::test]
async fn a_replaced_pod_refuses_logs_from_the_keys(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(cx, &t, web_with_sidecar("uid-web"));
    show_fetched(cx, &t, web_with_sidecar("uid-replacement"));
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
    let logs = actions(cx, &t).remove(0);
    assert_eq!(
        (logs.enabled, logs.reason.as_deref()),
        (false, Some("web was deleted"))
    );
    let refused = dispatch(
        cx,
        &t,
        "detail::OpenContainerLogs",
        json!({"container": "sidecar", "previous": true}),
    )
    .unwrap_err();
    assert!(
        format!("{refused:?}").contains("web was deleted"),
        "{refused:?}"
    );
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
}

// covers #251: a crash-looping sidecar's previous logs open from the row menu although the
// default container never restarted.
#[gpui_kit::test]
async fn previous_logs_open_for_a_restarted_sidecar(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(
        cx,
        &t,
        detail(json!({
            "kind": "Pod",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
            "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
            "status": {"containerStatuses": [
                {"name": "app", "restartCount": 0, "state": {"running": {}}},
                {"name": "sidecar", "restartCount": 3,
                 "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
            ]}
        })),
    );
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 1);
    assert_eq!(submenu(cx, &t), ["sidecar"]);
    click_submenu(cx, &t, 1);
    let (tabs, _) = dock_tabs(cx, &t);
    assert_eq!(tabs.len(), 1, "{tabs:?}");
    let request = t.workspace.read_with(cx, |ws, cx| {
        ws.dock
            .read(cx)
            .active_log()
            .unwrap()
            .read(cx)
            .request()
            .clone()
    });
    assert!(request.previous, "the picker opens the previous instance");
    assert_eq!(request.cluster.to_string(), "kind-alpha");
    let flint_core::LogSubject::Pod(target) = request.subject else {
        panic!("a pod's logs: {:?}", request.subject);
    };
    assert_eq!(
        (
            target.namespace.as_str(),
            target.pod.as_str(),
            target.container.as_deref()
        ),
        ("demo", "web", Some("sidecar"))
    );
}

// covers #496: a window that gets smaller dismisses the menu, whose limits were worked out for
// the old size, and focus returns.
#[gpui_kit::test]
async fn a_shrunk_window_closes_a_row_menu(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    focused_pods(cx, &t).await;
    right_click(cx, &t, "web").await;
    let size = cx
        .update_window(t.window, |_, window, _| window.viewport_size())
        .unwrap();
    gpui_kit::VisualTestContext::from_window(t.window, cx)
        .simulate_resize(gpui_kit::size(size.width - gpui_kit::px(40.), size.height));
    assert!(!menu_open(cx, &t));
    assert_eq!(app_state(cx, &t).focus, Focus::List);
}

// covers UP-47, UP-40: an action that waited does not take focus from where the user went
// meanwhile (its tab still opens); one whose focus stayed put takes it. It shows that it waits,
// and gives up, saying so, if its object never loads.
#[gpui_kit::test]
async fn a_late_action_keeps_focus_and_gives_up_in_time(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_loading(cx, &t);
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    let waiting = app_state(cx, &t)
        .detail
        .unwrap()
        .banners
        .into_iter()
        .find(|b| b.id == "detail-pending")
        .map(|b| b.text);
    assert_eq!(waiting.as_deref(), Some("Logs opens once web has loaded"));
    // The user moves to the filter meanwhile.
    dispatch(cx, &t, "resource_list::FocusFilter", Value::Null).unwrap();
    draw(cx, &t);
    assert_eq!(app_state(cx, &t).focus, Focus::ListFilter);
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t), (vec!["web · app".to_owned()], Some(0)));
    assert_eq!(app_state(cx, &t).focus, Focus::ListFilter, "focus stayed");

    // Focus left where it was: the tab takes it.
    show_loading(cx, &t);
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-l");
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t).1, Some(1));
    assert_ne!(app_state(cx, &t).focus, Focus::List, "the tab took focus");

    // Never loaded: it gives up, saying so.
    show_loading(cx, &t);
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-e");
    cx.executor().advance_clock(PENDING_TIMEOUT);
    draw(cx, &t);
    assert_eq!(
        refusal(cx, &t).as_deref(),
        Some("Can't open a shell: gave up waiting for web to load (30 s).")
    );
    let state = app_state(cx, &t).detail.unwrap();
    assert!(state.banners.iter().all(|b| b.id != "detail-pending"));
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t).0.len(), 2, "no shell once given up");
}

// covers UP-47: a pending action whose object fails to load says why.
#[gpui_kit::test]
async fn a_pending_action_on_a_failed_fetch_says_why(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_loading(cx, &t);
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    let detail_view = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail_view.update(cx, |d, cx| {
        d.show_failed("the server is unavailable".into(), cx)
    });
    draw(cx, &t);
    assert_eq!(
        refusal(cx, &t).as_deref(),
        Some("Can't open a shell: the pod could not be fetched: the server is unavailable.")
    );
}

// covers UP-47, UP-45: the selector logs action opens one tab for the pods a label selector
// matches, and refuses an empty selector.
#[gpui_kit::test]
async fn selector_logs_open_from_the_palette(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    assert_eq!(
        dispatch(
            cx,
            &t,
            "logs::OpenSelectorLogs",
            json!({"namespace": "demo", "selector": "app=web"})
        ),
        Err(ActionError::NotAvailable("logs::OpenSelectorLogs".into())),
        "no active cluster"
    );
    activate(cx, &t, "kind-alpha").await;
    assert!(matches!(
        dispatch(
            cx,
            &t,
            "logs::OpenSelectorLogs",
            json!({"namespace": "demo", "selector": " "})
        ),
        Err(ActionError::Rejected { .. })
    ));
    for _ in 0..2 {
        dispatch(
            cx,
            &t,
            "logs::OpenSelectorLogs",
            json!({"namespace": "demo", "selector": "app=web"}),
        )
        .unwrap();
        draw(cx, &t);
    }
    assert_eq!(dock_tabs(cx, &t), (vec!["-l app=web".to_owned()], Some(0)));
}

// covers UP-47: once the row's cluster is not connected, Logs and Shell are disabled and the
// workspace says why.
#[gpui_kit::test]
async fn a_cluster_that_is_not_connected_disables_the_actions(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    draw(cx, &t);
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dispatch(cx, &t, "detail::OpenShell", Value::Null), Ok(()));
    // Disconnected, the list and the pane stay: the pane's buttons are disabled, saying why
    // without hovering.
    let set_state = |cx: &mut TestAppContext, state: ConnectionState| {
        t.model.update(cx, |model, cx| {
            for cluster in &mut model.clusters {
                cluster.state = state.clone();
            }
            cx.notify();
        });
        draw(cx, &t);
    };
    set_state(cx, ConnectionState::Disconnected);
    assert_eq!(logs_button_disabled(cx, &t), Some(true));
    assert_eq!(shell_button_disabled(cx, &t), Some(true));
    assert_eq!(
        captions(cx, &t),
        ["Can't open the logs or a shell: kind-alpha is not connected."]
    );
    set_state(
        cx,
        ConnectionState::Failed(SafeErrorText::reduced("connection refused")),
    );
    let detail_view = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    assert_eq!(
        detail_view.read_with(cx, |d, _| d.action_captions()),
        ["Can't open the logs or a shell: kind-alpha is not connected: connection refused."]
    );
    for action in [
        "detail::OpenLogs",
        "detail::OpenPreviousLogs",
        "detail::OpenShell",
        "logs::OpenSelectorLogs",
    ] {
        let args = match action {
            "logs::OpenSelectorLogs" => json!({"namespace": "demo", "selector": "app=web"}),
            _ => Value::Null,
        };
        assert_eq!(
            dispatch(cx, &t, action, args),
            Err(ActionError::NotAvailable(action.into()))
        );
    }
    let reason = t
        .workspace
        .read_with(cx, |ws, cx| ws.sessions_blocked("kind-alpha", cx));
    assert_eq!(
        reason.as_deref(),
        Some("kind-alpha is not connected: connection refused")
    );
    assert_eq!(dock_tabs(cx, &t).0, ["Shell: web · app"]);
    // Every entry is unavailable, saying why.
    let why = "kind-alpha is not connected: connection refused";
    assert_eq!(
        actions(cx, &t),
        [
            unavailable("Logs", why),
            unavailable("Previous Logs", why),
            unavailable("Shell", why)
        ]
    );
    // A long error shows as core's summary of it: one line, quoted values hidden.
    let long = format!("connection refused by \"{}\"\nsecond line", "x".repeat(300));
    let summary = SafeErrorText::reduced(&long);
    t.model.update(cx, |model, cx| {
        for cluster in &mut model.clusters {
            cluster.state = ConnectionState::Failed(summary.clone());
        }
        cx.notify();
    });
    let shown = actions(cx, &t)[0].reason.clone().unwrap();
    assert_eq!(shown, format!("kind-alpha is not connected: {summary}"));
    assert!(!shown.contains('\n') && !shown.contains("xxx"), "{shown}");
    // A session asked for anyway (a pending action that loads late) is refused, saying why.
    let request = t.workspace.read_with(cx, |ws, cx| {
        ws.dock
            .read(cx)
            .active_terminal()
            .unwrap()
            .read(cx)
            .request()
            .clone()
    });
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.open_shell(request, true, window, cx))
    })
    .unwrap();
    let why = refusal(cx, &t).unwrap();
    assert!(
        why.starts_with("Can't open a shell: kind-alpha is not connected"),
        "{why}"
    );
}

// covers UP-42, UP-47: a log tab opened from a row's menu frees everything once the tab closes
// and the row goes: the tab's view and session, and the menu (docs/TESTING.md, "Memory leaks").
#[gpui_kit::test]
async fn a_tab_opened_from_a_row_leaks_nothing_once_closed(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let _pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    dispatch(
        cx,
        &t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    draw(cx, &t);
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 0);
    let (id, view) = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        let id = dock.active_id().unwrap();
        let DockTab::Logs(view) = dock.tab(id).unwrap() else {
            panic!("a log tab")
        };
        (id, view.downgrade())
    });
    dispatch(cx, &t, "dock::CloseTab", json!({ "tab": id })).unwrap();
    draw(cx, &t);
    assert!(view.upgrade().is_none(), "closing the tab ends its session");
    // gpui-component keeps a trigger's last menu until the trigger is no longer drawn: the
    // Deployment is deleted. The detail pane then shows it as deleted, with what that draws
    // (a disabled Shell), until it is closed too.
    feed(cx, &t, deployments(&[])).await;
    dispatch(cx, &t, "resource_list::CloseDetail", Value::Null).unwrap();
    draw(cx, &t);
    draw(cx, &t);
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
}

/// Feeds the open list `snapshot` and waits until it shows it: the core filters and sorts its
/// rows on its own runtime, which the test executor does not wait for.
async fn feed(cx: &mut TestAppContext, t: &TestWorkspace, snapshot: Arc<KindSnapshot>) {
    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    list.update(cx, |list, cx| list.set_snapshot(snapshot.clone(), cx));
    cx.wait_for(t.window, WAIT, move |_, cx| {
        Arc::ptr_eq(list.read(cx).shown().0, &snapshot)
    })
    .await;
    draw(cx, t);
}

/// Selects the row of `name` in `demo` and draws.
fn select_row(cx: &mut TestAppContext, t: &TestWorkspace, name: &str) {
    dispatch(
        cx,
        t,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": name}),
    )
    .unwrap();
    draw(cx, t);
}

// covers UP-47: an action that comes right after moving to another row, before the window
// draws again (a quick key, an automation step), acts on the new row once it has loaded: not
// on the row left, and it is not lost.
#[gpui_kit::test]
async fn an_action_right_after_moving_rows_acts_on_the_new_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(
        cx,
        &t,
        pods_entry(),
        pods(&[("demo", "api", 0), ("demo", "web", 1)]),
        "api",
    )
    .await;
    select_row(cx, &t, "api");
    show_fetched(cx, &t, pod_detail("api"));

    // The test platform draws after every update, so select web and ask for its logs in one
    // update, as two events the app handles before its next frame. The pane left stays alive
    // meanwhile, as an entity may.
    let api = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    let workspace = t.workspace.clone();
    let list = t.workspace.read_with(cx, |ws, _| ws.list.clone().unwrap());
    cx.update_window(t.window, |_, window, cx| {
        let selected = Selected {
            uid: "uid-web".into(),
            name: "web".into(),
            namespace: Some("demo".into()),
            resource_version: "1".into(),
        };
        workspace.update(cx, |ws, cx| {
            ws.on_list_event(&list, &ListEvent::Selected(Some(selected)), window, cx);
            // Still loading, as on a slow server.
            let web = ws.detail.clone().unwrap();
            web.update(cx, |d, cx| d.show_loading(cx));
        });
        registry::dispatch("detail::OpenLogs", Value::Null, window, cx).unwrap();
    })
    .unwrap();
    drop(api);
    cx.run_until_parked();
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new(), "waits for web");
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t).0, ["web · app"]);
}

// covers UP-47, UP-41: a deleted workload or pod has no shell: Shell is disabled and says why,
// though the pods listed before still show.
#[gpui_kit::test]
async fn a_deleted_object_has_no_shell(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods_listed = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    let_pods_list(cx, &t);
    pods_listed.answer(cx, &t, Ok(vec![workload_pod("web-a", true, &["app"])]));
    assert_eq!(actions(cx, &t)[1], available("Shell"));

    feed(cx, &t, deployments(&[])).await;
    assert_eq!(
        dispatch(cx, &t, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into()))
    );
    assert_eq!(actions(cx, &t)[1], unavailable("Shell", "web was deleted"));
    assert_eq!(shell_button_disabled(cx, &t), Some(true));

    // The same for a pod.
    open_list(cx, &t, pods_entry(), pods(&[("demo", "api", 0)]), "api").await;
    select_row(cx, &t, "api");
    show_fetched(cx, &t, pod_detail("api"));
    feed(cx, &t, pods(&[])).await;
    assert_eq!(
        dispatch(cx, &t, "detail::OpenShell", Value::Null),
        Err(ActionError::NotAvailable("detail::OpenShell".into()))
    );
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
}

/// Bumps the selected Deployment `web`'s resourceVersion, as a change of it does.
async fn change_web(cx: &mut TestAppContext, t: &TestWorkspace, version: &str) {
    let mut changed = (*deployments(&["web"])).clone();
    let mut row = (*changed.rows[0]).clone();
    row.resource_version = version.into();
    changed.rows = vec![Arc::new(row)];
    feed(cx, t, Arc::new(changed)).await;
}

// covers UP-47, UP-41: what the shown pods say does not outlive a change of the workload or a
// failed list: Shell is then enabled (it lists the pods itself when chosen), the keys and the
// button agree, and the pods are listed again soon (once, however often the workload changes).
// A failed list is not retried on a timer.
#[gpui_kit::test]
async fn a_stale_or_failed_pod_list_does_not_disable_shell(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    let_pods_list(cx, &t);
    pods.answer(cx, &t, Ok(vec![workload_pod("web-a", false, &["app"])]));
    assert_eq!(shell_button_disabled(cx, &t), Some(true), "none ready");

    // Its pods became ready: the Deployment changes, more than once.
    change_web(cx, &t, "2").await;
    change_web(cx, &t, "3").await;
    assert_eq!(shell_button_disabled(cx, &t), Some(false));
    assert_eq!(actions(cx, &t)[1], available("Shell"));
    assert_eq!(pods.asked(), 1);
    cx.executor().advance_clock(crate::ui::detail::RELIST_DELAY);
    draw(cx, &t);
    assert_eq!(pods.waiting(), [Freshness::Cached], "listed again, once");
    // It changes again while that list runs: the list is out of date once it arrives.
    change_web(cx, &t, "4").await;
    pods.answer(cx, &t, Ok(vec![workload_pod("web-a", true, &["app"])]));
    cx.executor().advance_clock(crate::ui::detail::RELIST_DELAY);
    draw(cx, &t);
    assert_eq!(pods.waiting(), [Freshness::Cached]);
    pods.answer(cx, &t, Ok(vec![workload_pod("web-a", true, &["app"])]));
    assert_eq!(
        open_menu(cx, &t, "web").await,
        ["Logs", "Shell in web-a", "", "Scale…", "Rollout Restart…"]
    );
    press(cx, &t, "escape");

    // A failed list: Shell stays enabled, and the keys run it.
    change_web(cx, &t, "5").await;
    cx.executor().advance_clock(crate::ui::detail::RELIST_DELAY);
    draw(cx, &t);
    pods.answer(cx, &t, Err("the server is busy".into()));
    assert_eq!(shell_button_disabled(cx, &t), Some(false));
    assert_eq!(dispatch(cx, &t, "detail::OpenShell", Value::Null), Ok(()));
    assert_eq!(pods.waiting(), [Freshness::Current]);
    pods.answer(cx, &t, Err("the server is busy".into()));
    // Not retried on a timer.
    let asked = pods.asked();
    cx.executor()
        .advance_clock(crate::ui::detail::RELIST_DELAY * 10);
    draw(cx, &t);
    assert_eq!(pods.asked(), asked);
}

// covers UP-47: a Shell that gave up waiting drops its list: the next Shell lists again
// instead of joining one that hangs.
#[gpui_kit::test]
async fn a_shell_that_gave_up_drops_its_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let pods = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    assert_eq!(pods.waiting(), [Freshness::Current]);
    cx.executor().advance_clock(PENDING_TIMEOUT);
    draw(cx, &t);
    assert_eq!(pods.waiting(), Vec::<Freshness>::new(), "dropped");
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    assert_eq!(pods.waiting(), [Freshness::Current], "a list of its own");
    pods.answer(cx, &t, Ok(vec![workload_pod("web-a", true, &["app"])]));
    assert_eq!(dock_tabs(cx, &t).0, ["Shell: web-a · app"]);
}

// covers UP-47, UP-40: the reason an action did not open goes once it no longer applies: when
// the selection clears, another list opens, or after a while.
#[gpui_kit::test]
async fn a_refusal_goes_when_it_no_longer_applies(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    let refuse = |cx: &mut TestAppContext| {
        let workspace = t.workspace.clone();
        workspace.update(cx, |ws, cx| ws.refuse("Shell: no".into(), cx));
        draw(cx, &t);
    };
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    refuse(cx);
    assert_eq!(refusal(cx, &t).as_deref(), Some("Shell: no"));
    cx.executor()
        .advance_clock(crate::ui::workspace::ACTION_ERROR_SHOWN);
    draw(cx, &t);
    assert_eq!(refusal(cx, &t), None, "after a while");

    refuse(cx);
    dispatch(cx, &t, "resource_list::CloseDetail", Value::Null).unwrap();
    draw(cx, &t);
    assert_eq!(refusal(cx, &t), None, "the selection cleared");

    refuse(cx);
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    assert_eq!(refusal(cx, &t), None, "another list");
}

/// Whether the detail pane's Logs button is drawn disabled (`None`: not drawn).
fn logs_button_disabled(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<bool> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        window
            .try_find("detail-logs")
            .map(|b| b.disabled() == Some(true))
    })
    .unwrap()
}

/// The captions under the detail pane's buttons, as drawn.
fn captions(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    draw(cx, t);
    cx.update_window(t.window, |_, window, _| {
        gpui_kit::base::test_support::texts(window)
            .into_iter()
            .map(|t| t.text().to_owned())
            .filter(|t| t.starts_with("Can't"))
            .collect()
    })
    .unwrap()
}

// covers UP-47: the pane says why Logs or Shell cannot open in a caption under its buttons, not
// only in their tooltips; Logs stays, disabled, when the pod could not be fetched.
#[gpui_kit::test]
async fn the_pane_says_why_under_its_buttons(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let lister = FakePods::install(cx);
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    // Loading: both wait, so both are enabled and nothing is said.
    show_loading(cx, &t);
    assert_eq!(logs_button_disabled(cx, &t), Some(false));
    assert_eq!(captions(cx, &t), Vec::<String>::new());
    // The fetch failed: both are disabled, the reason said once.
    let detail_view = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail_view.update(cx, |d, cx| d.show_failed("Forbidden: no".into(), cx));
    assert_eq!(logs_button_disabled(cx, &t), Some(true));
    assert_eq!(shell_button_disabled(cx, &t), Some(true));
    // The body says what the server said; the caption does not repeat it.
    assert_eq!(
        captions(cx, &t),
        ["Can't open the logs or a shell until the pod is fetched."]
    );
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(logs_button_disabled(cx, &t), Some(false));
    assert_eq!(captions(cx, &t), Vec::<String>::new());

    // A Deployment with no ready pod: Shell alone.
    open_list(cx, &t, deployments_entry(), deployments(&["web"]), "web").await;
    select_row(cx, &t, "web");
    let_pods_list(cx, &t);
    lister.answer(cx, &t, Ok(vec![workload_pod("web-a", false, &["app"])]));
    assert_eq!(shell_button_disabled(cx, &t), Some(true));
    assert_eq!(
        captions(cx, &t),
        ["Can't open a shell: no pod of Deployment web is ready."]
    );
}

// covers UP-47: a deleted pod's Shell is disabled, but the caption leaves the "deleted" to the
// pane's notice instead of saying it again.
#[gpui_kit::test]
async fn a_deleted_pod_gets_no_caption(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(cx, &t, pod_detail("web"));
    let detail_view = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail_view.update(cx, |d, cx| d.mark_gone(cx));
    assert_eq!(shell_button_disabled(cx, &t), Some(true));
    assert_eq!(captions(cx, &t), Vec::<String>::new());
}

// covers UP-47, UP-40, UP-41: on a multi-container pod each picker shows the key of the action
// on its default container's entry, so the keys stay visible; the pane's picker marks the
// default and its tooltip names what the key opens. A container that is not running is listed
// disabled in the Shell picker, and a native sidecar is an app container.
#[gpui_kit::test]
async fn pickers_show_the_keys_and_which_containers_run(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(
        cx,
        &t,
        detail(json!({
            "kind": "Pod",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
            "spec": {
                "initContainers": [{"name": "mesh", "restartPolicy": "Always"}],
                "containers": [{"name": "app"}, {"name": "worker"}]
            },
            "status": {
                "initContainerStatuses": [{"name": "mesh", "state": {"running": {}}}],
                "containerStatuses": [
                    {"name": "app", "state": {"running": {}}},
                    {"name": "worker", "restartCount": 4,
                     "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]
            }
        })),
    );
    // The pane's Logs picker: the default marked, the tooltip naming what Cmd-L opens.
    let tooltip = t.description(cx, "detail-logs");
    let keys = registry::key_hint("secondary-l");
    assert_eq!(
        tooltip.as_deref(),
        Some(format!("Choose a container ({keys} opens app, the default)").as_str())
    );
    cx.update_window(t.window, |_, window, cx| window.click("detail-logs", cx))
        .unwrap();
    let entries: Vec<String> = menu(cx, &t).into_iter().filter(|e| !e.is_empty()).collect();
    assert_eq!(
        entries,
        [
            "Container",
            "mesh",
            "app (default)",
            "worker",
            "Previous instance",
            "worker"
        ]
    );
    press(cx, &t, "escape");

    // The row's Shell picker: the sidecar is offered, the crash-looping one disabled; the
    // default's entry shows Shift-Cmd-E.
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 2);
    assert_eq!(
        submenu(cx, &t),
        ["mesh", "app (default)", "worker (not running)"]
    );
    let shown_keys = cx
        .update_window(t.window, |_, window, _| {
            gpui_kit::base::test_support::texts(window)
                .into_iter()
                .map(|t| t.text().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap();
    let shell_keys = gpui_kit::component::kbd::Kbd::format(
        &gpui_kit::Keystroke::parse(if cfg!(target_os = "macos") {
            "cmd-shift-e"
        } else {
            "ctrl-shift-e"
        })
        .unwrap(),
    );
    assert!(
        shown_keys.contains(&shell_keys),
        "{shell_keys} in {shown_keys:?}"
    );
    // The disabled one does nothing.
    click_submenu(cx, &t, 3);
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new());
}

// covers UP-41, UP-47: when the default container is not running, Shift-Cmd-E refuses with the
// reason its "(default, not running)" entry shows, while the picker still offers the others.
#[gpui_kit::test]
async fn the_key_refuses_a_default_container_that_is_not_running(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(
        cx,
        &t,
        detail(json!({
            "kind": "Pod",
            "metadata": {"name": "web", "namespace": "demo", "uid": "uid-web"},
            "spec": {"containers": [{"name": "app"}, {"name": "sidecar"}]},
            "status": {"containerStatuses": [
                {"name": "app", "state": {"waiting": {"reason": "CrashLoopBackOff"}}},
                {"name": "sidecar", "state": {"running": {}}}
            ]}
        })),
    );
    focus_list(cx, &t);
    press(cx, &t, "secondary-shift-e");
    assert_eq!(dock_tabs(cx, &t).0, Vec::<String>::new(), "no doomed shell");
    assert_eq!(
        refusal(cx, &t).as_deref(),
        Some("Can't open a shell: container app is not running.")
    );
    // The pane's Shell is still a picker, and says nothing against it.
    assert_eq!(shell_button_disabled(cx, &t), Some(false));
    assert_eq!(captions(cx, &t), Vec::<String>::new());
    right_click(cx, &t, "web").await;
    click_entry(cx, &t, 3); // After Logs, Previous Logs and its reason.
    assert_eq!(submenu(cx, &t), ["app (default, not running)", "sidecar"]);
    click_submenu(cx, &t, 2);
    assert_eq!(dock_tabs(cx, &t).0, ["Shell: web · sidecar"]);
}

// covers UP-47: the strip that says why takes a row of its own under the list and the pane,
// above the dock, so it covers none of them, and stays one line high however long the reason
// is (the whole sentence is in its tooltip).
#[gpui_kit::test]
async fn the_refusal_strip_covers_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(cx, &t, pod_detail("web"));
    // A dock tab, so the dock shows below the strip.
    dispatch(cx, &t, "detail::OpenLogs", Value::Null).unwrap();
    draw(cx, &t);
    let strip = |cx: &mut TestAppContext, why: String| {
        let workspace = t.workspace.clone();
        workspace.update(cx, |ws, cx| ws.refuse(why, cx));
        draw(cx, &t);
        cx.update_window(t.window, |_, window, _| {
            let bounds = |id: &'static str| window.find(id).bounds();
            (
                bounds("action-error"),
                bounds("detail"),
                bounds("row-uid-web"),
                bounds("dock-pane"),
            )
        })
        .unwrap()
    };
    let (short, detail, row, dock) = strip(cx, "Can't open a shell: no.".into());
    assert!(short.top() >= detail.bottom(), "{short:?} under {detail:?}");
    assert!(short.top() >= row.bottom(), "{short:?} under {row:?}");
    assert!(short.bottom() <= dock.top(), "{short:?} over {dock:?}");
    let long = format!("Can't open a shell: {}.", "a long reason ".repeat(60));
    let (long, ..) = strip(cx, long);
    assert_eq!(long.size.height, short.size.height, "one line");
}

// covers UP-47, UP-41: a tab that opens late while the user types in a dock tab opens behind
// it: the tab in use stays shown and keeps focus, so keys are not lost to a hidden tab.
#[gpui_kit::test]
async fn a_late_tab_opens_behind_the_dock_tab_in_use(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(
        cx,
        &t,
        pods_entry(),
        pods(&[("demo", "api", 0), ("demo", "web", 1)]),
        "api",
    )
    .await;
    select_row(cx, &t, "api");
    show_fetched(cx, &t, pod_detail("api"));
    dispatch(cx, &t, "detail::OpenShell", Value::Null).unwrap();
    draw(cx, &t);
    let shell = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).active_id().unwrap());

    // Logs of web, asked for from the list before web has loaded.
    select_row(cx, &t, "web");
    show_loading(cx, &t);
    focus_list(cx, &t);
    press(cx, &t, "secondary-l");
    // The user goes back to the shell and types there.
    let terminal = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        crate::ui::dock::DockItem::tab_focus(dock.active_terminal().unwrap().read(cx))
    });
    cx.update_window(t.window, |_, window, cx| window.focus(&terminal, cx))
        .unwrap();
    draw(cx, &t);
    show_fetched(cx, &t, pod_detail("web"));
    let (titles, _) = dock_tabs(cx, &t);
    assert_eq!(titles, ["Shell: api · app", "web · app"], "opened");
    let active = t
        .workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).active_id());
    assert_eq!(active, Some(shell), "the shell stays shown");
    let focused = cx
        .update_window(t.window, |_, window, _| terminal.is_focused(window))
        .unwrap();
    assert!(focused, "and keeps focus");
}

// covers UP-40: `FLINT_VIEW=…#logs` opens the pod's logs once it loads, and the tab takes
// focus: no key asked for it, so there is nothing to keep focus for.
#[gpui_kit::test]
async fn logs_asked_for_at_startup_take_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_loading(cx, &t);
    let detail_view = t
        .workspace
        .read_with(cx, |ws, _| ws.detail.clone().unwrap());
    detail_view.update(cx, |d, cx| d.open_logs_when_loaded(cx));
    focus_list(cx, &t);
    show_fetched(cx, &t, pod_detail("web"));
    assert_eq!(dock_tabs(cx, &t).0, ["web · app"]);
    assert_ne!(app_state(cx, &t).focus, Focus::List, "the tab took focus");
}

// covers UP-47: a dismissed menu stops following its row, so it is not built again.
#[gpui_kit::test]
async fn a_dismissed_menu_is_not_built_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(cx, &t, pod_detail("web"));
    right_click(cx, &t, "web").await;
    press(cx, &t, "escape");
    let rebuilds = crate::ui::row_menu::tests::REBUILDS.with(|n| n.get());
    let mut finished = pod_detail("web");
    finished.object["status"]["phase"] = json!("Succeeded");
    show_fetched(cx, &t, finished);
    assert_eq!(
        crate::ui::row_menu::tests::REBUILDS.with(|n| n.get()),
        rebuilds
    );
}

// covers UP-47: a menu entry refused as not available says why in the user's words, from the
// row's actions, never by the action's internal name.
#[gpui_kit::test]
async fn a_refused_entry_says_why_in_words(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    select_row(cx, &t, "web");
    show_fetched(cx, &t, pod_detail("web"));
    t.model.update(cx, |model, cx| {
        for cluster in &mut model.clusters {
            cluster.state = ConnectionState::Failed(SafeErrorText::reduced("connection refused"));
        }
        cx.notify();
    });
    let why = t.workspace.read_with(cx, |ws, cx| {
        crate::ui::row_menu::refusal(
            ws,
            &registry::OpenShell,
            &ActionError::NotAvailable("detail::OpenShell".into()),
            cx,
        )
    });
    assert_eq!(
        why,
        "Can't open a shell: kind-alpha is not connected: connection refused."
    );
}
