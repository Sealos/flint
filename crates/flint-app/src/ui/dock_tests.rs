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
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::InputState;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WeakEntity,
    WindowBounds, WindowOptions, px, size,
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
    shell_request_in("web-1")
}

fn shell_request_in(pod: &str) -> ShellRequest {
    shell_request_on("test", pod)
}

fn shell_request_on(context: &str, pod: &str) -> ShellRequest {
    ShellRequest {
        cluster: context.into(),
        namespace: "demo".into(),
        pod: pod.into(),
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
    open_shell_in(cx, window, dock, "web-1")
}

/// [`open_shell`] in pod `pod`.
fn open_shell_in(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    dock: &Entity<Dock>,
    pod: &str,
) -> (u64, FakeExec) {
    open_shell_on(cx, window, dock, "test", pod)
}

/// [`open_shell`] in pod `pod` of context `context`.
fn open_shell_on(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    dock: &Entity<Dock>,
    context: &str,
    pod: &str,
) -> (u64, FakeExec) {
    let (session, mut fake) =
        FakeExec::open(shell_request_on(context, pod).exec_request(TerminalSize::default()));
    let id = cx
        .update_window(window, |_, window, cx| {
            dock.update(cx, |dock, cx| {
                let request = shell_request_on(context, pod);
                let key = TabKey::Shell(request.clone());
                dock.open(
                    key,
                    TabCluster::new(context, Some("https://10.0.0.1:6443")),
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

/// Closes tab `id` as its close button does, and draws the dock without it, as the app does
/// next: what only its frame held (the tab list's menu state, say) is dropped then.
fn close(cx: &mut TestAppContext, window: AnyWindowHandle, id: u64) {
    cx.update_window(window, |_, window, cx| {
        registry::dispatch("dock::CloseTab", json!({ "tab": id }), window, cx)
    })
    .unwrap()
    .unwrap();
    draw(cx, window);
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

// covers UP-42: a log tab closed while its search field has focus frees that field too. The
// window keeps the focused text input registered (gpui-component's `WindowState`, a strong
// handle) and lets it go only once asked after focus moved on.
#[gpui_kit::test]
async fn closing_a_log_tab_while_searching_leaks_nothing(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, dock) = open_dock(cx);
    let snapshot = cx.update(|cx| cx.leak_detector_snapshot());
    let id = open_logs(cx, window, &dock, &core);
    focus_search(cx, window);
    close(cx, window, id);
    cx.update(|cx| cx.assert_no_new_leaks(&snapshot));
}

// covers UP-42: a log tab whose search had focus when another tab was shown is freed when it
// closes later.
#[gpui_kit::test]
async fn closing_a_log_tab_searched_before_another_tab_leaks_nothing(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, dock) = open_dock(cx);
    let logs = open_logs(cx, window, &dock, &core);
    let view = match dock.read_with(cx, |d, _| d.tab(logs).unwrap().clone()) {
        DockTab::Logs(view) => view.downgrade(),
        DockTab::Shell(_) | DockTab::Activity(_) => panic!("a log tab"),
    };
    let search = focus_search(cx, window);
    let (shell, _fake) = open_shell(cx, window, &dock);
    close(cx, window, logs);
    view.assert_released();
    search.assert_released();
    close(cx, window, shell);
}

/// Moves focus to the active log tab's search field, as ⌘F does, and draws it focused. Returns
/// the field.
fn focus_search(cx: &mut TestAppContext, window: AnyWindowHandle) -> WeakEntity<InputState> {
    cx.update_window(window, |_, window, cx| {
        registry::dispatch("logs::FocusSearch", json!(null), window, cx)
    })
    .unwrap()
    .unwrap();
    draw(cx, window);
    cx.update_window(window, |_, window, cx| {
        let focused = window
            .focused_input(cx)
            .expect("the search field has focus");
        focused.as_input().expect("a text field").downgrade()
    })
    .unwrap()
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
        assert!(
            window
                .try_find(("dock-tab-cluster", shell as usize))
                .is_some()
        );
        assert!(window.try_find("terminal-cluster").is_some());
    })
    .unwrap();
}

// covers UP-42: a live shell is reused after an edit that keeps its context's definition, and
// not after the context moved and came back to the same server.
#[gpui_kit::test]
async fn a_shell_is_reused_only_for_the_same_definition(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let (shell, _fake) = open_shell(cx, window, &dock);
    let key = TabKey::Shell(shell_request_on("test", "web-1"));
    let at = TabCluster::new("test", Some("https://10.0.0.1:6443"));
    let found = |cx: &mut TestAppContext, cluster: &TabCluster| {
        dock.read_with(cx, |d, cx| d.find(&key, cluster, cx))
    };
    assert_eq!(found(cx, &at), Some(shell), "a same-server edit");
    assert_eq!(found(cx, &at.clone().defined_by(7)), None, "moved and back");
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
    dock.update(cx, |d, cx| {
        d.check_servers(|_| Some(Some("https://10.0.0.1:6443/".into())), cx)
    });
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
            &TabKey::Shell(shell_request()),
            &TabCluster::new("test", None),
            cx,
        )
    });
    assert_eq!(found, None, "Shell opens a new tab");
}

/// Whether tab `id` lies wholly within the tab strip, as drawn.
fn tab_in_view(cx: &mut TestAppContext, window: AnyWindowHandle, id: u64) -> bool {
    cx.update_window(window, |_, window, _| {
        let strip = window.find("dock-tabs").bounds();
        let tab = window.find(("dock-tab", id as usize)).bounds();
        tab.origin.x >= strip.origin.x - px(0.5)
            && tab.origin.x + tab.size.width <= strip.origin.x + strip.size.width + px(0.5)
    })
    .unwrap()
}

fn activate(cx: &mut TestAppContext, window: AnyWindowHandle, id: u64) {
    cx.update_window(window, |_, window, cx| {
        registry::dispatch("dock::ActivateTab", json!({ "tab": id }), window, cx)
    })
    .unwrap()
    .unwrap();
    draw(cx, window);
    draw(cx, window);
}

// covers UP-42: the log tab shown tells its session, so the shared log memory budget trims
// the others first (#254).
#[gpui_kit::test]
async fn the_shown_log_tab_tells_its_session(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let (core, _dir) = core();
    let logs = open_logs(cx, window, &dock, &core);
    let log_view = match dock.read_with(cx, |dock, _| dock.tab(logs).cloned()) {
        Some(DockTab::Logs(view)) => view,
        _ => panic!("a log tab"),
    };
    let shown = |cx: &mut TestAppContext| log_view.read_with(cx, |v, _| v.is_shown());
    assert!(shown(cx), "opened and shown");
    // A shell tab in front: the log tab is not shown any more.
    let (shell, _fake) = open_shell_in(cx, window, &dock, "web-2");
    draw(cx, window);
    assert!(!shown(cx));
    activate(cx, window, logs);
    assert!(shown(cx));
    // Hiding the dock: nothing is shown, though the dock is not drawn any more.
    let toggle = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| {
            registry::dispatch("dock::ToggleDock", serde_json::Value::Null, window, cx)
        })
        .unwrap()
        .unwrap();
    };
    toggle(cx);
    assert!(!shown(cx));
    toggle(cx);
    assert!(shown(cx));
    // Closing the shell leaves the log tab shown.
    close(cx, window, shell);
    assert!(shown(cx));
}

// covers UP-42: with more tabs than fit, the strip scrolls so a tab just opened or shown is in
// view, and the tab list names every tab (title, cluster, status) and shows the one picked.
#[gpui_kit::test]
async fn the_active_tab_stays_in_view(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let mut tabs = Vec::new();
    let mut fakes = Vec::new();
    for n in 1..=8 {
        let (id, fake) = open_shell_in(cx, window, &dock, &format!("web-{n}"));
        draw(cx, window);
        tabs.push(id);
        fakes.push(fake);
    }
    let last = *tabs.last().unwrap();
    assert!(tab_in_view(cx, window, last), "the tab just opened");
    assert!(!tab_in_view(cx, window, tabs[0]), "more tabs than fit");
    activate(cx, window, tabs[0]);
    assert!(tab_in_view(cx, window, tabs[0]));
    assert!(!tab_in_view(cx, window, last));
    activate(cx, window, last);
    assert!(tab_in_view(cx, window, last));

    // The tab list, from its button at the strip's end.
    cx.update_window(window, |_, window, cx| window.click("dock-tab-list", cx))
        .unwrap();
    draw(cx, window);
    let entries = cx
        .update_window(window, |_, window, _| {
            let menu = window.within("popup-menu");
            (0usize..)
                .map_while(|ix| menu.try_find(ix))
                .map(|item| item.label().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(entries.len(), 8, "{entries:?}");
    assert_eq!(entries[0], "Shell: web-1 · app — test · live");
    cx.update_window(window, |_, window, cx| {
        window.within("popup-menu").click(0usize, cx)
    })
    .unwrap();
    draw(cx, window);
    draw(cx, window);
    assert_eq!(dock.read_with(cx, |d, _| d.active_id()), Some(tabs[0]));
    assert!(tab_in_view(cx, window, tabs[0]));
}

// covers UP-42: an empty dock shows no tab strip, only how to open logs or a shell.
#[gpui_kit::test]
async fn an_empty_dock_says_how_to_open_a_tab(cx: &mut TestAppContext) {
    let (window, _dock) = open_dock(cx);
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("dock-empty").is_some());
        assert!(window.try_find("dock-tabs").is_none(), "no empty tab strip");
        assert!(
            window.try_find("dock-hide").is_some(),
            "it can still be hidden"
        );
        let texts: Vec<String> = gpui_kit::base::test_support::texts(window)
            .into_iter()
            .map(|t| t.text().to_owned())
            .collect();
        assert!(
            texts.iter().any(|t| t == "No logs or shells open."),
            "{texts:?}"
        );
    })
    .unwrap();
}

/// Records the dock's restart requests.
fn record_restarts(
    cx: &mut TestAppContext,
    dock: &Entity<Dock>,
) -> (
    std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
    gpui_kit::Subscription,
) {
    let seen: std::rc::Rc<std::cell::RefCell<Vec<u64>>> = Default::default();
    let record = seen.clone();
    let subscription = cx.update(|cx| {
        cx.subscribe(dock, move |_, event: &super::dock::DockEvent, _| {
            if let super::dock::DockEvent::Restart(id) = event {
                record.borrow_mut().push(*id);
            }
        })
    });
    (seen, subscription)
}

fn dispatch(cx: &mut TestAppContext, window: AnyWindowHandle, name: &str) {
    cx.update_window(window, |_, window, cx| {
        registry::dispatch(name, json!(null), window, cx)
    })
    .unwrap()
    .unwrap();
    draw(cx, window);
}

// covers UP-42, UP-45: a failed log tab no longer answers for its request (opening the logs
// again opens a fresh tab), offers Reconnect, and Reconnect starts the stream again in a new
// tab beside it, which is shown; the failed tab keeps its lines until it is closed.
#[gpui_kit::test]
async fn a_failed_log_tab_reconnects_beside_itself(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, dock) = open_dock(cx);
    let (restarts, _subscription) = record_restarts(cx, &dock);
    let id = open_logs(cx, window, &dock, &core);
    let status = |cx: &mut TestAppContext| {
        dock.read_with(cx, |d, cx| d.tab(id).unwrap().item(cx).tab_status())
    };
    cx.wait_for(window, super::support_tests::WAIT, |_, cx| {
        dock.read(cx).tab(id).unwrap().item(cx).tab_status() == super::dock::TabStatus::Failed
    })
    .await;
    draw(cx, window);
    let found = dock.read_with(cx, |d, cx| {
        d.find(
            &TabKey::Logs(log_request()),
            &TabCluster::new("test", None),
            cx,
        )
    });
    assert_eq!(found, None, "Logs opens a fresh tab");
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("log-reconnect").is_some());
    })
    .unwrap();

    dispatch(cx, window, "dock::ReconnectLogs");
    assert_eq!(*restarts.borrow(), [id]);
    let old = match dock.read_with(cx, |d, _| d.tab(id).cloned()).unwrap() {
        DockTab::Logs(view) => view.downgrade(),
        DockTab::Shell(_) | DockTab::Activity(_) => panic!("a log tab"),
    };
    let (other, _) = open_shell_in(cx, window, &dock, "web-9");
    activate(cx, window, id);
    let core2 = core.clone();
    let new = cx
        .update_window(window, |_, window, cx| {
            dock.update(cx, |dock, cx| {
                dock.open_beside(
                    id,
                    TabCluster::new("test", None),
                    |window, cx| {
                        DockTab::Logs(cx.new(|cx| LogView::new(core2, log_request(), window, cx)))
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap()
        .unwrap();
    draw(cx, window);
    assert_eq!(
        dock.read_with(cx, |d, _| d.tab_ids()),
        [id, new, other],
        "right after the ended tab"
    );
    assert_eq!(dock.read_with(cx, |d, _| d.active_id()), Some(new));
    assert!(old.upgrade().is_some(), "the ended tab keeps its output");
    assert_eq!(status(cx), super::dock::TabStatus::Failed);
}

// covers UP-45: a deleted pod's ended log tab offers no Reconnect, by key, palette or button:
// there is nothing to reconnect to.
#[gpui_kit::test]
async fn a_deleted_pods_log_tab_does_not_reconnect(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, dock) = open_dock(cx);
    let (restarts, _subscription) = record_restarts(cx, &dock);
    let id = open_logs(cx, window, &dock, &core);
    let DockTab::Logs(view) = dock.read_with(cx, |d, _| d.tab(id).cloned()).unwrap() else {
        panic!("a log tab")
    };
    view.update(cx, |view, cx| {
        view.stop_streaming();
        let snapshot = flint_core::LogSnapshot {
            state: flint_core::LogState::Ended {
                hop: Default::default(),
                error: None,
                pod_deleted: true,
            },
            ..(**view.snapshot()).clone()
        };
        view.set_snapshot(std::sync::Arc::new(snapshot), cx);
    });
    draw(cx, window);
    assert_eq!(dock.read_with(cx, |d, cx| d.restartable(true, cx)), None);
    cx.update_window(window, |_, window, cx| {
        let _ = registry::dispatch("dock::ReconnectLogs", json!(null), window, cx);
        assert!(window.try_find("log-reconnect").is_none());
    })
    .unwrap();
    draw(cx, window);
    assert!(restarts.borrow().is_empty());
}

// covers UP-41: an exited shell offers Restart Shell, which asks for a new session in the same
// tab; a running one does not.
#[gpui_kit::test]
async fn an_exited_shell_asks_to_restart(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let (restarts, _subscription) = record_restarts(cx, &dock);
    let (id, mut fake) = open_shell(cx, window, &dock);
    let restartable = |cx: &mut TestAppContext| {
        dock.read_with(cx, |d, cx| {
            (d.restartable(false, cx), d.restartable(true, cx))
        })
    };
    assert_eq!(restartable(cx), (None, None), "it runs");
    fake.set_state(ExecState::Exited { code: Some(0) });
    draw(cx, window);
    assert_eq!(restartable(cx), (Some(id), None));
    dispatch(cx, window, "dock::RestartShell");
    assert_eq!(*restarts.borrow(), [id]);
    // Its key from the tab does the same (Cmd-R; Ctrl-Shift-R on Linux, where Ctrl-R is the
    // shell's).
    cx.update_window(window, |_, window, cx| {
        window.press(registry::RESTART_SHELL[0], cx)
    })
    .unwrap();
    draw(cx, window);
    assert_eq!(*restarts.borrow(), [id, id]);
}

const PROD: &str = "arn:aws:eks:eu-west-1:111111111111:cluster/main";
const STAGING: &str = "arn:aws:eks:eu-west-1:222222222222:cluster/main";

/// Shell tab `id`'s toolbar chip: its label, tooltip, and whether it is marked re-pointed.
fn shell_chip(dock: &Entity<Dock>, id: u64, cx: &mut TestAppContext) -> (String, String, bool) {
    dock.read_with(cx, |d, cx| match d.tab(id).unwrap() {
        DockTab::Shell(view) => {
            let (label, tooltip, repointed) = view.read(cx).chip().clone();
            (label.to_string(), tooltip.to_string(), repointed)
        }
        DockTab::Logs(_) | DockTab::Activity(_) => panic!("a shell"),
    })
}

// covers UP-41, UP-42: two contexts whose cluster names read alike (one EKS cluster name in
// two accounts) get different chips, on the tabs and in the shell's toolbar, whose tooltip
// names the full context and server; a re-pointed context's chips are marked alike.
#[gpui_kit::test]
async fn alike_clusters_get_different_chips(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let (prod, _prod_fake) = open_shell_on(cx, window, &dock, PROD, "web-1");
    let (staging, _staging_fake) = open_shell_on(cx, window, &dock, STAGING, "web-1");
    dock.update(cx, |d, cx| {
        d.set_contexts(vec![PROD.to_owned(), STAGING.to_owned()], cx)
    });
    draw(cx, window);
    let (prod_chip, tooltip, _) = shell_chip(&dock, prod, cx);
    assert_eq!(prod_chip, "main · 111111111111");
    assert_eq!(shell_chip(&dock, staging, cx).0, "main · 222222222222");
    assert_eq!(
        tooltip,
        format!("Context {PROD}\nAPI server https://10.0.0.1:6443")
    );
    let tab_chip = |cx: &mut TestAppContext, id: u64| {
        dock.read_with(cx, |d, cx| d.list_entry(id, cx).unwrap())
    };
    assert_eq!(
        tab_chip(cx, staging),
        "Shell: web-1 · app — main · 222222222222 · live"
    );

    // Re-pointing staging marks its tab and its toolbar chip alike; prod's stay as they were.
    dock.update(cx, |d, cx| {
        d.check_servers(
            |context| {
                Some(Some(
                    if context == STAGING {
                        "https://10.9.9.9"
                    } else {
                        "https://10.0.0.1:6443"
                    }
                    .to_owned(),
                ))
            },
            cx,
        )
    });
    draw(cx, window);
    assert!(shell_chip(&dock, staging, cx).2);
    assert!(dock.read_with(cx, |d, _| d.is_repointed(staging)));
    assert!(!shell_chip(&dock, prod, cx).2);
    assert!(!dock.read_with(cx, |d, _| d.is_repointed(prod)));
}

// covers UP-42: a long context and a long title stay short enough that about two and a half
// tabs fit in a dock 860 points wide (345 points on macOS; the bound leaves room for other
// fonts).
#[gpui_kit::test]
async fn a_long_context_and_title_keep_the_tab_narrow(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let context = "gke_acme-platform-production-eu_europe-west1-b_payments-core-cluster";
    let (id, _fake) = open_shell_on(
        cx,
        window,
        &dock,
        context,
        "checkout-api-7d9f8b6c5d-x2x9z-with-a-long-suffix",
    );
    draw(cx, window);
    let width = cx
        .update_window(window, |_, window, _| {
            f32::from(window.find(("dock-tab", id as usize)).bounds().size.width)
        })
        .unwrap();
    assert!(width <= 360., "a tab is {width} wide");
    assert_eq!(shell_chip(&dock, id, cx).0, "payment…-cluster");
}

// covers UP-42: the active tab stays in view when the strip narrows (the window or a pane was
// resized), and when a tab before it closes.
#[gpui_kit::test]
async fn the_active_tab_stays_in_view_through_resizes_and_closes(cx: &mut TestAppContext) {
    let (window, dock) = open_dock(cx);
    let mut tabs = Vec::new();
    let mut fakes = Vec::new();
    for n in 1..=8 {
        let (id, fake) = open_shell_in(cx, window, &dock, &format!("web-{n}"));
        tabs.push(id);
        fakes.push(fake);
    }
    draw(cx, window);
    let last = *tabs.last().unwrap();
    assert!(tab_in_view(cx, window, last));
    // Nothing left to redraw the window (the shells' bell flashes are over).
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
    // The resize draws one frame at the new size; the dock asks for the next one itself.
    cx.simulate_window_resize(window, size(px(600.), px(400.)));
    cx.run_until_parked();
    assert!(tab_in_view(cx, window, last), "after the window narrowed");

    // The second tab shown at the strip's left edge; the first one closes.
    activate(cx, window, tabs[1]);
    assert!(tab_in_view(cx, window, tabs[1]));
    close(cx, window, tabs[0]);
    draw(cx, window);
    assert!(
        tab_in_view(cx, window, tabs[1]),
        "after a tab before it closed"
    );
}

// covers UP-42: a tab whose context left kubeconfig keeps a chip unlike every context still
// there, marked as gone, while its shell runs on.
#[gpui_kit::test]
async fn a_removed_contexts_tab_keeps_its_own_chip(cx: &mut TestAppContext) {
    const GONE: &str = "gke_acme-prod_eu_core";
    const KEPT: &str = "gke_acme-dev_eu_core";
    let (window, dock) = open_dock(cx);
    dock.update(cx, |d, cx| {
        d.set_contexts(vec![GONE.into(), KEPT.into()], cx)
    });
    let (gone, gone_fake) = open_shell_on(cx, window, &dock, GONE, "web-1");
    let (kept, _kept_fake) = open_shell_on(cx, window, &dock, KEPT, "web-1");
    // The context is removed from kubeconfig; its shell keeps running.
    dock.update(cx, |d, cx| d.set_contexts(vec![KEPT.into()], cx));
    draw(cx, window);
    let (gone_label, gone_tooltip, gone_marked) = shell_chip(&dock, gone, cx);
    let (kept_label, _, kept_marked) = shell_chip(&dock, kept, cx);
    assert_ne!(gone_label, kept_label);
    assert!(gone_marked && !kept_marked);
    assert!(
        gone_tooltip.ends_with("no longer in kubeconfig."),
        "{gone_tooltip}"
    );
    assert!(
        dock.read_with(cx, |d, cx| d.list_entry(gone, cx).unwrap())
            .contains(&gone_label)
    );
    drop(gone_fake);

    // A tab opened on a context no chip covers yet is labelled with the others too.
    let (late, _late_fake) = open_shell_on(cx, window, &dock, "gke_acme-qa_eu_core", "web-1");
    draw(cx, window);
    let late_label = shell_chip(&dock, late, cx).0;
    assert_ne!(late_label, kept_label);
    assert_ne!(late_label, shell_chip(&dock, gone, cx).0);
}

// covers UP-42: chips that grow on a kubeconfig reload (a context that now reads like another)
// do not push the active tab out of view.
#[gpui_kit::test]
async fn longer_chips_keep_the_active_tab_in_view(cx: &mut TestAppContext) {
    const ONE: &str = "gke_p1_z_core";
    let (window, dock) = open_dock(cx);
    dock.update(cx, |d, cx| d.set_contexts(vec![ONE.into()], cx));
    let mut tabs = Vec::new();
    let mut fakes = Vec::new();
    for n in 1..=8 {
        let (id, fake) = open_shell_on(cx, window, &dock, ONE, &format!("web-{n}"));
        tabs.push(id);
        fakes.push(fake);
    }
    draw(cx, window);
    let last = *tabs.last().unwrap();
    assert!(tab_in_view(cx, window, last));
    dock.update(cx, |d, cx| {
        d.set_contexts(vec![ONE.into(), "gke_p2_z_core".into()], cx)
    });
    draw(cx, window);
    assert_eq!(shell_chip(&dock, last, cx).0, "core · p1");
    assert!(tab_in_view(cx, window, last));
}
