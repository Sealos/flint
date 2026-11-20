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
            json!({ "cluster": "alpha" }),
            window,
            cx,
        )
    })
    .unwrap()
    .unwrap();
    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).cluster.as_deref() == Some("alpha")
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
