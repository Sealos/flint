//! View tests for the dock's part of the automation state (`state.dock`), its focus words, the
//! open dialog's text and a row menu's entries, over the whole window (`support_tests`). The
//! test servers never answer, so log tabs are fed their snapshots by hand and shell tabs run
//! over a [`FakeExec`].

use std::sync::Arc;

use flint_core::exec::fake::FakeExec;
use flint_core::logs::LogBuffer;
use flint_core::terminal::TerminalSize;
use flint_core::{ExecState, LogLine, LogSnapshot, LogState};
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{Value, json};

use super::row_actions_tests::{
    activate, app_state, dispatch, draw, menu, open_list, pod_detail, pods_entry, press,
    right_click, show_fetched,
};
use crate::ui::dock::{DockTab, TabKey};
use crate::ui::dock_model::TabCluster;
use crate::ui::log_view::LogView;
use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace, pods};
use crate::ui::terminal_model::ShellRequest;
use crate::ui::terminal_view::TerminalView;

/// The window's state as JSON, as the automation channel sends it.
fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> Value {
    serde_json::to_value(app_state(cx, t)).unwrap()
}

/// The log view of dock tab `id`, reading snapshots only from the test.
fn log_view(cx: &mut TestAppContext, t: &TestWorkspace, id: u64) -> Entity<LogView> {
    let view = t
        .workspace
        .read_with(cx, |ws, cx| match ws.dock.read(cx).tab(id) {
            Some(DockTab::Logs(view)) => view.clone(),
            _ => panic!("tab {id} is not a log tab"),
        });
    view.update(cx, |v, _| v.stop_streaming());
    view
}

/// Opens a shell tab on `kind-alpha` over a fake session, focused. Returns its id.
fn open_shell(cx: &mut TestAppContext, t: &TestWorkspace) -> (u64, FakeExec) {
    let request = ShellRequest {
        cluster: "kind-alpha".into(),
        namespace: "demo".into(),
        pod: "web".into(),
        container: Some("app".into()),
    };
    let (session, fake) = FakeExec::open(request.exec_request(TerminalSize::default()));
    let dock = t.workspace.read_with(cx, |ws, _| ws.dock.clone());
    let server = t.server.clone();
    let id = cx
        .update_window(t.window, |_, window, cx| {
            dock.update(cx, |dock, cx| {
                dock.open(
                    TabKey::Shell(request.clone()),
                    TabCluster::new("kind-alpha", Some(&server)),
                    move |window, cx| {
                        DockTab::Shell(cx.new(|cx| TerminalView::new(session, request, window, cx)))
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    draw(cx, t);
    (id, fake)
}

fn tab(state: &Value, id: u64) -> &Value {
    state["dock"]["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == id)
        .unwrap_or(&Value::Null)
}

// covers UP-40, UP-41, UP-42: the state reports the dock (shown, height, focus, active tab) and
// every tab with its kind, title, cluster and status; the tab's own state comes with it. Focus
// in a log tab, a shell tab, the dialog and a menu is named. Closing the tabs empties the dock.
#[gpui_kit::test]
async fn the_state_reports_the_dock_its_tabs_and_focus(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    activate(cx, &t, "kind-alpha").await;
    open_list(cx, &t, pods_entry(), pods(&[("demo", "web", 1)]), "web").await;
    let s = state(cx, &t);
    assert_eq!(
        s["dock"],
        json!({"visible": false, "height": 280.0, "focused": false, "active": null, "tabs": []})
    );
    assert_eq!(s["menu"], Value::Null);
    assert_eq!(s["list"]["rows"][0]["id"], "row-uid-web");

    // The row's menu, as drawn, with focus in it: each entry, and why it can't run under a
    // disabled one (the pod's fetch fails on this silent server, or is still loading).
    right_click(cx, &t, "web").await;
    let drawn = menu(cx, &t);
    let s = state(cx, &t);
    assert_eq!(s["focus"], "menu");
    assert_eq!(s["menu"]["owner"], "row-menu-uid-web");
    let rows: Vec<String> = s["menu"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|e| [e["label"].as_str(), e["reason"].as_str()])
        .flatten()
        .map(str::to_owned)
        .collect();
    assert_eq!(rows, drawn);
    press(cx, &t, "escape");
    draw(cx, &t);
    assert_eq!(state(cx, &t)["menu"], Value::Null);

    // The detail pane's Logs picker: its containers, then those with a previous instance; its
    // notes ("Container", "Previous instance") are not entries.
    show_fetched(cx, &t, pod_detail("web"));
    t.click(cx, "detail-logs");
    draw(cx, &t);
    let s = state(cx, &t);
    assert_eq!(s["menu"]["owner"], "detail-logs");
    let labels: Vec<&str> = s["menu"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["app (default)", "sidecar", "app (default)"]);
    press(cx, &t, "escape");
    draw(cx, &t);
    assert_eq!(state(cx, &t)["menu"], Value::Null);

    // A log tab, opened from the detail pane, takes focus.
    dispatch(cx, &t, "detail::OpenLogs", Value::Null).unwrap();
    draw(cx, &t);
    let s = state(cx, &t);
    let id = s["dock"]["active"].as_u64().expect("a tab is shown");
    assert_eq!(s["focus"], "log");
    assert_eq!(
        (&s["dock"]["visible"], &s["dock"]["focused"]),
        (&json!(true), &json!(true))
    );
    let log = tab(&s, id);
    assert_eq!(log["kind"], "pod_logs");
    assert_eq!(log["title"], "web · app");
    assert_eq!(
        (&log["active"], &log["focused"]),
        (&json!(true), &json!(true))
    );
    assert_eq!(
        log["cluster"],
        json!({"context": "kind-alpha", "server": t.server, "chip": "kind-alpha",
               "repointed": false, "removed": false})
    );
    assert_eq!(log["shell"], Value::Null);

    // The toolbar's "⋯" menu (the tab is narrow) is drawn inside the tab, yet focus is in the
    // menu. It says what it was built with: no lines yet, so no Save entries, and toggles say
    // whether they are on.
    assert!(t.find(cx, "log-more").is_some(), "a narrow toolbar");
    t.click(cx, "log-more");
    draw(cx, &t);
    let toggles = [
        json!({"label": "Timestamps", "enabled": true, "reason": null,
               "checked": false, "submenu": []}),
        json!({"label": "Wrap Lines", "enabled": true, "reason": null,
               "checked": false, "submenu": []}),
        json!({"label": "Errors and Warnings Only", "enabled": true, "reason": null,
               "checked": false, "submenu": []}),
        json!({"label": "Regex Search", "enabled": true, "reason": null,
               "checked": false, "submenu": []}),
        json!({"label": "Matching Lines Only", "enabled": true, "reason": null,
               "checked": false, "submenu": []}),
    ];
    let s = state(cx, &t);
    assert_eq!(s["focus"], "menu");
    assert_eq!(s["menu"]["owner"], "log-more");
    assert_eq!(s["menu"]["entries"], Value::from(toggles.to_vec()));

    // Lines arrive while it is open: the menu stays as built, and so does the state.
    let view = log_view(cx, &t, id);
    let mut buffer = LogBuffer::new(100);
    for text in ["ready", "WARN slow"] {
        buffer.push(LogLine::parse(
            &format!("2024-05-01T12:00:00Z {text}"),
            false,
        ));
    }
    view.update(cx, |v, cx| {
        v.set_snapshot(
            Arc::new(LogSnapshot {
                lines: buffer.view(),
                state: LogState::Streaming,
                paused: false,
                held_back: 0,
                version: 1,
                pods: None,
                trimmed: false,
            }),
            cx,
        )
    });
    draw(cx, &t);
    let s = state(cx, &t);
    assert_eq!(s["menu"]["entries"], Value::from(toggles.to_vec()));
    let log = tab(&s, id);
    assert_eq!(log["status"], json!({"state": "streaming"}));
    assert_eq!(log["status_text"], "Streaming");
    assert_eq!(log["logs"]["lines"], 2);
    assert_eq!(log["logs"]["last_lines"][1]["severity"], "warning");
    press(cx, &t, "escape");
    draw(cx, &t);
    let s = state(cx, &t);
    assert_eq!(s["menu"], Value::Null);
    assert_eq!(s["focus"], "log");

    // Opened again, it offers Save; its note is not an entry.
    t.click(cx, "log-more");
    draw(cx, &t);
    let entries = state(cx, &t)["menu"]["entries"].clone();
    let mut expected = toggles.to_vec();
    expected.extend([
        json!({"label": "Save All Lines…", "enabled": true, "reason": null, "checked": null,
               "submenu": []}),
        json!({"label": "Save Matching Lines…", "enabled": false, "reason": null,
               "checked": null, "submenu": []}),
        json!({"label": "Save Selected Lines…", "enabled": false, "reason": null,
               "checked": null, "submenu": []}),
    ]);
    assert_eq!(entries, Value::from(expected));
    press(cx, &t, "escape");
    draw(cx, &t);
    assert_eq!(state(cx, &t)["focus"], "log");

    // The save confirmation says what it asks, with focus in it.
    dispatch(cx, &t, "logs::SaveLines", json!({"lines": "all"})).unwrap();
    cx.wait_for(t.window, WAIT, |window, cx| {
        gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    let s = state(cx, &t);
    assert_eq!(s["focus"], "dialog");
    assert_eq!(s["dialog"]["title"], "Save logs to a file?");
    let text = s["dialog"]["text"].as_str().unwrap();
    assert!(
        text.starts_with("2 lines will be written as shown"),
        "{text}"
    );
    assert!(text.contains("not redacted"), "{text}");
    assert_eq!(s["dialog"]["buttons"], json!(["Choose File…", "Cancel"]));
    press(cx, &t, "escape");
    cx.wait_for(t.window, WAIT, |window, cx| {
        !gpui_kit::component::WindowExt::has_active_dialog(window, cx)
    })
    .await;
    assert_eq!(state(cx, &t)["dialog"], Value::Null);

    // A shell tab beside it.
    let (shell, mut fake) = open_shell(cx, &t);
    fake.set_state(ExecState::Running {
        command: vec!["bash".into()],
    });
    fake.print(b"$ ");
    draw(cx, &t);
    let s = state(cx, &t);
    assert_eq!(s["focus"], "shell");
    assert_eq!(s["dock"]["active"], shell);
    let sh = tab(&s, shell);
    assert_eq!(sh["kind"], "shell");
    assert_eq!(sh["title"], "Shell: web · app");
    assert_eq!(sh["status"], json!({"state": "running", "command": "bash"}));
    assert_eq!(sh["shell"]["screen"], json!(["$"]));
    assert_eq!(sh["shell"]["live"], true);
    assert_eq!(sh["logs"], Value::Null);
    assert_eq!(tab(&s, id)["active"], false);
    assert_eq!(tab(&s, id)["focused"], false);

    // The tab list is a menu of its own, listed as it was built.
    dispatch(cx, &t, "dock::ShowTabList", Value::Null).unwrap();
    draw(cx, &t);
    let s = state(cx, &t);
    assert_eq!(s["focus"], "menu");
    assert_eq!(s["menu"]["owner"], "dock-tab-list");
    let labels: Vec<&str> = s["menu"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["label"].as_str().unwrap())
        .collect();
    assert_eq!(
        labels,
        [
            "web · app — kind-alpha · Streaming",
            "Shell: web · app — kind-alpha · live"
        ]
    );
    // The shown tab is checked.
    let checked: Vec<&Value> = s["menu"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| &e["checked"])
        .collect();
    assert_eq!(checked, [&json!(false), &json!(true)]);
    press(cx, &t, "escape");
    draw(cx, &t);
    assert_eq!(state(cx, &t)["menu"], Value::Null);

    // The height follows SetDockHeight; hiding keeps the tabs and gives focus back.
    dispatch(cx, &t, "workspace::SetDockHeight", json!({"height": 333})).unwrap();
    draw(cx, &t);
    assert_eq!(state(cx, &t)["dock"]["height"], 333.0);
    dispatch(cx, &t, "dock::ToggleDock", Value::Null).unwrap();
    draw(cx, &t);
    let s = state(cx, &t);
    assert_eq!(
        (&s["dock"]["visible"], &s["dock"]["focused"]),
        (&json!(false), &json!(false))
    );
    assert_ne!(s["focus"], "shell");
    assert_eq!(s["dock"]["tabs"].as_array().unwrap().len(), 2);
    dispatch(cx, &t, "dock::ToggleDock", Value::Null).unwrap();
    draw(cx, &t);

    // Closing every tab empties and hides the dock.
    for tab in [shell, id] {
        dispatch(cx, &t, "dock::CloseTab", json!({"tab": tab})).unwrap();
        draw(cx, &t);
    }
    let s = state(cx, &t);
    assert_eq!(
        (
            &s["dock"]["visible"],
            &s["dock"]["active"],
            &s["dock"]["tabs"]
        ),
        (&json!(false), &Value::Null, &json!([]))
    );
}
