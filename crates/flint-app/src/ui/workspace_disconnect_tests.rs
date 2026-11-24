//! View tests for Disconnect: it asks only while a live shell would end, says what ends, and
//! keeps an open set when its selected member leaves.

use flint_core::{ConnectionState, Event};
use gpui_kit::TestAppContext;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestAppContextExt as _;
use serde_json::json;

use super::super::super::dock_model::LiveSessions;
use super::super::super::support_tests::{WAIT, open_fake_shell_in, open_workspace};
use super::{confirm_dialog, done_note};

const DISCONNECT: &str = "workspace::DisconnectCluster";

#[test]
fn the_confirmation_says_what_ends_and_that_it_stays_disconnected() {
    let live = LiveSessions { shells: 1, logs: 2 };
    let dialog = confirm_dialog("prod", live);
    assert_eq!(dialog.title, "Disconnect prod?");
    assert!(dialog.text.contains("1 shell and 2 log streams"));
    assert!(dialog.text.contains("output stays in the dock"));
    assert!(dialog.text.contains("until you select it again"));
    assert!(dialog.default_cancel, "Enter must not end a shell");
}

#[test]
fn the_note_names_what_ended() {
    assert_eq!(
        done_note("dev", LiveSessions::default()),
        "Disconnected dev. Select it to connect again."
    );
    let note = done_note("dev", LiveSessions { shells: 0, logs: 1 });
    assert!(note.contains("ended 1 log stream"), "{note}");
}

#[gpui_kit::test]
async fn a_live_shell_is_asked_about_and_cancel_keeps_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    let (tab, _fake) = open_fake_shell_in(cx, &t, "web");

    t.dispatch(cx, DISCONNECT, json!({ "cluster": "kind-alpha" }))
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    t.dispatch(cx, "ui::Cancel", serde_json::Value::Null)
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| !window.has_active_dialog(cx))
        .await;

    let live = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock_view();
        let dock = dock.read(cx);
        assert!(dock.tab(tab).is_some());
        dock.live_sessions_on("kind-alpha", cx)
    });
    assert_eq!(live.shells, 1, "the shell goes on");
}

#[gpui_kit::test]
async fn confirming_ends_the_shell_and_keeps_its_tab(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    let (tab, _fake) = open_fake_shell_in(cx, &t, "web");

    t.dispatch(cx, DISCONNECT, json!({ "cluster": "kind-alpha" }))
        .unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    t.dispatch(cx, "ui::Confirm", serde_json::Value::Null)
        .unwrap();
    cx.run_until_parked();

    t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock_view();
        let dock = dock.read(cx);
        assert!(dock.tab(tab).is_some(), "the output stays");
        assert_eq!(dock.live_sessions_on("kind-alpha", cx).shells, 0);
    });
}

#[gpui_kit::test]
async fn the_selected_member_leaves_and_its_set_stays_open(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.dispatch(
        cx,
        "cluster_sets::ToggleClusterPicking",
        serde_json::Value::Null,
    )
    .unwrap();
    for cluster in ["kind-alpha", "kind-beta"] {
        t.dispatch(
            cx,
            "cluster_sets::TogglePickedCluster",
            json!({ "cluster": cluster }),
        )
        .unwrap();
    }
    t.dispatch(
        cx,
        "cluster_sets::OpenPickedClusters",
        serde_json::Value::Null,
    )
    .unwrap();
    t.wait_for_element(cx, "cluster-set-view").await;
    t.feed(cx, Event::ActiveChanged(Some("kind-alpha".into())));

    t.dispatch(cx, DISCONNECT, json!({ "cluster": "kind-alpha" }))
        .unwrap();
    t.feed(cx, Event::ActiveChanged(None));
    cx.run_until_parked();

    t.workspace.read_with(cx, |ws, _| {
        assert!(ws.sets.is_open(), "the other member's rows stay");
        assert!(ws.sets.member_disconnected("kind-alpha"));
        assert!(!ws.sets.member_disconnected("kind-beta"));
    });
}

#[gpui_kit::test]
async fn with_nothing_running_it_is_refused(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.feed(
        cx,
        Event::ConnectionChanged {
            cluster: "kind-beta".into(),
            state: ConnectionState::Disconnected,
        },
    );
    let error = t
        .dispatch(cx, DISCONNECT, json!({ "cluster": "kind-beta" }))
        .unwrap_err();
    assert!(error.to_string().contains("not connected"), "{error}");
}
