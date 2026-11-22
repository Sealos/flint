//! View tests for a cluster's agent activity (UP-16): the AI tag opens it in the dock, it shows
//! the cluster's calls newest first, and it updates while an agent works.

use std::sync::Arc;

use flint_core::activity::{ActivityRecord, Outcome};
use flint_core::settings::AiScope;
use gpui_kit::TestAppContext;
use gpui_kit::test::TestAppContextExt as _;
use serde_json::json;

use crate::ui::activity_view::{SETUP_NOTE, TEST_PROMPT, register_command};
use crate::ui::dock::DockTab;
use crate::ui::registry::ActionError;
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace};

fn record(at_ms: i64, cluster: &str, tool: &str) -> ActivityRecord {
    ActivityRecord {
        at_ms,
        tool: tool.into(),
        cluster: Some(cluster.into()),
        server: None,
        kind: Some("pods".into()),
        namespace: Some("shop".into()),
        name: None,
        container: None,
        items: Some(4),
        bytes: 2048,
        truncated: false,
        outcome: Outcome::Ok,
        cluster_key: Some(flint_core::activity::cluster_key(cluster)),
        server_key: None,
        probed: Vec::new(),
        probed_more: 0,
    }
}

/// Sets the activity log as the core reports it.
fn set_activity(cx: &mut TestAppContext, t: &TestWorkspace, records: Vec<ActivityRecord>) {
    t.model.update(cx, |m, cx| {
        m.agent_activity = Arc::new(records);
        cx.notify();
    });
}

/// The labels of the activity tab's rows, top to bottom; the first is checked as drawn.
fn row_labels(cx: &mut TestAppContext, t: &TestWorkspace) -> Vec<String> {
    let rows: Vec<String> = t.workspace.read_with(cx, |ws, cx| {
        let dock = ws.dock.read(cx);
        dock.tab_ids()
            .into_iter()
            .find_map(|id| match dock.tab(id)? {
                DockTab::Activity(view) => {
                    Some(view.read(cx).rows().iter().map(|r| r.label()).collect())
                }
                _ => None,
            })
            .unwrap_or_default()
    });
    if let Some(first) = rows.first() {
        assert_eq!(t.label(cx, ("activity-row", 0usize)).as_ref(), Some(first));
    }
    rows
}

fn dock_tabs(cx: &mut TestAppContext, t: &TestWorkspace) -> usize {
    t.workspace
        .read_with(cx, |ws, cx| ws.dock.read(cx).tab_ids().len())
}

// covers UP-16: the AI tag opens the cluster's agent activity: its calls only, newest first,
// with what each named and got. A new call shows while the tab is open, and the tag shows the
// same tab again. Edit AI Scope is one click away.
#[gpui_kit::test]
async fn the_ai_tag_shows_what_agents_asked_for(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let server = t.server.clone();
    t.model.update(cx, |m, _| {
        m.expose_to_ai("kind-alpha".into(), server, AiScope::default())
    });
    t.wait_for_element(cx, "ai-scope-kind-alpha").await;
    let tip = t.description(cx, "ai-scope-kind-alpha").unwrap_or_default();
    assert!(tip.ends_with("Click to see what they asked for."), "{tip}");
    set_activity(
        cx,
        &t,
        vec![
            record(1_000, "kind-alpha", "list_resources"),
            record(2_000, "kind-beta", "get_events"),
            ActivityRecord {
                name: Some("web-0".into()),
                outcome: Outcome::Denied,
                ..record(3_000, "kind-alpha", "get_pod_logs")
            },
        ],
    );

    t.click(cx, "ai-scope-kind-alpha");
    t.wait_for_label(cx, "activity-count", "2 calls").await;
    let rows = row_labels(cx, &t);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows[0].contains("get_pod_logs pods shop/web-0 — denied"),
        "{rows:?}"
    );
    assert!(
        rows[1].contains("list_resources pods in shop — 4 items, 2.0 KB"),
        "{rows:?}"
    );
    assert_eq!(dock_tabs(cx, &t), 1);

    // An agent calls again: the open tab shows it on top.
    t.model.update(cx, |m, cx| {
        let mut records = (*m.agent_activity).clone();
        records.push(record(4_000, "kind-alpha", "get_resource"));
        m.agent_activity = Arc::new(records);
        cx.notify();
    });
    t.wait_for_label(cx, "activity-count", "3 calls").await;
    assert!(row_labels(cx, &t)[0].contains("get_resource"));

    // The tag shows the same tab again.
    t.click(cx, "ai-scope-kind-alpha");
    cx.run_until_parked();
    assert_eq!(dock_tabs(cx, &t), 1);

    t.click(cx, "activity-edit-scope");
    cx.wait_for(t.window, WAIT, |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    t.press(cx, "escape");
    cx.wait_for(t.window, WAIT, |window, cx| {
        !gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;

    // The context now points at another server: this tab is for the old one, so it offers no
    // scope to edit (that would be the new server's).
    t.model.update(cx, |m, cx| {
        if let Some(c) = m.clusters.iter_mut().find(|c| c.info.name == "kind-alpha") {
            c.info.server = Some("https://moved.example:6443".into());
        }
        cx.notify();
    });
    cx.run_until_parked();
    assert!(t.find(cx, "activity-edit-scope").is_none());
}

// covers UP-16: any context has an activity view, exposed or not (a cluster hidden again still
// shows what agents read while it was exposed); one never called says so.
#[gpui_kit::test]
async fn show_agent_activity_works_for_any_context(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    assert!(matches!(
        t.dispatch(
            cx,
            "workspace::ShowAgentActivity",
            json!({"cluster": "nope"})
        ),
        Err(ActionError::Rejected { .. })
    ));
    t.dispatch(
        cx,
        "workspace::ShowAgentActivity",
        json!({"cluster": "kind-beta"}),
    )
    .unwrap();
    t.wait_for_element(cx, "activity-empty").await;
    assert_eq!(t.label(cx, "activity-count").as_deref(), Some("0 calls"));
    // Not exposed: no scope to edit from here.
    assert!(t.find(cx, "activity-edit-scope").is_none());
    set_activity(
        cx,
        &t,
        vec![record(1_000, "kind-beta", "list_resource_kinds")],
    );
    t.wait_for_label(cx, "activity-count", "1 call").await;
    assert!(t.find(cx, "activity-empty").is_none());
}

// covers UP-16: an exposed cluster no agent has called says how to connect one, and the test
// prompt copies from the keyboard: Tab reaches the tab's copy button, Enter presses it. The steps
// go away with the first call; a cluster that is not exposed never shows them.
#[gpui_kit::test]
async fn an_exposed_cluster_without_calls_shows_setup_steps(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.dispatch(
        cx,
        "workspace::ShowAgentActivity",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    t.wait_for_element(cx, "activity-empty").await;
    assert!(t.find(cx, "activity-setup").is_none());

    let server = t.server.clone();
    t.model.update(cx, |m, cx| {
        m.expose_to_ai("kind-alpha".into(), server, AiScope::default());
        cx.notify();
    });
    t.wait_for_element(cx, "activity-setup").await;
    assert_eq!(
        t.label(cx, "activity-test-prompt").as_deref(),
        Some(TEST_PROMPT)
    );
    assert_eq!(
        t.label(cx, "activity-setup-note").as_deref(),
        Some(SETUP_NOTE)
    );

    t.focus_workspace(cx);
    for _ in 0..8 {
        if t.find(cx, "activity-copy-prompt")
            .is_some_and(|b| b.focused() == Some(true))
        {
            break;
        }
        t.press(cx, "tab");
    }
    assert_eq!(
        t.find(cx, "activity-copy-prompt").unwrap().focused(),
        Some(true)
    );
    t.press(cx, "enter");
    assert_eq!(
        cx.read_from_clipboard().and_then(|i| i.text()).as_deref(),
        Some(TEST_PROMPT)
    );

    set_activity(
        cx,
        &t,
        vec![record(1_000, "kind-alpha", "list_resource_kinds")],
    );
    t.wait_for_label(cx, "activity-count", "1 call").await;
    assert!(t.find(cx, "activity-setup").is_none());
    assert!(t.find(cx, "activity-copy-prompt").is_none());
}

// The command names the executable running now, in the README's form for a stable path.
#[test]
fn the_setup_command_registers_the_given_server() {
    let readme = include_str!("../../../../README.md");
    assert!(readme.contains(&register_command("/usr/local/bin/flint mcp")));
    assert_eq!(
        register_command("'/home/me/My Flint.AppImage' mcp"),
        "claude mcp add flint -- '/home/me/My Flint.AppImage' mcp"
    );
}

// covers UP-16: at the smallest dock the copy button stays in view, and the keyboard scrolls the
// steps to their end without moving focus off it.
#[gpui_kit::test]
async fn the_setup_steps_stay_reachable_in_a_short_dock(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    let server = t.server.clone();
    t.model.update(cx, |m, cx| {
        m.expose_to_ai("kind-alpha".into(), server, AiScope::default());
        cx.notify();
    });
    t.dispatch(
        cx,
        "workspace::ShowAgentActivity",
        json!({"cluster": "kind-alpha"}),
    )
    .unwrap();
    t.wait_for_element(cx, "activity-setup").await;
    t.dispatch(cx, "workspace::SetDockHeight", json!({"height": 120.0}))
        .unwrap();
    cx.run_until_parked();
    t.focus_workspace(cx);
    for _ in 0..8 {
        if t.find(cx, "activity-copy-prompt")
            .is_some_and(|b| b.focused() == Some(true))
        {
            break;
        }
        t.press(cx, "tab");
    }
    let button = t.find(cx, "activity-copy-prompt").unwrap();
    assert_eq!(button.focused(), Some(true));
    let pane = t.find(cx, "dock-pane").unwrap().bounds();
    assert!(button.bounds().bottom() <= pane.bottom(), "{pane:?}");

    let viewport = t.find(cx, "activity-setup").unwrap().bounds();
    let note = t.find(cx, "activity-setup-note").unwrap().bounds();
    assert!(note.bottom() > viewport.bottom(), "the steps overflow");
    t.press(cx, "end");
    let note = t.find(cx, "activity-setup-note").unwrap().bounds();
    assert!(note.bottom() <= viewport.bottom(), "{note:?} {viewport:?}");
    assert_eq!(
        t.find(cx, "activity-copy-prompt").unwrap().focused(),
        Some(true)
    );
}
