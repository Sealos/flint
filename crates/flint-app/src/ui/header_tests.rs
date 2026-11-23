//! View tests for the main area's header: the namespace filter (its button, popover and
//! actions, remembered per cluster) and the watch mode switch.

use flint_core::{ClusterId, ConnectionState, KindId, Settings, SyncState, WatchMode};
use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use crate::ui::registry::{ActionError, ToggleNamespace};
use crate::ui::support_tests::{
    TestWorkspace, WAIT, objects, open_workspace, pods, standard_sections,
};
use crate::ui::ui_state::UiState;
use crate::ui::view_state::Focus;

const ROWS: &[(&str, &str, i64)] = &[
    ("demo", "api", 0),
    ("demo", "web", 1),
    ("data", "db", 2),
    ("kube-system", "dns", 0),
];

fn filter(cx: &mut TestAppContext, t: &TestWorkspace, cluster: &str) -> Vec<String> {
    let cluster: ClusterId = cluster.into();
    t.workspace
        .read_with(cx, |ws, _| ws.namespace_filter_of(&cluster))
}

async fn wait_for_rows(cx: &mut TestAppContext, t: &TestWorkspace, rows: usize) {
    let workspace = t.workspace.clone();
    gpui_kit::test::TestAppContextExt::wait_for(cx, t.window, WAIT, move |_, cx| {
        workspace
            .read(cx)
            .list_view()
            .is_some_and(|l| l.read(cx).shown().1 == rows)
    })
    .await;
}

// covers UP-21: the namespace filter narrows the list from the registry and from its popover
// (a checkbox, a typed name and Enter, "All namespaces"). Its button names the choice, and the
// choice is remembered per cluster, also in state.toml.
#[gpui_kit::test]
async fn the_namespace_filter_narrows_the_list_and_is_remembered(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.open_kind(cx, "pods", pods(ROWS)).await;
    assert_eq!(
        t.label(cx, "namespace-filter-button").as_deref(),
        Some("All namespaces")
    );
    assert_eq!(t.row_count(cx), 4);

    t.dispatch(
        cx,
        "workspace::ToggleNamespace",
        json!({"namespace": "demo"}),
    )
    .unwrap();
    wait_for_rows(cx, &t, 2).await;
    t.wait_for_label(cx, "namespace-filter-button", "demo")
        .await;
    t.dispatch(
        cx,
        "workspace::AddNamespace",
        json!({"namespace": " data "}),
    )
    .unwrap();
    wait_for_rows(cx, &t, 3).await;
    t.wait_for_label(cx, "namespace-filter-button", "2 namespaces")
        .await;
    assert_eq!(filter(cx, &t, "kind-alpha"), ["data", "demo"]);

    // The popover: "All namespaces" is a checkbox, a name typed in its field is added with
    // Enter, and each filtered namespace has a checkbox.
    t.click(cx, "namespace-filter-button");
    t.wait_for_element(cx, "ns-all").await;
    t.click(cx, "ns-all");
    wait_for_rows(cx, &t, 4).await;
    assert!(filter(cx, &t, "kind-alpha").is_empty());
    for namespace in ["kube-system", "demo"] {
        t.click(cx, "ns-add");
        t.type_text(cx, namespace);
        t.press(cx, "enter");
    }
    wait_for_rows(cx, &t, 3).await;
    assert_eq!(filter(cx, &t, "kind-alpha"), ["demo", "kube-system"]);
    assert_eq!(t.find(cx, "ns-kube-system").unwrap().checked(), Some(true));
    t.click(cx, "ns-kube-system");
    wait_for_rows(cx, &t, 2).await;
    assert_eq!(filter(cx, &t, "kind-alpha"), ["demo"]);
    t.press(cx, "escape");

    // Remembered per cluster: kind-beta has its own (none; as its connection failed, its header has no
    // filter), and kind-alpha gets its own back, shown
    // as soon as the list reopens, before any rows arrive.
    t.activate(cx, "kind-beta").await;
    assert!(filter(cx, &t, "kind-beta").is_empty());
    t.connect_alpha(cx, standard_sections()).await;
    assert_eq!(t.row_count(cx), 0);
    assert_eq!(
        t.label(cx, "namespace-filter-button").as_deref(),
        Some("demo")
    );
    t.feed_list(cx, pods(ROWS)).await;
    assert_eq!(t.row_count(cx), 2);
    // And in state.toml, for the next launch.
    cx.run_until_parked();
    let saved = UiState::load(dir.path().to_owned());
    assert_eq!(saved.namespace_filter("kind-alpha"), ["demo"]);
    assert!(saved.namespace_filter("kind-beta").is_empty());

    // Blank names are refused; ShowAllNamespaces clears the filter.
    assert!(matches!(
        t.dispatch(cx, "workspace::ToggleNamespace", json!({"namespace": " "})),
        Err(ActionError::Rejected { .. })
    ));
    t.dispatch(cx, "workspace::ShowAllNamespaces", Value::Null)
        .unwrap();
    wait_for_rows(cx, &t, 4).await;
}

// covers UP-21: a cluster-scoped kind has no namespaces to filter, so the header drops the
// filter while it is open.
#[gpui_kit::test]
async fn cluster_scoped_kinds_have_no_namespace_filter(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    // Before a list is open, the filter shows.
    assert!(t.find(cx, "namespace-filter-button").is_some());
    t.open_kind(
        cx,
        "nodes",
        objects(
            KindId::new("", "nodes"),
            &[("", "node-1", 0)],
            SyncState::Synced,
            1,
        ),
    )
    .await;
    assert!(t.find(cx, "namespace-filter-button").is_none());
    t.open_kind(cx, "pods", pods(ROWS)).await;
    assert!(t.find(cx, "namespace-filter-button").is_some());
}

// covers UP-25: the watch mode switch sets this cluster's mode or the default, from its menu or
// the registry. Its label shows the mode in effect, and the choice is written to
// settings.toml.
#[gpui_kit::test]
async fn the_watch_mode_follows_the_menu_and_persists(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    // No cluster: only the default can be set.
    let available = t.available(cx);
    assert!(!available.contains(&"workspace::WatchOpenViewsOnly"));
    assert!(available.contains(&"workspace::DefaultWatchOpenViewsOnly"));

    t.activate(cx, "kind-alpha").await;
    assert_eq!(
        t.label(cx, "watch-mode").as_deref(),
        Some("Live: all kinds")
    );
    // The menu, as the user opens it: this cluster's choices, then the default's.
    t.click(cx, "watch-mode");
    let items = |cx: &mut TestAppContext, label: &'static str| {
        cx.update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            window.find_all_by_label(label).len()
        })
        .unwrap()
    };
    assert_eq!(items(cx, "Use the default (all kinds)"), 1);
    assert_eq!(
        items(cx, "All kinds"),
        2,
        "this cluster's and the default's"
    );
    assert_eq!(items(cx, "Open views only"), 2);
    t.press(cx, "escape");

    t.dispatch(cx, "workspace::WatchOpenViewsOnly", Value::Null)
        .unwrap();
    t.wait_for_label(cx, "watch-mode", "Live: open views only")
        .await;
    let settings = t.model.read_with(cx, |m, _| m.settings.clone());
    assert_eq!(
        settings.cluster_watch_mode.get("kind-alpha"),
        Some(&WatchMode::OnView)
    );
    assert_eq!(settings.watch_mode, WatchMode::All);
    let toml = t.settings_toml();
    assert!(
        toml.contains("kind-alpha") && toml.contains("on_view"),
        "{toml}"
    );
    // And back to all kinds, for this cluster only.
    t.dispatch(cx, "workspace::WatchAllKinds", Value::Null)
        .unwrap();
    t.wait_for_label(cx, "watch-mode", "Live: all kinds").await;
    let own = t.model.read_with(cx, |m, _| {
        m.settings.cluster_watch_mode.get("kind-alpha").copied()
    });
    assert_eq!(own, Some(WatchMode::All));
    t.dispatch(cx, "workspace::WatchOpenViewsOnly", Value::Null)
        .unwrap();
    t.wait_for_label(cx, "watch-mode", "Live: open views only")
        .await;

    // On a failed cluster nothing is watched, so its header has no switch; the default can still
    // be changed from there.
    t.activate(cx, "kind-beta").await;
    t.wait_for_state(cx, "kind-beta", |s| matches!(s, ConnectionState::Failed(_)))
        .await;
    assert!(t.find(cx, "watch-mode").is_none());
    t.dispatch(cx, "workspace::DefaultWatchOpenViewsOnly", Value::Null)
        .unwrap();
    let model = t.model.clone();
    gpui_kit::test::TestAppContextExt::wait_for(cx, t.window, WAIT, move |_, cx| {
        model.read(cx).settings.watch_mode == WatchMode::OnView
    })
    .await;

    // Back on kind-alpha, drop its own mode: it follows the default (open views only), and the
    // default back to all kinds takes it along.
    t.activate(cx, "kind-alpha").await;
    t.dispatch(cx, "workspace::UseDefaultWatchMode", Value::Null)
        .unwrap();
    let model = t.model.clone();
    gpui_kit::test::TestAppContextExt::wait_for(cx, t.window, WAIT, move |_, cx| {
        !model
            .read(cx)
            .settings
            .cluster_watch_mode
            .contains_key("kind-alpha")
    })
    .await;
    t.dispatch(cx, "workspace::DefaultWatchAllKinds", Value::Null)
        .unwrap();
    t.wait_for_label(cx, "watch-mode", "Live: all kinds").await;
    let saved: Settings = t.model.read_with(cx, |m, _| (*m.settings).clone());
    assert_eq!(saved.watch_mode, WatchMode::All);
    assert!(saved.cluster_watch_mode.is_empty());
}

// #432: the header says the access mode in words, from the model's protection, and follows it.
#[gpui_kit::test]
async fn the_header_names_the_access_mode(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    let shown = t.label(cx, "access-mode");
    let protection = t
        .model
        .read_with(cx, |m, _| m.protection(m.active_cluster().unwrap()).name());
    let expected = match protection {
        "protected" => "Read-only",
        "local" => "Local: writable",
        "unlocked" => "Unlocked: writable",
        "trusted" => "Trusted: writable",
        other => panic!("unknown protection {other}"),
    };
    assert_eq!(shown.as_deref(), Some(expected));

    // Another server, so no longer a local cluster: the label follows the real state.
    t.model.update(cx, |model, cx| {
        let alpha = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        alpha.info.server = Some("https://203.0.113.10:6443".into());
        cx.notify();
    });
    t.wait_for_label(cx, "access-mode", "Read-only").await;
}

// The header never says writable while the core refuses writes on a provisional definition.
#[gpui_kit::test]
async fn the_header_says_writes_are_paused(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    let before = t.label(cx, "access-mode").unwrap();
    assert_ne!(before, "Writes paused");

    let set_provisional = |provisional, cx: &mut TestAppContext| {
        t.model.update(cx, |model, cx| {
            let alpha = model
                .clusters
                .iter_mut()
                .find(|c| c.info.name == "kind-alpha")
                .unwrap();
            alpha.provisional = provisional;
            cx.notify();
        });
    };
    set_provisional(true, cx);
    t.wait_for_label(cx, "access-mode", "Writes paused").await;
    set_provisional(false, cx);
    t.wait_for_label(cx, "access-mode", &before).await;
}

// Waiting for the kubeconfig is not a failed connection: it says so and offers no Retry.
#[gpui_kit::test]
async fn waiting_for_confirmation_is_not_a_failure(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.model.update(cx, |model, cx| {
        let alpha = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        alpha.state = ConnectionState::AwaitingConfirmation;
        cx.notify();
    });
    t.wait_for_label(
        cx,
        "connection-waiting",
        "Waiting for kubeconfig to be confirmed",
    )
    .await;
    assert!(t.find(cx, "connection-retry").is_none());
    assert!(t.find(cx, "nav-retry").is_none());
    assert!(!t.available(cx).contains(&"workspace::RetryConnection"));
}

// #431: when the active context is re-pointed, the main area names both servers, without what a
// kubeconfig URL may hold besides the address. Choosing another context clears it.
#[gpui_kit::test]
async fn a_re_pointed_context_names_both_servers(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.connect_alpha(cx, standard_sections()).await;
    assert!(t.find(cx, "context-repointed").is_none());
    let old = t.model.read_with(cx, |m, _| {
        let server = m.active_cluster().unwrap().info.server.as_deref();
        flint_core::redact::server_url(server.unwrap())
    });

    t.model.update(cx, |model, cx| {
        let alpha = model
            .clusters
            .iter_mut()
            .find(|c| c.info.name == "kind-alpha")
            .unwrap();
        alpha.info.server = Some("https://admin:hunter2@203.0.113.10:6443/?token=abc".into());
        alpha.endpoint += 1;
        alpha.confirmed = true;
        cx.notify();
    });
    t.wait_for_element(cx, "context-repointed").await;
    let text = t.label(cx, "context-repointed").unwrap();
    assert!(text.contains("kind-alpha"), "{text}");
    assert!(text.contains(&format!("Was {old},")), "{text}");
    assert!(text.contains("now https://203.0.113.10:6443."), "{text}");
    assert!(!text.contains("hunter2") && !text.contains("abc"), "{text}");

    t.activate(cx, "kind-beta").await;
    cx.run_until_parked();
    assert!(t.find(cx, "context-repointed").is_none());
}

/// The workspace on the pods list, with `demo` in the filter so the picker has a namespace row.
async fn open_with_picker_rows(cx: &mut TestAppContext, dir: &std::path::Path) -> TestWorkspace {
    let t = open_workspace(cx, dir).await;
    t.connect_alpha(cx, standard_sections()).await;
    t.open_kind(cx, "pods", pods(ROWS)).await;
    t.dispatch(
        cx,
        "workspace::ToggleNamespace",
        json!({"namespace": "demo"}),
    )
    .unwrap();
    wait_for_rows(cx, &t, 2).await;
    t
}

fn is_focused(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) -> bool {
    t.find(cx, id).and_then(|e| e.focused()) == Some(true)
}

#[track_caller]
fn assert_focus(cx: &mut TestAppContext, t: &TestWorkspace, id: &'static str) {
    cx.run_until_parked();
    // The text field's focus is on its state, not on the element with its id.
    let focused = if id == "ns-add" {
        let workspace = t.workspace.clone();
        cx.update_window(t.window, |_, window, cx| {
            window.render_frame(cx);
            workspace
                .update(cx, |ws, cx| ws.view_state(window, cx))
                .focus
                == Focus::NamespaceInput
        })
        .unwrap()
    } else {
        is_focused(cx, t, id)
    };
    assert!(focused, "focus should be on {id}");
}

// covers UP-33: the picker takes focus as it opens, and Tab, Shift-Tab and the arrows go round
// its controls, also from its text field, and never reach the panes behind it.
#[gpui_kit::test]
async fn the_namespace_picker_keeps_the_keys_inside(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_picker_rows(cx, dir.path()).await;
    t.click(cx, "namespace-filter-button");
    t.wait_for_element(cx, "ns-all").await;
    assert_focus(cx, &t, "ns-all");

    for (forward, back) in [("tab", "shift-tab"), ("down", "up")] {
        for id in ["ns-demo", "ns-add", "ns-all"] {
            t.press(cx, forward);
            assert_focus(cx, &t, id);
        }
        for id in ["ns-add", "ns-demo", "ns-all"] {
            t.press(cx, back);
            assert_focus(cx, &t, id);
        }
    }
    assert!(t.find(cx, "ns-all").is_some(), "the picker stays open");
}

// covers UP-33: the picker works from the keyboard alone: Space and Enter toggle the checkbox
// with focus, and a name typed in its field is added with Enter.
#[gpui_kit::test]
async fn the_namespace_picker_works_from_the_keyboard(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_picker_rows(cx, dir.path()).await;
    // Listed, so its row stays when it leaves the filter.
    t.workspace.update(cx, |ws, cx| {
        ws.namespaces = vec!["demo".to_owned()];
        cx.notify();
    });
    t.dispatch(cx, "workspace::OpenNamespaceFilter", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "ns-all").await;
    assert_focus(cx, &t, "ns-all");

    t.press(cx, "tab");
    assert_focus(cx, &t, "ns-demo");
    t.press(cx, "space");
    wait_for_rows(cx, &t, 4).await;
    assert!(filter(cx, &t, "kind-alpha").is_empty());
    t.press(cx, "enter");
    wait_for_rows(cx, &t, 2).await;
    assert_eq!(filter(cx, &t, "kind-alpha"), ["demo"]);

    t.press(cx, "tab");
    assert_focus(cx, &t, "ns-add");
    t.type_text(cx, "data");
    t.press(cx, "enter");
    wait_for_rows(cx, &t, 3).await;
    assert_eq!(filter(cx, &t, "kind-alpha"), ["data", "demo"]);
}

// covers UP-33: unchecking a namespace that is not listed removes its row; focus stays in the
// picker, so Tab and Esc still work.
#[gpui_kit::test]
async fn focus_stays_in_the_picker_when_its_row_goes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_picker_rows(cx, dir.path()).await;
    t.dispatch(cx, "workspace::OpenNamespaceFilter", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "ns-all").await;
    assert_focus(cx, &t, "ns-all");
    t.press(cx, "tab");
    assert_focus(cx, &t, "ns-demo");
    t.press(cx, "space");
    wait_for_rows(cx, &t, 4).await;
    assert!(t.find(cx, "ns-demo").is_none(), "the row is gone");
    cx.run_until_parked();
    assert_focus(cx, &t, "ns-all");
    t.press(cx, "tab");
    assert_focus(cx, &t, "ns-add");
    t.press(cx, "escape");
    assert!(t.find(cx, "ns-all").is_none());
    assert_focus(cx, &t, "namespace-filter-button");
}

// covers UP-33: the handoff from a removed row waits for the draw to finish: a focus change made
// during a draw schedules no frame, which would leave the picker with no focus ring.
#[gpui_kit::test]
async fn a_removed_row_hands_focus_on_after_the_draw(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_picker_rows(cx, dir.path()).await;
    t.dispatch(cx, "workspace::OpenNamespaceFilter", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "ns-all").await;
    assert_focus(cx, &t, "ns-all");
    t.press(cx, "tab");
    assert_focus(cx, &t, "ns-demo");
    // What Space does, without the frame a test key press draws after it.
    cx.update_window(t.window, |_, window, cx| {
        let namespace = "demo".to_owned();
        window.dispatch_action(Box::new(ToggleNamespace { namespace }), cx);
    })
    .unwrap();

    let picker = t
        .workspace
        .read_with(cx, |ws, _| ws.namespace_picker_focus.clone());
    cx.update_window(t.window, |_, window, cx| {
        // The frame without the row.
        window.draw(cx).clear(cx);
        assert!(window.try_find("ns-demo").is_none(), "the row is gone");
        assert!(!picker.is_focused(window), "focus moved during the draw");
    })
    .unwrap();
    cx.run_until_parked();
    assert_focus(cx, &t, "ns-all");
}

// covers UP-33: with more namespaces than fit, each keyboard move shows the row it lands on, in
// both directions and across the wraparound.
#[gpui_kit::test]
async fn keyboard_moves_keep_the_focused_namespace_in_view(cx: &mut TestAppContext) {
    const ROW_COUNT: usize = 40;
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_picker_rows(cx, dir.path()).await;
    let names: Vec<String> = (0..ROW_COUNT).map(|i| format!("ns{i:02}")).collect();
    let ids: Vec<&'static str> = names
        .iter()
        .map(|n| &*String::leak(format!("ns-{n}")))
        .collect();
    t.dispatch(cx, "workspace::ShowAllNamespaces", Value::Null)
        .unwrap();
    t.workspace.update(cx, |ws, cx| {
        ws.namespaces = names.clone();
        cx.notify();
    });
    t.dispatch(cx, "workspace::OpenNamespaceFilter", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "ns-all").await;
    assert_focus(cx, &t, "ns-all");

    let assert_shown = |cx: &mut TestAppContext, id: &'static str| {
        assert_focus(cx, &t, id);
        // The scroll lands in the frame after the move.
        let row = t.find(cx, id).unwrap().bounds();
        let list = t
            .workspace
            .read_with(cx, |ws, _| ws.namespace_list_scroll.bounds());
        assert!(
            row.top() >= list.top() && row.bottom() <= list.bottom(),
            "{id} at {row:?} is outside {list:?}"
        );
    };
    for id in &ids {
        t.press(cx, "down");
        assert_shown(cx, id);
    }
    t.press(cx, "tab");
    assert_focus(cx, &t, "ns-add");
    t.press(cx, "tab");
    assert_focus(cx, &t, "ns-all");
    // Back round the wrap, onto the last row from the field.
    t.press(cx, "shift-tab");
    assert_focus(cx, &t, "ns-add");
    for id in ids.iter().rev() {
        t.press(cx, "shift-tab");
        assert_shown(cx, id);
    }
    t.press(cx, "up");
    assert_focus(cx, &t, "ns-all");
    t.press(cx, "up");
    assert_focus(cx, &t, "ns-add");
    t.press(cx, "up");
    assert_shown(cx, ids[ROW_COUNT - 1]);
}

// covers UP-33: Esc closes the picker from a checkbox or its field, and focus is on its button.
#[gpui_kit::test]
async fn esc_closes_the_namespace_picker_onto_its_button(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_picker_rows(cx, dir.path()).await;
    for steps in [0, 2] {
        t.click(cx, "namespace-filter-button");
        t.wait_for_element(cx, "ns-all").await;
        cx.run_until_parked();
        for _ in 0..steps {
            t.press(cx, "tab");
        }
        t.press(cx, "escape");
        assert!(t.find(cx, "ns-all").is_none(), "closed after {steps} tabs");
        assert_focus(cx, &t, "namespace-filter-button");
    }
}

// covers UP-33: the header's buttons are not Tab stops, and the picker opens from the palette's
// action with focus inside it.
#[gpui_kit::test]
async fn header_buttons_are_off_the_tab_order(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_with_picker_rows(cx, dir.path()).await;
    t.press_key_of(cx, "resource_list::FocusFilter");
    for _ in 0..8 {
        t.press(cx, "tab");
        assert!(!is_focused(cx, &t, "namespace-filter-button"));
        assert!(!is_focused(cx, &t, "watch-mode"));
    }
    t.dispatch(cx, "workspace::OpenNamespaceFilter", Value::Null)
        .unwrap();
    t.wait_for_element(cx, "ns-all").await;
    assert_focus(cx, &t, "ns-all");
}
