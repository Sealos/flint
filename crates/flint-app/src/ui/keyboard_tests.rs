//! View tests for the workspace's keyboard paths (UP-33): Tab and Shift-Tab between the panes,
//! the cluster sidebar's and the navigation's arrows, Enter, Left and Right, and Go Back
//! (Cmd-[).

use flint_core::KindId;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::json;

use crate::ui::support_tests::{
    TestWorkspace, WAIT, objects, open_fake_shell, open_workspace, pods, standard_sections,
};
use crate::ui::view_state::{AppState, Focus, NavState};

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        window.render_frame(cx);
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

fn focus(cx: &mut TestAppContext, t: &TestWorkspace) -> Focus {
    state(cx, t).focus
}

/// The cluster under the sidebar's cursor.
fn sidebar_cursor(cx: &mut TestAppContext, t: &TestWorkspace) -> Option<String> {
    state(cx, t)
        .clusters
        .into_iter()
        .find(|c| c.cursor)
        .map(|c| c.name)
}

/// The navigation row under the cursor: `section:Title` or `kind:plural`.
fn nav_cursor(nav: &NavState) -> Option<String> {
    nav.sections.iter().find_map(|s| {
        if s.cursor {
            return Some(format!("section:{}", s.title));
        }
        s.entries
            .iter()
            .find(|e| e.cursor)
            .map(|e| format!("kind:{}", e.kind))
    })
}

fn nav(cx: &mut TestAppContext, t: &TestWorkspace) -> NavState {
    state(cx, t).nav.expect("a navigation is shown")
}

/// Pods open with `web` selected, so the detail pane shows.
async fn pods_with_detail(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    assert!(t.detail(cx).is_some());
}

/// Forgets the places before now, the overview the cluster opened on among them, so a test
/// of the lists starts with nothing to go back to.
fn start_history_here(cx: &mut TestAppContext, t: &TestWorkspace) {
    t.workspace.update(cx, |ws, _| ws.forget_places());
}

// covers UP-33: Tab moves focus sidebar, navigation, list, detail and around again; Shift-Tab
// goes the other way. From nowhere in particular, Tab starts at the sidebar.
#[gpui_kit::test]
async fn tab_and_shift_tab_cycle_the_panes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    pods_with_detail(cx, &t).await;
    t.focus_workspace(cx);
    assert_eq!(focus(cx, &t), Focus::Workspace);
    let order = [
        Focus::Sidebar,
        Focus::Navigation,
        Focus::List,
        Focus::Detail,
        Focus::Sidebar,
    ];
    for wanted in order {
        t.press(cx, "tab");
        assert_eq!(focus(cx, &t), wanted);
    }
    for wanted in [
        Focus::Detail,
        Focus::List,
        Focus::Navigation,
        Focus::Sidebar,
    ] {
        t.press(cx, "shift-tab");
        assert_eq!(focus(cx, &t), wanted);
    }
    // Panes not shown are skipped: no detail without a selection.
    t.dispatch(cx, "resource_list::CloseDetail", serde_json::Value::Null)
        .unwrap();
    assert_eq!(focus(cx, &t), Focus::List);
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Sidebar);
    t.press(cx, "shift-tab");
    assert_eq!(focus(cx, &t), Focus::List);
    // In a text field, Tab is the field's: the list's filter keeps it.
    t.dispatch(cx, "resource_list::FocusFilter", serde_json::Value::Null)
        .unwrap();
    t.press(cx, "tab");
    assert_ne!(focus(cx, &t), Focus::Sidebar);
}

// covers UP-33: from the list's filter, Esc, Enter and Down each move focus back to the rows,
// and the filter keeps its text.
#[gpui_kit::test]
async fn keys_in_the_filter_return_to_the_rows(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "we");
    let list = t.list(cx).expect("a list is open");
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 1)
        .await;
    for key in ["escape", "enter", "down"] {
        t.press_key_of(cx, "resource_list::FocusFilter");
        assert_eq!(
            focus(cx, &t),
            Focus::ListFilter,
            "{key}: the filter has focus"
        );
        t.press(cx, key);
        assert_eq!(focus(cx, &t), Focus::List, "{key} returns to the rows");
        assert_eq!(
            list.read_with(cx, |l, _| l.shown().1),
            1,
            "{key} keeps the filter"
        );
    }
    // Back in the rows, the list's own keys work again.
    t.press(cx, "down");
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
}

// covers UP-33: Enter on the rows opens the selected row and nothing else: with no selection,
// or one the filter hides, it does nothing, and in the filter it only returns to the rows.
#[gpui_kit::test]
async fn enter_opens_only_the_selected_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    let list = t.list(cx).expect("a list is open");
    let rows = list.read_with(cx, |l, _| l.focus_handle().clone());
    cx.update_window(t.window, |_, window, cx| window.focus(&rows, cx))
        .unwrap();
    t.press(cx, "enter");
    assert_eq!(t.selected(cx), None, "no row is selected by Enter");
    assert!(t.detail(cx).is_none());

    t.press_key_of(cx, "resource_list::FocusFilter");
    t.press(cx, "enter");
    assert_eq!(focus(cx, &t), Focus::List);
    assert_eq!(t.selected(cx), None, "Enter in the filter selects nothing");
    assert!(t.detail(cx).is_none());

    t.press(cx, "down");
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    let opened = t.detail(cx).expect("Down opened the row");
    t.press(cx, "enter");
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    let same = t.detail(cx).expect("the detail stays");
    assert_eq!(opened.entity_id(), same.entity_id(), "not opened again");
    assert_eq!(focus(cx, &t), Focus::List);

    // The filter hides the selected row: Enter on the rows does not pick another.
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "we");
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 1)
        .await;
    t.press(cx, "enter");
    assert_eq!(focus(cx, &t), Focus::List);
    t.press(cx, "enter");
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
}

// covers UP-33: Tab reaches a shell tab in the dock, where Tab then belongs to the shell.
#[gpui_kit::test]
async fn tab_reaches_the_dock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    pods_with_detail(cx, &t).await;
    let (_, _fake) = open_fake_shell(cx, &t);
    assert_eq!(focus(cx, &t), Focus::Shell);
    // From outside every pane, Shift-Tab starts at the last one: the dock's tab.
    t.focus_workspace(cx);
    t.press(cx, "shift-tab");
    assert_eq!(focus(cx, &t), Focus::Shell);
    t.focus_workspace(cx);
    for _ in 0..4 {
        t.press(cx, "tab");
    }
    assert_eq!(focus(cx, &t), Focus::Detail);
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Shell);
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Shell, "the shell gets Tab");
}

// covers UP-33: in the sidebar the arrows move a cursor over the clusters, stopping at the
// ends, and Enter connects to the cluster under it, keeping focus there.
#[gpui_kit::test]
async fn the_sidebar_moves_with_the_arrows_and_enter_connects(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.focus_workspace(cx);
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Sidebar);
    // No cluster is active: the cursor starts on the first.
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("kind-alpha"));
    t.press(cx, "down");
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("kind-beta"));
    t.press(cx, "down");
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("kind-beta"));
    t.press(cx, "up");
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("kind-alpha"));
    t.press(cx, "enter");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).shown_cluster() == Some("kind-alpha")
    })
    .await;
    assert_eq!(focus(cx, &t), Focus::Sidebar);
    // The keys are the sidebar's only while it has focus.
    let available = t.available(cx);
    assert!(available.contains(&"sidebar::SelectNextCluster"));
    t.focus_workspace(cx);
    assert!(!t.available(cx).contains(&"sidebar::SelectNextCluster"));
    // A cluster picked another way moves the cursor too.
    t.activate(cx, "kind-beta").await;
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("kind-beta"));
}

// covers UP-33: in the navigation the arrows move over section headers and kinds, Enter opens a
// kind (its list takes focus) or toggles a section, Left closes the section (from a kind, its
// own, moving to its header) and Right opens it.
#[gpui_kit::test]
async fn the_navigation_moves_opens_expands_and_collapses(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.focus_workspace(cx);
    t.press(cx, "tab");
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Navigation);
    assert_eq!(
        nav_cursor(&nav(cx, &t)).as_deref(),
        Some("section:Workloads")
    );
    t.press(cx, "up");
    assert_eq!(
        nav_cursor(&nav(cx, &t)).as_deref(),
        Some("section:Workloads")
    );
    t.press(cx, "down");
    assert_eq!(nav_cursor(&nav(cx, &t)).as_deref(), Some("kind:pods"));
    t.press(cx, "enter");
    let list = t.list(cx).expect("Pods opened");
    assert_eq!(list.read_with(cx, |l, _| l.kind().clone()), KindId::pods());
    assert_eq!(focus(cx, &t), Focus::List);

    t.press(cx, "shift-tab");
    assert_eq!(focus(cx, &t), Focus::Navigation);
    assert_eq!(nav_cursor(&nav(cx, &t)).as_deref(), Some("kind:pods"));
    t.press(cx, "left");
    let shown = nav(cx, &t);
    assert_eq!(nav_cursor(&shown).as_deref(), Some("section:Workloads"));
    assert!(shown.sections[0].collapsed);
    t.press(cx, "right");
    assert!(!nav(cx, &t).sections[0].collapsed);
    // Down past Pods and Deployments to the next header; Enter opens it.
    for _ in 0..3 {
        t.press(cx, "down");
    }
    assert_eq!(nav_cursor(&nav(cx, &t)).as_deref(), Some("section:Network"));
    assert!(nav(cx, &t).sections[1].collapsed);
    t.press(cx, "enter");
    assert!(!nav(cx, &t).sections[1].collapsed);
    t.press(cx, "enter");
    assert!(nav(cx, &t).sections[1].collapsed);
    // Right on a kind does nothing; Left on a closed section keeps it closed.
    t.press(cx, "left");
    assert!(nav(cx, &t).sections[1].collapsed);
    assert_eq!(focus(cx, &t), Focus::Navigation);
}

// covers #313: Cmd-] returns to the place Go Back left, and a new navigation clears it.
#[gpui_kit::test]
async fn go_forward_returns_to_the_place_go_back_left(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    pods_with_detail(cx, &t).await;
    assert!(!t.available(cx).contains(&"workspace::GoForward"));
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[("demo", "front", 0)],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    assert!(!t.available(cx).contains(&"workspace::GoForward"));
    t.press_key_of(cx, "workspace::GoBack");
    let list = t.list(cx).expect("a list is open");
    assert_eq!(list.read_with(cx, |l, _| l.kind().clone()), KindId::pods());
    t.feed_list(cx, pods(&[("demo", "api", 0), ("demo", "web", 0)]))
        .await;
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert!(t.available(cx).contains(&"workspace::GoForward"));
    t.press_key_of(cx, "workspace::GoForward");
    let list = t.list(cx).expect("a list is open");
    assert_eq!(
        list.read_with(cx, |l, _| l.kind().clone()),
        KindId::new("", "services")
    );
    assert!(!t.available(cx).contains(&"workspace::GoForward"));
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[("demo", "front", 0)],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    // Back again, then a new navigation: nothing is left to go forward to.
    t.press_key_of(cx, "workspace::GoBack");
    assert!(t.available(cx).contains(&"workspace::GoForward"));
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "secrets"}))
        .unwrap();
    assert!(!t.available(cx).contains(&"workspace::GoForward"));
}

// covers #313: an object opened by name that has not loaded yet is a place Go Back leaves
// and Go Forward returns to, shown once it has loaded, as Go Back does the other way.
#[gpui_kit::test]
async fn go_forward_returns_to_an_object_opened_before_it_loaded(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    pods_with_detail(cx, &t).await;
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[("demo", "front", 0)],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    // The pods are not listed yet when "api" is asked for.
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert_eq!(t.selected(cx), None);
    t.press_key_of(cx, "workspace::GoBack");
    let list = t.list(cx).expect("a list is open");
    assert_eq!(
        list.read_with(cx, |l, _| l.kind().clone()),
        KindId::new("", "services")
    );
    assert!(t.available(cx).contains(&"workspace::GoForward"));
    t.press_key_of(cx, "workspace::GoForward");
    t.feed_list(cx, pods(&[("demo", "api", 0), ("demo", "web", 0)]))
        .await;
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    // And back the same way.
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.press_key_of(cx, "workspace::GoBack");
    let list = t.list(cx).expect("a list is open");
    assert_eq!(
        list.read_with(cx, |l, _| l.kind().clone()),
        KindId::new("", "services")
    );
}

// covers #313: the cluster overview is a place: Go Back reaches it from a list opened there,
// and Go Forward returns from it.
#[gpui_kit::test]
async fn go_back_and_forward_reach_the_overview(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    cx.run_until_parked();
    t.dispatch(cx, "workspace::OpenClusterOverview", json!(null))
        .unwrap();
    let overview = |cx: &mut TestAppContext| {
        t.workspace
            .read_with(cx, |ws, _| ws.dashboard_view().is_some())
    };
    assert!(overview(cx));
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "pods"}))
        .unwrap();
    assert!(!overview(cx));
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.press_key_of(cx, "workspace::GoBack");
    assert!(overview(cx));
    assert!(t.list(cx).is_none());
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
    assert!(t.available(cx).contains(&"workspace::GoForward"));
    t.press_key_of(cx, "workspace::GoForward");
    assert!(!overview(cx));
    let list = t.list(cx).expect("a list is open");
    assert_eq!(list.read_with(cx, |l, _| l.kind().clone()), KindId::pods());
    // Forward from the list has nowhere to go; Back reaches the overview again.
    assert!(!t.available(cx).contains(&"workspace::GoForward"));
    t.press_key_of(cx, "workspace::GoBack");
    assert!(overview(cx));
}

// covers UP-33: Cmd-[ goes back to the kind and object shown before, then has nowhere to go.
#[gpui_kit::test]
async fn go_back_returns_to_the_kind_and_object_shown_before(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    pods_with_detail(cx, &t).await;
    start_history_here(cx, &t);
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[("demo", "front", 0)],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.press_key_of(cx, "workspace::GoBack");
    let list = t.list(cx).expect("a list is open");
    assert_eq!(list.read_with(cx, |l, _| l.kind().clone()), KindId::pods());
    t.feed_list(cx, pods(&[("demo", "api", 0), ("demo", "web", 0)]))
        .await;
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).detail_view().is_some()
    })
    .await;
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert_eq!(focus(cx, &t), Focus::List);
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
    // Back to the kind already open: the object is selected again, and the list has focus.
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    assert_eq!(focus(cx, &t), Focus::List);
    // A place whose kind the cluster no longer serves is no way back.
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "secrets"}))
        .unwrap();
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    let without_pods: Vec<_> = standard_sections()
        .into_iter()
        .map(|mut s| {
            s.entries.retain(|e| e.id != KindId::pods());
            s
        })
        .collect();
    t.feed(
        cx,
        flint_core::Event::DiscoveryChanged {
            cluster: "kind-alpha".into(),
            generation: crate::ui::support_tests::CURRENT_GENERATION,
            discovery: Some(std::sync::Arc::new(crate::ui::support_tests::discovery(
                without_pods,
            ))),
            error: None,
        },
    );
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
    t.feed(
        cx,
        flint_core::Event::DiscoveryChanged {
            cluster: "kind-alpha".into(),
            generation: crate::ui::support_tests::CURRENT_GENERATION,
            discovery: Some(std::sync::Arc::new(crate::ui::support_tests::discovery(
                standard_sections(),
            ))),
            error: None,
        },
    );
    // Another cluster starts over.
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.activate(cx, "kind-beta").await;
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
}

// covers UP-33: the cursor's ring moves without moving anything: every row keeps its border,
// so a row's contents sit where they were with the ring on or off.
#[gpui_kit::test]
async fn the_cursor_ring_shifts_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    let origin = |cx: &mut TestAppContext, id: &str| {
        t.find(cx, id.to_owned())
            .unwrap_or_else(|| panic!("{id} is drawn"))
            .bounds()
            .origin
    };
    let header = |cx: &mut TestAppContext| {
        t.find(cx, ("nav-section-label", 0usize))
            .expect("the Workloads header is drawn")
            .bounds()
            .origin
    };
    t.focus_workspace(cx);
    let (dot, label) = (
        origin(cx, "cluster-dot-kind-alpha"),
        origin(cx, "nav-label-pods"),
    );
    let title = header(cx);
    // The sidebar's cursor on kind-alpha, then the navigation's on the Workloads header and Pods.
    t.press(cx, "tab");
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("kind-alpha"));
    assert_eq!(origin(cx, "cluster-dot-kind-alpha"), dot);
    t.press(cx, "tab");
    assert_eq!(
        nav_cursor(&nav(cx, &t)).as_deref(),
        Some("section:Workloads")
    );
    assert_eq!(header(cx), title);
    t.press(cx, "down");
    assert_eq!(nav_cursor(&nav(cx, &t)).as_deref(), Some("kind:pods"));
    assert_eq!(header(cx), title);
    assert_eq!(origin(cx, "nav-label-pods"), label);
    assert_eq!(origin(cx, "cluster-dot-kind-alpha"), dot);
}

// covers #415: Go Back to another kind puts its filter back, in the field and on the rows.
#[gpui_kit::test]
async fn go_back_restores_the_list_filter(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let rows = [
        ("demo", "api", 0),
        ("demo", "web-a", 0),
        ("demo", "web-b", 0),
    ];
    t.open_pods(cx, &rows).await;
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "web");
    let list = t.list(cx).unwrap();
    cx.wait_for(t.window, WAIT, move |_, cx| list.read(cx).shown().1 == 2)
        .await;
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[("demo", "front", 0)],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    t.focus_workspace(cx);

    t.press_key_of(cx, "workspace::GoBack");
    let list = t.list(cx).expect("a list is open");
    assert_eq!(list.read_with(cx, |l, _| l.filter_text().to_owned()), "web");
    t.feed_list(cx, pods(&rows)).await;
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 2)
        .await;
    assert_eq!(t.selected(cx), None);
    assert!(t.detail(cx).is_none());
}

// covers #415: while the restored filter's rows are computed, the rows of the old filter are
// gone and the old selection is cleared, so nothing can be acted on by mistake.
#[gpui_kit::test]
async fn go_back_offers_no_stale_row_while_the_filter_is_restored(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(
        cx,
        &[
            ("demo", "api", 0),
            ("demo", "web-a", 0),
            ("demo", "web-b", 0),
        ],
    )
    .await;
    let list = t.list(cx).unwrap();
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "web");
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 2)
        .await;
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "web-a"}),
    )
    .unwrap();
    assert_eq!(t.selected(cx).as_deref(), Some("web-a"));
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "-b");
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 1)
        .await;

    let permits = list.update(cx, |list, _| list.hold_computes());
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    assert_eq!(list.read_with(cx, |l, _| l.filter_text().to_owned()), "web");
    assert_eq!(
        list.read_with(cx, |l, _| l.shown().1),
        0,
        "old rows dropped"
    );
    assert_eq!(t.selected(cx), None);
    assert!(t.detail(cx).is_none(), "no detail is left as a target");
    assert_eq!(
        list.read_with(cx, |l, _| l.view_state().placeholder),
        Some("loading"),
        "pending, not an empty result"
    );
    assert!(t.find(cx, "list-filtering").is_some());
    t.press(cx, "down");
    assert_eq!(t.selected(cx), None, "no row to move to yet");

    for _ in 0..8 {
        permits.try_send(()).unwrap();
    }
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 2)
        .await;
    assert_eq!(t.selected(cx), None, "the rows arriving select nothing");
    assert_eq!(list.read_with(cx, |l, _| l.view_state().placeholder), None);
    assert!(t.find(cx, "list-filtering").is_none());
}

// covers #415: rows computed for the text before the restore are never drawn.
#[gpui_kit::test]
async fn rows_of_the_old_text_are_dropped_after_a_restore(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let rows = [
        ("demo", "api", 0),
        ("demo", "web-a", 0),
        ("demo", "web-b", 0),
    ];
    t.open_pods(cx, &rows).await;
    let list = t.list(cx).unwrap();
    let permits = list.update(cx, |list, _| list.hold_computes());
    list.update(cx, |list, cx| list.set_snapshot(pods(&rows), cx));
    let restoring = list.clone();
    cx.update_window(t.window, |_, window, cx| {
        restoring.update(cx, |list, cx| list.restore_filter("web", window, cx));
    })
    .unwrap();

    // One permit: the old text's job ends, and the job replacing it stays gated.
    let done = list.read_with(cx, |l, _| l.computes_done());
    permits.try_send(()).unwrap();
    let finished = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        finished.read(cx).computes_done() > done
    })
    .await;
    assert_eq!(
        list.read_with(cx, |l, _| l.shown().1),
        0,
        "the job for the old text is not drawn"
    );
    for _ in 0..8 {
        permits.try_send(()).unwrap();
    }
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 2)
        .await;
}

// covers #415: the saved text typed again before its rows arrive is not yet on the rows: Go Back
// drops the rows drawn for the cleared filter, so Down cannot open a pod the filter excludes.
#[gpui_kit::test]
async fn go_back_to_retyped_text_offers_no_unfiltered_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(
        cx,
        &[
            ("demo", "api", 0),
            ("demo", "web-a", 0),
            ("demo", "web-b", 0),
        ],
    )
    .await;
    let list = t.list(cx).unwrap();
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "web");
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 2)
        .await;
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "web-a"}),
    )
    .unwrap();
    t.press_key_of(cx, "resource_list::FocusFilter");
    for _ in 0..3 {
        t.press(cx, "backspace");
    }
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 3)
        .await;

    let permits = list.update(cx, |list, _| list.hold_computes());
    t.type_text(cx, "web");
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    assert_eq!(
        list.read_with(cx, |l, _| l.shown().1),
        0,
        "unfiltered rows dropped"
    );
    t.press(cx, "down");
    assert_eq!(t.selected(cx), None, "no row to move to yet");

    for _ in 0..8 {
        permits.try_send(()).unwrap();
    }
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 2)
        .await;
    assert_eq!(t.selected(cx), None);
}

// covers #415: an object restored with a filter that excludes it keeps its details, and the
// list says why its row is missing once the restored rows are in.
#[gpui_kit::test]
async fn go_back_says_when_the_text_filter_hides_the_selection(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let rows = [("demo", "api", 0), ("demo", "web", 0)];
    t.open_pods(cx, &rows).await;
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "api"}),
    )
    .unwrap();
    t.press_key_of(cx, "resource_list::FocusFilter");
    t.type_text(cx, "web");
    let list = t.list(cx).unwrap();
    let shown = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| shown.read(cx).shown().1 == 1)
        .await;
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[("demo", "front", 0)],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    t.focus_workspace(cx);

    t.press_key_of(cx, "workspace::GoBack");
    let list = t.list(cx).expect("a list is open");
    t.feed_list(cx, pods(&rows)).await;
    let banner = list.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        let view = banner.read(cx).view_state();
        view.shown == 1 && view.banners.iter().any(|b| b.id == "list-filtered-out")
    })
    .await;
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    assert!(t.detail(cx).is_some());
    assert!(t.find(cx, "list-filtered-out").is_some());
}

// covers UP-33: Go Back to the kind already open, with nothing to select, still puts focus in
// the list.
#[gpui_kit::test]
async fn go_back_to_the_open_kind_focuses_the_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    assert_eq!(t.selected(cx), None);
    assert_eq!(focus(cx, &t), Focus::List);
}

// covers UP-33: selecting a row records nothing, so the place remembered last can be the one
// shown; Go Back skips it rather than spend a press staying put.
#[gpui_kit::test]
async fn go_back_skips_the_object_already_selected_again(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[("demo", "front", 0)],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    // Back to pods: web, then api (remembering web), then web selected by hand.
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "web"}),
    )
    .unwrap();
    t.feed_list(cx, pods(&[("demo", "api", 0), ("demo", "web", 0)]))
        .await;
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "api"}),
    )
    .unwrap();
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    assert_eq!(t.selected(cx).as_deref(), Some("web"));
    // The top of the history is pods/web, the place shown: one press goes past it.
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    let list = t.list(cx).expect("a list is open");
    assert_eq!(
        list.read_with(cx, |l, _| l.kind().clone()),
        KindId::new("", "services")
    );
}

// covers UP-33: clearing the selection records nothing either; with only the place shown left
// in the history, Go Back has nowhere to go.
#[gpui_kit::test]
async fn go_back_has_nowhere_to_go_from_the_place_remembered_last(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    start_history_here(cx, &t);
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.dispatch(cx, "resource_list::CloseDetail", serde_json::Value::Null)
        .unwrap();
    assert_eq!(t.selected(cx), None);
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    assert_eq!(t.selected(cx), None);
    assert_eq!(
        t.list(cx)
            .expect("a list is open")
            .read_with(cx, |l, _| l.kind().clone()),
        KindId::pods()
    );
}

// covers UP-33: an object opened before its row arrives waits for it; Go Back to the bare list
// still cancels that pick, though the list shown is that same bare list.
#[gpui_kit::test]
async fn go_back_cancels_an_object_still_waiting_for_its_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "pods"}))
        .unwrap();
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "api"}),
    )
    .unwrap();
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    t.feed_list(cx, pods(&[("demo", "api", 0)])).await;
    assert_eq!(t.selected(cx), None, "the pick was cancelled");
}

// covers UP-33: an object opened by name and found missing leaves a notice over the bare list;
// Go Back to the bare list still dismisses it.
#[gpui_kit::test]
async fn go_back_dismisses_an_object_found_missing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "gone"}),
    )
    .unwrap();
    t.feed_list(cx, pods(&[("demo", "api", 1)])).await;
    let list = t.list(cx).expect("a list is open");
    assert!(list.read_with(cx, |l, _| l.missing_text()).is_some());
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    assert_eq!(list.read_with(cx, |l, _| l.missing_text()), None);
}

// covers UP-33: the Go Back key where Go Back has nowhere to go leaves the history alone, so a
// later selection can still go back.
#[gpui_kit::test]
async fn go_back_with_nowhere_to_go_keeps_the_history(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0), ("demo", "web", 0)])
        .await;
    start_history_here(cx, &t);
    t.dispatch(
        cx,
        "workspace::OpenObject",
        json!({"kind": "pods", "namespace": "demo", "name": "api"}),
    )
    .unwrap();
    t.dispatch(cx, "resource_list::CloseDetail", serde_json::Value::Null)
        .unwrap();
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.focus_workspace(cx);
    t.press_key_of(cx, "workspace::GoBack");
    assert_eq!(t.selected(cx), None, "back to the bare list");
}

// covers UP-33, UP-55: on a protected cluster the sidebar's cursor ring leaves its lock button
// where it was, and the lock stays reachable from the keyboard: from the sidebar, the palette
// unlocks the cluster, asking first.
#[gpui_kit::test]
async fn the_lock_stays_reachable_from_the_sidebar(cx: &mut TestAppContext) {
    use crate::ui::support_tests::{alpha_and_beta, open_workspace_with};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "protect_local_clusters = true\n",
    )
    .unwrap();
    let t = open_workspace_with(cx, dir.path(), alpha_and_beta).await;
    let lock = |cx: &mut TestAppContext| {
        t.find(cx, "lock-kind-alpha")
            .expect("kind-alpha's lock button is drawn")
            .bounds()
    };
    t.focus_workspace(cx);
    let before = lock(cx);
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Sidebar);
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("kind-alpha"));
    assert_eq!(lock(cx), before, "the ring moved the lock button");

    t.press_key_of(cx, "palette::ToggleCommandPalette");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace
            .read(cx)
            .palette_view()
            .is_some_and(|p| !p.read(cx).view_state().rows.is_empty())
    })
    .await;
    t.type_text(cx, "unlock cluster");
    t.press(cx, "enter");
    t.type_text(cx, "kind-alpha");
    t.press(cx, "enter");
    cx.wait_for(t.window, WAIT, move |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    assert!(t.has_dialog(cx), "Unlock asks first, naming the server");
}

// covers UP-33: Enter on the kind already open in the navigation puts focus in its list, as
// opening it did.
#[gpui_kit::test]
async fn enter_on_the_open_kind_focuses_its_list(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "api", 0)]).await;
    t.focus_workspace(cx);
    t.press(cx, "tab");
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Navigation);
    assert_eq!(nav_cursor(&nav(cx, &t)).as_deref(), Some("kind:pods"));
    t.press(cx, "enter");
    assert_eq!(focus(cx, &t), Focus::List);
}

// covers UP-33: the sidebar's cursor brings its cluster into view: moving past the last row
// shown scrolls the clusters.
#[gpui_kit::test]
async fn the_sidebar_cursor_scrolls_into_view(cx: &mut TestAppContext) {
    use crate::ui::support_tests::{kubeconfig_yaml, open_workspace_with};

    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..40).map(|i| format!("ctx{i:02}")).collect();
    let t = open_workspace_with(cx, dir.path(), |dir, servers| {
        let contexts: Vec<(&str, &str)> = names
            .iter()
            .map(|n| (n.as_str(), servers.refused.as_str()))
            .collect();
        let path = dir.join("kubeconfig");
        std::fs::write(&path, kubeconfig_yaml(&contexts)).unwrap();
        vec![path]
    })
    .await;
    let shown = |cx: &mut TestAppContext, name: &str| {
        t.find(cx, format!("cluster-{name}"))
            .is_some_and(|row| row.visible())
    };
    assert!(
        !shown(cx, "ctx35"),
        "the window shows only the first clusters"
    );
    t.focus_workspace(cx);
    t.press(cx, "tab");
    for _ in 0..35 {
        t.press(cx, "down");
    }
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("ctx35"));
    assert!(
        shown(cx, "ctx35"),
        "the cursor's row was not brought into view"
    );
}

// covers UP-33: Tab to the list or the detail pane rings the pane with the focus ring, and the
// ring goes with focus; a click on a row still reaches the row under it.
#[gpui_kit::test]
async fn the_list_and_the_detail_pane_show_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    pods_with_detail(cx, &t).await;
    let rings = |cx: &mut TestAppContext| {
        (
            t.find(cx, "list-focus-ring").is_some(),
            t.find(cx, "detail-focus-ring").is_some(),
        )
    };
    t.focus_workspace(cx);
    assert_eq!(rings(cx), (false, false));
    t.press(cx, "tab");
    t.press(cx, "tab");
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::List);
    assert_eq!(rings(cx), (true, false));
    t.press(cx, "tab");
    assert_eq!(focus(cx, &t), Focus::Detail);
    assert_eq!(rings(cx), (false, true));
    t.press(cx, "tab");
    assert_eq!(rings(cx), (false, false));
    // The ring is drawn over the list's edge, not over its rows' clicks.
    t.press(cx, "shift-tab");
    t.press(cx, "shift-tab");
    assert_eq!(rings(cx), (true, false));
    t.click(cx, "row-uid-api");
    assert_eq!(t.selected(cx).as_deref(), Some("api"));
}
