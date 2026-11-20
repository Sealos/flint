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
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("alpha"));
    t.press(cx, "down");
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("beta"));
    t.press(cx, "down");
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("beta"));
    t.press(cx, "up");
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("alpha"));
    t.press(cx, "enter");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).shown_cluster() == Some("alpha")
    })
    .await;
    assert_eq!(focus(cx, &t), Focus::Sidebar);
    // The keys are the sidebar's only while it has focus.
    let available = t.available(cx);
    assert!(available.contains(&"sidebar::SelectNextCluster"));
    t.focus_workspace(cx);
    assert!(!t.available(cx).contains(&"sidebar::SelectNextCluster"));
    // A cluster picked another way moves the cursor too.
    t.activate(cx, "beta").await;
    assert_eq!(sidebar_cursor(cx, &t).as_deref(), Some("beta"));
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

// covers UP-33: Cmd-[ goes back to the kind and object shown before, then has nowhere to go.
#[gpui_kit::test]
async fn go_back_returns_to_the_kind_and_object_shown_before(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    pods_with_detail(cx, &t).await;
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
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
    // Another cluster starts over.
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    assert!(t.available(cx).contains(&"workspace::GoBack"));
    t.activate(cx, "beta").await;
    assert!(!t.available(cx).contains(&"workspace::GoBack"));
}
