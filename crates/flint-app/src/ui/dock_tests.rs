//! Leak tests for the bottom dock and its tabs (docs/TESTING.md, "Memory leaks"): opening a tab
//! and closing it must free every entity it made, its view included, so a closed session ends
//! and nothing it drew keeps running. A callback that captured the view strongly (a timer, a
//! task, a frame callback) fails these tests.
//!
//! GPUI tracks entity handles only when built with `GPUI_LEAK_DETECTION=1` (as CI builds this
//! crate's tests); without it the checks pass trivially.

use flint_core::exec::fake::FakeExec;
use flint_core::terminal::TerminalSize;
use flint_core::{CoreConfig, CoreHandle, ExecState, LogSubject, LogTarget, Settings};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};
use serde_json::json;

use super::dock::{Dock, DockTab, TabKey};
use super::dock_model::TabCluster;
use super::log_view::{LogRequest, LogView};
use super::registry;
use super::terminal_model::ShellRequest;
use super::terminal_view::TerminalView;

/// A dock alone in a window, shown and drawn.
fn open_dock(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Dock>) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        registry::init(cx);
    });
    let (window, dock) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(900.), px(400.)),
                })),
                ..Default::default()
            },
            cx,
            |_, cx| cx.new(Dock::new),
        )
        .unwrap()
    });
    draw(cx, window);
    (window, dock)
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

fn shell_request() -> ShellRequest {
    ShellRequest {
        cluster: "test".into(),
        namespace: "demo".into(),
        pod: "web-1".into(),
        container: Some("app".into()),
    }
}

fn log_request() -> LogRequest {
    LogRequest {
        cluster: "test".into(),
        subject: LogSubject::Pod(LogTarget {
            namespace: "demo".into(),
            pod: "web-1".into(),
            container: Some("app".into()),
        }),
        previous: false,
    }
}

/// A core with no clusters: log sessions on it end at once.
fn core() -> (CoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().to_owned(),
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: Vec::new(),
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    })
    .unwrap();
    (core, dir)
}

/// Opens a shell tab over a fake session that prints a prompt (so the grid, its cursor and
/// the text input are drawn) and rings the bell (so a timer runs). Returns the tab's id.
fn open_shell(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    dock: &Entity<Dock>,
) -> (u64, FakeExec) {
    let (session, mut fake) = FakeExec::open(shell_request().exec_request(TerminalSize::default()));
    let id = cx
        .update_window(window, |_, window, cx| {
            dock.update(cx, |dock, cx| {
                let request = shell_request();
                let key = TabKey::Shell(super::terminal_view::ShellKey {
                    request: request.clone(),
                    ended: None,
                });
                dock.open(
                    key,
                    TabCluster::new("test", None),
                    |window, cx| {
                        DockTab::Shell(cx.new(|cx| TerminalView::new(session, request, window, cx)))
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    fake.set_state(ExecState::Running {
        command: vec!["bash".into()],
    });
    fake.print(b"root@web-1:/# \x07");
    draw(cx, window);
    fake.pump();
    draw(cx, window);
    (id, fake)
}

fn open_logs(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    dock: &Entity<Dock>,
    core: &CoreHandle,
) -> u64 {
    let core = core.clone();
    let id = cx
        .update_window(window, |_, window, cx| {
            dock.update(cx, |dock, cx| {
                dock.open(
                    TabKey::Logs(log_request()),
                    TabCluster::new("test", None),
                    |window, cx| {
                        DockTab::Logs(cx.new(|cx| LogView::new(core, log_request(), window, cx)))
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    draw(cx, window);
    id
}

/// Closes tab `id` as its close button does.
fn close(cx: &mut TestAppContext, window: AnyWindowHandle, id: u64) {
    cx.update_window(window, |_, window, cx| {
        registry::dispatch("dock::CloseTab", json!({ "tab": id }), window, cx)
    })
    .unwrap()
    .unwrap();
    cx.run_until_parked();
}

// covers UP-42: closing a shell tab frees its view and ends its session.
#[gpui_kit::test]
async fn closing_a_shell_tab_leaks_nothing(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    let (id, mut fake) = open_shell(cx, window, &dock);
    // Typing and a resize schedule work on the view too.
    cx.update_window(window, |_, window, cx| window.input("ls", cx))
        .unwrap();
    cx.simulate_window_resize(window, size(px(700.), px(300.)));
    draw(cx, window);
    close(cx, window, id);
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
    assert_eq!(dock.read_with(cx, |d, _| d.tab_count()), 0);
    assert!(fake.is_closed(), "closing the tab ends the session");
}

// covers UP-42: closing a log tab frees its view (its search field and list included).
#[gpui_kit::test]
async fn closing_a_log_tab_leaks_nothing(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, dock) = open_dock(cx);
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    let id = open_logs(cx, window, &dock, &core);
    close(cx, window, id);
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
}

// covers UP-42: a dock that showed tabs of both kinds, switched between them and closed them
// all holds on to nothing, and hides.
#[gpui_kit::test]
async fn the_dock_leaks_nothing_once_its_tabs_close(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, dock) = open_dock(cx);
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    let logs = open_logs(cx, window, &dock, &core);
    let (shell, _fake) = open_shell(cx, window, &dock);
    let titles = dock.read_with(cx, |d, cx| {
        d.tab_ids()
            .into_iter()
            .map(|id| d.tab(id).unwrap().item(cx).tab_title().to_string())
            .collect::<Vec<_>>()
    });
    assert_eq!(titles, ["web-1 · app", "Shell: web-1 · app"]);
    for action in ["dock::PrevTab", "dock::NextTab"] {
        cx.update_window(window, |_, window, cx| {
            registry::dispatch(action, json!(null), window, cx)
        })
        .unwrap()
        .unwrap();
        draw(cx, window);
    }
    cx.update_window(window, |_, window, cx| {
        registry::dispatch("dock::ActivateTab", json!({ "tab": shell }), window, cx)
    })
    .unwrap()
    .unwrap();
    draw(cx, window);
    close(cx, window, shell);
    draw(cx, window);
    close(cx, window, logs);
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
    assert!(!dock.read_with(cx, |d, _| d.is_visible()));
}

/// Tab `id`'s accessible label, as drawn.
fn tab_label(cx: &mut TestAppContext, window: AnyWindowHandle, id: u64) -> Option<String> {
    cx.update_window(window, |_, window, _| {
        window
            .try_find(("dock-tab", id as usize))
            .and_then(|tab| tab.label().map(str::to_owned))
    })
    .unwrap()
}

// covers UP-41, UP-42: a tab names the cluster its session runs on, and so does a shell's
// toolbar, so a shell on one cluster never passes for one on another.
#[gpui_kit::test]
async fn tabs_name_their_cluster(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, dock) = open_dock(cx);
    let logs = open_logs(cx, window, &dock, &core);
    let (shell, _fake) = open_shell(cx, window, &dock);
    assert_eq!(
        tab_label(cx, window, logs).as_deref(),
        Some("web-1 · app on test")
    );
    assert_eq!(
        tab_label(cx, window, shell).as_deref(),
        Some("Shell: web-1 · app on test")
    );
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find(("dock-tab-cluster", shell as usize)).is_some());
        assert!(window.try_find("terminal-cluster").is_some());
    })
    .unwrap();
}

// covers UP-41: a shell whose context is re-pointed at another API server ends, says why, and
// no longer answers for its container; a context that is gone, or unchanged, leaves it alone.
#[gpui_kit::test]
async fn a_shell_ends_when_its_context_is_re_pointed(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let (shell, mut fake) = open_shell(cx, window, &dock);
    let status = |cx: &mut TestAppContext| {
        dock.read_with(cx, |d, cx| d.tab(shell).unwrap().item(cx).tab_status())
    };
    dock.update(cx, |d, cx| d.check_servers(|_| None, cx));
    dock.update(cx, |d, cx| d.check_servers(|_| Some(None), cx));
    draw(cx, window);
    assert_eq!(status(cx), super::dock::TabStatus::Live);
    assert!(!fake.is_closed());

    dock.update(cx, |d, cx| {
        d.check_servers(|_| Some(Some("https://10.0.0.9:6443".into())), cx)
    });
    draw(cx, window);
    fake.pump();
    assert!(fake.is_closed(), "the session ends");
    assert_eq!(status(cx), super::dock::TabStatus::Ended);
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("terminal-ended").is_some());
    })
    .unwrap();
    let found = dock.read_with(cx, |d, cx| {
        d.find(
            &TabKey::Shell(super::terminal_view::ShellKey {
                request: shell_request(),
                ended: None,
            }),
            &TabCluster::new("test", None),
            cx,
        )
    });
    assert_eq!(found, None, "Shell opens a new tab");
}
