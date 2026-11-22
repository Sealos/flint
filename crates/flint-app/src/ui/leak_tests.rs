//! Leak test for the window as a whole (see "Memory leaks" in docs/TESTING.md): closing it
//! releases the workspace and everything it made. Run with `GPUI_LEAK_DETECTION=1
//! LEAK_BACKTRACE=1` to see where a leaked handle was made.

use flint_core::{KindId, NavEntry};
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::json;

use crate::ui::registry;
use crate::ui::support_tests::{WAIT, open_workspace};

#[gpui_kit::test]
async fn a_closed_window_releases_the_workspace(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());

    cx.update_window(t.window, |_, window, cx| {
        registry::dispatch(
            "workspace::SelectCluster",
            json!({ "cluster": "kind-alpha" }),
            window,
            cx,
        )
    })
    .unwrap()
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some("kind-alpha")
    })
    .await;
    let pods = NavEntry {
        id: KindId::pods(),
        label: "Pods".into(),
        kind: "Pod".into(),
        api_version: "v1".into(),
        namespaced: true,
        listable: true,
        watchable: true,
    };
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.open_kind(&pods, window, cx))
    })
    .unwrap();
    cx.run_until_parked();

    let weak = t.workspace.downgrade();
    let window = t.window;
    drop(workspace);
    drop(t.workspace);
    drop(t.model);
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    weak.assert_released();
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
}

// The keyboard paths keep nothing alive (UP-31, UP-33): the resource switcher with the objects
// it read, the panes Tab moved through, and the places Go Back remembers.
#[gpui_kit::test]
async fn the_keyboard_paths_release_everything_with_the_window(cx: &mut TestAppContext) {
    use crate::ui::support_tests::{objects, pods};

    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    t.open_pods(cx, &[("demo", "web", 0)]).await;
    let stored = t.model.read_with(cx, |m, _| {
        m.core()
            .store_for_tests((*pods(&[("demo", "web", 0)])).clone())
    });
    t.dispatch(
        cx,
        "resource_list::SelectRow",
        json!({"namespace": "demo", "name": "web"}),
    )
    .unwrap();
    t.focus_workspace(cx);
    for _ in 0..5 {
        t.press(cx, "tab");
    }
    t.press_key_of(cx, "palette::ToggleResourceSwitcher");
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).palette_view().is_some()
    })
    .await;
    t.type_text(cx, "web");
    t.press(cx, "escape");
    t.dispatch(cx, "workspace::OpenView", json!({"kind": "services"}))
        .unwrap();
    t.feed_list(
        cx,
        objects(
            KindId::new("", "services"),
            &[],
            flint_core::SyncState::Synced,
            1,
        ),
    )
    .await;
    t.press_key_of(cx, "workspace::GoBack");
    drop(stored);

    let weak = t.workspace.downgrade();
    let window = t.window;
    drop(t);
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    weak.assert_released();
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
}
