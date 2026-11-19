//! View tests for a log tab, in a headless GPUI window (`#[gpui_kit::test]`). Snapshots are fed
//! by hand; the first scan of a search runs through a real core's `search_logs`.

use std::sync::Arc;
use std::time::Duration;

use flint_core::logs::LogBuffer;
use flint_core::{
    CoreConfig, CoreHandle, LineSelection, LineSource, LogLine, LogSnapshot, LogState, LogSubject,
    LogTarget, PodSet, PodSetStatus, Settings, StreamStatus, WorkloadKind, WorkloadTarget,
};
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Modifiers, Point, ScrollDelta,
    TestAppContext, WindowBounds, WindowOptions, point, px, size,
};

use super::{LogRequest, LogView};
use crate::ui::dock::{DockItem, TabStatus};
use crate::ui::registry::{self, SaveLines, SaveScope};

const WAIT: Duration = Duration::from_secs(10);

fn core() -> (CoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.path().to_owned(),
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: Vec::new(),
        home: None,
        watch_files: false,
    })
    .unwrap();
    (core, dir)
}

fn request() -> LogRequest {
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

/// The logs of Deployment `web`, every pod and container.
fn workload_request() -> LogRequest {
    LogRequest {
        cluster: "test".into(),
        subject: LogSubject::Workload(WorkloadTarget {
            namespace: "demo".into(),
            pods: PodSet::Workload {
                kind: WorkloadKind::Deployment,
                name: "web".into(),
            },
            container: None,
        }),
        previous: false,
    }
}

/// Lines as a session's buffer holds them, and snapshots of it.
struct Feed {
    buffer: LogBuffer,
    version: u64,
}

impl Feed {
    fn new(limit: usize) -> Self {
        Feed {
            buffer: LogBuffer::new(limit),
            version: 0,
        }
    }

    /// Adds lines `from..to`, each `2024-05-01T12:00:00Z line N`, with an error every fifth.
    fn push(&mut self, from: u64, to: u64) -> &mut Self {
        for n in from..to {
            let text = if n % 5 == 4 {
                format!("2024-05-01T12:00:00Z ERROR line {n}")
            } else {
                format!("2024-05-01T12:00:00Z line {n}")
            };
            self.buffer.push(LogLine::parse(&text, false));
        }
        self
    }

    fn snapshot(&mut self, state: LogState, paused: bool, held_back: u64) -> Arc<LogSnapshot> {
        self.version += 1;
        Arc::new(LogSnapshot {
            lines: self.buffer.view(),
            state,
            paused,
            held_back,
            version: self.version,
            pods: None,
        })
    }

    fn streaming(&mut self) -> Arc<LogSnapshot> {
        self.snapshot(LogState::Streaming, false, 0)
    }
}

/// A log tab alone in a window, focused, reading snapshots only from the test.
fn open(cx: &mut TestAppContext, core: CoreHandle) -> (AnyWindowHandle, Entity<LogView>) {
    open_request(cx, core, request())
}

fn open_request(
    cx: &mut TestAppContext,
    core: CoreHandle,
    request: LogRequest,
) -> (AnyWindowHandle, Entity<LogView>) {
    open_sized(cx, core, request, 1000.)
}

/// A log tab alone in a window `width` wide.
fn open_sized(
    cx: &mut TestAppContext,
    core: CoreHandle,
    request: LogRequest,
    width: f32,
) -> (AnyWindowHandle, Entity<LogView>) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        registry::init(cx);
    });
    cx.update(|cx| {
        let (window, view) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(width), px(400.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    let mut view = LogView::new(core, request, window, cx);
                    view.stop_streaming();
                    view
                })
            },
        )
        .unwrap();
        window
            .update(cx, |_, window, cx| {
                let focus = view.read(cx).focus.clone();
                window.focus(&focus, cx);
            })
            .unwrap();
        (window, view)
    })
}

fn feed(cx: &mut TestAppContext, view: &Entity<LogView>, snapshot: Arc<LogSnapshot>) {
    view.update(cx, |view, cx| view.set_snapshot(snapshot, cx));
}

fn line_id(id: u64) -> (&'static str, usize) {
    ("log-line", id as usize)
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .unwrap();
    cx.run_until_parked();
}

// covers UP-40: the tab shows the session's state and its lines, and keeps the list in step
// with the ring buffer as lines arrive and are evicted.
#[gpui_kit::test]
async fn a_log_tab_shows_its_state_and_follows_the_buffer(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let status = |cx: &mut TestAppContext| view.read_with(cx, |v, _| v.tab_status());
    // Fed by hand: the real session (on no cluster) may already have ended.
    let mut lines = Feed::new(50);
    feed(cx, &view, lines.snapshot(LogState::Connecting, false, 0));
    assert_eq!(status(cx), TabStatus::Starting);
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-empty").is_some()
    })
    .await;

    feed(cx, &view, lines.push(0, 3).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        (0..3).all(|id| window.try_find(line_id(id)).is_some())
            && window.try_find("log-empty").is_none()
    })
    .await;
    assert_eq!(status(cx), TabStatus::Live);
    assert_eq!(
        view.read_with(cx, |v, _| v.tab_title()),
        "web-1 · app".to_owned()
    );

    // Following a chatty pod: 80 more lines into a 50-line buffer evict the oldest; the list
    // keeps one item per line, and the newest line is on screen.
    feed(cx, &view, lines.push(3, 83).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(82)).is_some()
    })
    .await;
    view.read_with(cx, |v, _| {
        assert_eq!(v.list.item_count(), v.snapshot.lines.len());
        assert_eq!(v.snapshot.lines.first_id(), 33);
        assert!(v.list.is_following_tail());
    });

    // Reconnecting and ended states, with the error shown over the kept lines.
    feed(
        cx,
        &view,
        lines.snapshot(
            LogState::Reconnecting {
                error: Some("unexpected EOF".into()),
                attempt: 2,
            },
            false,
            0,
        ),
    );
    assert_eq!(status(cx), TabStatus::Retrying);
    feed(
        cx,
        &view,
        lines.snapshot(
            LogState::Ended {
                error: Some("container not found".into()),
            },
            false,
            0,
        ),
    );
    assert_eq!(status(cx), TabStatus::Failed);
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-ended").is_some()
    })
    .await;
    assert_eq!(view.read_with(cx, |v, _| v.list.item_count()), 50);
}

// covers UP-40: Space pauses and resumes following, the toolbar counts the lines held back,
// and scrolling up stops following until Jump to Newest Line.
#[gpui_kit::test]
async fn pause_holds_lines_back_and_scrolling_up_stops_following(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut lines = Feed::new(1000);
    feed(cx, &view, lines.push(0, 200).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(199)).is_some()
    })
    .await;

    press(cx, window, "space");
    view.read_with(cx, |v, _| {
        assert!(v.is_paused() && v.session.is_paused());
    });
    // The core keeps the lines shown and counts the ones that arrive meanwhile.
    feed(cx, &view, lines.snapshot(LogState::Streaming, true, 42));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-paused").is_some()
    })
    .await;
    press(cx, window, "space");
    view.read_with(cx, |v, _| assert!(!v.is_paused() && !v.session.is_paused()));
    feed(cx, &view, lines.push(200, 242).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-paused").is_none() && window.try_find(line_id(241)).is_some()
    })
    .await;

    // Scrolling up stops following: new lines arrive below, out of view.
    cx.update_window(window, |_, window, cx| {
        window.scroll(
            line_id(241),
            ScrollDelta::Pixels(point(px(0.), px(2000.))),
            cx,
        )
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!view.read_with(cx, |v, _| v.list.is_following_tail()));
    feed(cx, &view, lines.push(242, 250).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-jump").is_some()
    })
    .await;
    cx.update_window(window, |_, window, cx| {
        assert!(window.try_find(line_id(249)).is_none());
        window.click("log-jump", cx);
    })
    .unwrap();
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(249)).is_some() && window.try_find("log-jump").is_none()
    })
    .await;
    assert!(view.read_with(cx, |v, _| v.list.is_following_tail()));

    // Home goes to the oldest line and stops following; End follows again and resumes a pause.
    press(cx, window, "home");
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(0)).is_some()
    })
    .await;
    assert!(!view.read_with(cx, |v, _| v.list.is_following_tail()));
    press(cx, window, "space");
    press(cx, window, "end");
    view.read_with(cx, |v, _| {
        assert!(v.list.is_following_tail());
        assert!(!v.is_paused() && !v.session.is_paused());
    });
}

// covers UP-40, UP-43: the search highlights matches, counts them, keeps matching new lines,
// and Cmd-G / Shift-Cmd-G / Enter move between them.
#[gpui_kit::test]
async fn search_highlights_counts_and_moves_between_matches(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut lines = Feed::new(1000);
    feed(cx, &view, lines.push(0, 20).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(19)).is_some()
    })
    .await;

    press(cx, window, "cmd-f");
    cx.update_window(window, |_, window, cx| window.input("error", cx))
        .unwrap();
    cx.wait_for(window, WAIT, |window, cx| {
        let v = view.read(cx);
        !v.searching && v.search.count() == 4 && window.try_find("log-matches").is_some()
    })
    .await;
    // Error lines are 4, 9, 14 and 19; their text is highlighted where it matches.
    view.read_with(cx, |v, _| {
        assert_eq!(
            v.search.matches().iter().copied().collect::<Vec<_>>(),
            [4, 9, 14, 19]
        );
        let line = v.snapshot.lines.get(9).unwrap();
        let shown = crate::ui::log_model::shown_line(line, v.format(), &v.query, None);
        assert_eq!(&shown.text[shown.matches[0].clone()], "ERROR");
        assert_eq!(line.severity, flint_core::Severity::Error);
    });

    // Enter in the field goes to the first match; Cmd-G and Shift-Cmd-G move from there.
    press(cx, window, "enter");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(4));
    let focus = view.read_with(cx, |v, _| v.focus.clone());
    cx.update_window(window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    press(cx, window, "cmd-g");
    press(cx, window, "cmd-g");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(14));
    press(cx, window, "cmd-shift-g");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(9));
    assert_eq!(
        view.read_with(cx, |v, _| crate::ui::log_model::match_text(
            &v.search,
            v.current_match
        )),
        "2 of 4"
    );

    // Following keeps matching new lines, without a new scan.
    feed(cx, &view, lines.push(20, 30).streaming());
    assert_eq!(view.read_with(cx, |v, _| v.search.count()), 6);
    // Wrapping around: next from the last match is the first.
    view.update(cx, |v, _| v.current_match = Some(29));
    press(cx, window, "cmd-g");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(4));
}

// covers UP-44: a click selects a line, Shift-click and Shift-arrows extend it, following pauses
// while selecting, Cmd-C copies the lines as shown, and Esc clears and resumes.
#[gpui_kit::test]
async fn selecting_pauses_and_copies_the_lines_as_shown(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut lines = Feed::new(1000);
    feed(cx, &view, lines.push(0, 10).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(9)).is_some()
    })
    .await;

    cx.update_window(window, |_, window, cx| window.click(line_id(2), cx))
        .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |v, _| {
        assert_eq!(v.selection, Some(LineSelection::line(2)));
        assert!(v.session.is_paused(), "selecting pauses following");
    });
    cx.update_window(window, |_, window, cx| {
        window.click_with_modifiers(line_id(4), Modifiers::shift(), cx)
    })
    .unwrap();
    cx.run_until_parked();
    press(cx, window, "shift-down");
    assert_eq!(
        view.read_with(cx, |v, _| v.selection),
        Some(LineSelection::new(2, 5))
    );
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-selection").is_some()
    })
    .await;

    press(cx, window, "cmd-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(copied, "line 2\nline 3\nERROR line 4\nline 5");
    // With timestamps shown, they are copied too.
    press(cx, window, "t");
    press(cx, window, "cmd-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert!(
        copied.starts_with("2024-05-01T12:00:00Z line 2\n"),
        "{copied}"
    );

    // Lines arriving meanwhile do not move the selection.
    feed(
        cx,
        &view,
        lines.push(10, 12).snapshot(LogState::Streaming, true, 2),
    );
    assert_eq!(
        view.read_with(cx, |v, _| v.selection),
        Some(LineSelection::new(2, 5))
    );
    // End follows again with the lines still selected; selecting more pauses again.
    press(cx, window, "end");
    view.read_with(cx, |v, _| {
        assert!(v.selection.is_some() && !v.session.is_paused());
    });
    press(cx, window, "shift-up");
    assert!(
        view.read_with(cx, |v, _| v.session.is_paused()),
        "selecting after a resume pauses again"
    );
    press(cx, window, "escape");
    view.read_with(cx, |v, _| {
        assert_eq!(v.selection, None);
        assert!(!v.session.is_paused(), "clearing the selection resumes");
    });

    // Up and Down with nothing selected start at the newest line; Cmd-A selects all.
    press(cx, window, "up");
    assert_eq!(
        view.read_with(cx, |v, _| v.selection),
        Some(LineSelection::line(11))
    );
    press(cx, window, "up");
    assert_eq!(
        view.read_with(cx, |v, _| v.selection),
        Some(LineSelection::line(10))
    );
    press(cx, window, "cmd-a");
    assert_eq!(
        view.read_with(cx, |v, _| v.selection),
        Some(LineSelection::new(0, 11))
    );
}

// covers UP-43: timestamps and wrapping are view toggles; the session is not reopened.
#[gpui_kit::test]
async fn timestamps_and_wrap_toggle_without_reloading(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut lines = Feed::new(100);
    feed(cx, &view, lines.push(0, 5).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(4)).is_some()
    })
    .await;
    let shown = |cx: &mut TestAppContext| {
        view.read_with(cx, |v, _| {
            let line = v.snapshot.lines.get(4).unwrap();
            crate::ui::log_model::shown_line(line, v.format(), &v.query, None).text
        })
    };
    assert_eq!(shown(cx), "ERROR line 4");
    cx.update_window(window, |_, window, cx| window.click("log-timestamps", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(shown(cx), "2024-05-01T12:00:00Z ERROR line 4");
    press(cx, window, "w");
    view.read_with(cx, |v, _| {
        assert!(v.wrap && v.timestamps);
        // The same session, with the same lines.
        assert_eq!(v.request(), &request());
        assert_eq!(v.snapshot().lines.len(), 5);
        assert_eq!(v.snapshot().version, 1);
    });
    press(cx, window, "t");
    assert_eq!(shown(cx), "ERROR line 4");
}

// covers UP-46 (#75): save writes the chosen lines as shown, not redacted, where the user
// picked in the save dialog.
#[gpui_kit::test]
async fn save_writes_matches_as_shown_to_the_chosen_file(cx: &mut TestAppContext) {
    let (core, dir) = core();
    let (window, view) = open(cx, core);
    let mut lines = Feed::new(100);
    feed(cx, &view, lines.push(0, 10).streaming());
    view.update(cx, |v, cx| v.set_query("error".into(), cx));
    cx.wait_for(window, WAIT, |_, cx| {
        let v = view.read(cx);
        !v.searching && v.search.count() == 2
    })
    .await;

    // With nothing selected, saving the selection asks nothing.
    view.read_with(cx, |v, _| {
        assert!(v.check_save(SaveScope::Selection).is_err());
        assert!(v.check_save(SaveScope::Matches).is_ok());
    });
    cx.update_window(window, |_, window, cx| {
        registry::dispatch(
            "logs::SaveLines",
            serde_json::json!({"lines": "selection"}),
            window,
            cx,
        )
    })
    .unwrap()
    .unwrap();
    cx.run_until_parked();
    assert!(!cx.did_prompt_for_new_path());

    cx.update_window(window, |_, window, cx| {
        window.dispatch_action(
            Box::new(SaveLines {
                lines: SaveScope::Matches,
                timestamps: Some(true),
            }),
            cx,
        )
    })
    .unwrap();
    cx.run_until_parked();
    // First a confirmation that says the file is not redacted; the save dialog only after it.
    assert!(!cx.did_prompt_for_new_path());
    cx.wait_for(window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
    cx.update_window(window, |_, window, cx| {
        registry::dispatch("ui::Confirm", serde_json::Value::Null, window, cx)
    })
    .unwrap()
    .unwrap();
    cx.run_until_parked();
    assert!(cx.did_prompt_for_new_path());
    let path = dir.path().join("matches.log");
    let chosen = path.clone();
    cx.simulate_new_path_selection(move |_| Some(chosen));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-note").is_some()
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "2024-05-01T12:00:00Z ERROR line 4\n2024-05-01T12:00:00Z ERROR line 9\n"
    );
    let note = view.read_with(cx, |v, _| v.note.clone().unwrap().text);
    assert!(
        note.contains("not redacted") && note.contains("2 lines"),
        "{note}"
    );
}

// covers UP-45: a workload tab prefixes each line with its pod and container (a view toggle,
// copied as shown) and counts the workload's streams in the toolbar.
#[gpui_kit::test]
async fn a_workload_tab_shows_sources_and_stream_counts(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open_request(cx, core, workload_request());
    assert_eq!(
        view.read_with(cx, |v, _| v.tab_title()),
        "deployment/web".to_owned()
    );
    let mut buffer = LogBuffer::new(100);
    for (pod, text) in [
        ("web-1", "GET /a"),
        ("web-2", "ERROR boom"),
        ("web-1", "GET /b"),
    ] {
        let mut line = LogLine::parse(&format!("2024-05-01T12:00:00Z {text}"), false);
        line.source = Some(Arc::new(LineSource::new(pod, "app")));
        buffer.push(line);
    }
    let stream = |pod: &str, state| StreamStatus {
        source: Arc::new(LineSource::new(pod, "app")),
        state,
    };
    let pods = PodSetStatus {
        selector: Some("app=web".into()),
        pods: 2,
        streams: vec![
            stream("web-1", LogState::Streaming),
            stream("web-2", LogState::Streaming),
        ],
        max_streams: 50,
        ..Default::default()
    };
    feed(
        cx,
        &view,
        Arc::new(LogSnapshot {
            lines: buffer.view(),
            state: LogState::Streaming,
            paused: false,
            held_back: 0,
            version: 1,
            pods: Some(Arc::new(pods)),
        }),
    );
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-streams").is_some()
            && window.try_find("log-sources").is_some()
            && window.try_find(line_id(2)).is_some()
    })
    .await;

    press(cx, window, "cmd-a");
    press(cx, window, "cmd-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(
        copied,
        "[web-1/app] GET /a\n[web-2/app] ERROR boom\n[web-1/app] GET /b"
    );
    // P hides the prefix without reloading; copy follows what is shown.
    press(cx, window, "p");
    press(cx, window, "cmd-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(copied, "GET /a\nERROR boom\nGET /b");

    // With the prefix shown, the search finds a pod's lines by its name.
    press(cx, window, "p");
    view.update(cx, |v, cx| v.set_query("web-2".into(), cx));
    cx.wait_for(window, WAIT, |_, cx| {
        let v = view.read(cx);
        !v.searching && v.search.count() == 1
    })
    .await;
}

// covers UP-40: moving to a match far below the lines on screen shows it, though the rows in
// between were never drawn (so the list has not measured them).
#[gpui_kit::test]
async fn a_far_match_is_scrolled_into_view(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut buffer = LogBuffer::new(5000);
    for n in 0..3000 {
        let text = if n == 2500 {
            "needle here".to_owned()
        } else {
            format!("line {n}")
        };
        buffer.push(LogLine::parse(&text, false));
    }
    feed(
        cx,
        &view,
        Arc::new(LogSnapshot {
            lines: buffer.view(),
            state: LogState::Streaming,
            paused: false,
            held_back: 0,
            version: 1,
            pods: None,
        }),
    );
    press(cx, window, "home");
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(0)).is_some()
    })
    .await;
    view.update(cx, |v, cx| v.set_query("needle".into(), cx));
    cx.wait_for(window, WAIT, |_, cx| {
        let v = view.read(cx);
        !v.searching && v.search.count() == 1
    })
    .await;
    press(cx, window, "cmd-g");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(2500));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(2500)).is_some_and(|line| {
            let y = f32::from(line.bounds().origin.y);
            (0. ..400.).contains(&y)
        })
    })
    .await;
}

// covers UP-46: in a narrow dock, with a search and a pause showing, the toolbar wraps instead
// of clipping, so Save stays on screen; Save Matching Lines and Save Selected Lines also have
// keys and palette entries.
#[gpui_kit::test]
async fn a_narrow_toolbar_keeps_save_reachable(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open_sized(cx, core, request(), 420.);
    let mut lines = Feed::new(100);
    feed(
        cx,
        &view,
        lines.push(0, 10).snapshot(LogState::Streaming, true, 7),
    );
    view.update(cx, |v, cx| {
        v.user_paused = true;
        v.set_query("error".into(), cx);
    });
    cx.wait_for(window, WAIT, |window, cx| {
        !view.read(cx).searching
            && window.try_find("log-matches").is_some()
            && window.try_find("log-save").is_some_and(|save| {
                let b = save.bounds();
                f32::from(b.origin.x + b.size.width) <= 420. && f32::from(b.size.width) > 0.
            })
    })
    .await;
    for name in ["logs::SaveMatchingLines", "logs::SaveSelectedLines"] {
        let spec = registry::find(name).unwrap();
        assert!(!spec.keys.is_empty(), "{name} has a key");
    }
    press(cx, window, "cmd-shift-s");
    cx.wait_for(window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
}
