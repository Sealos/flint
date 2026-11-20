//! View tests for a log tab, in a headless GPUI window (`#[gpui_kit::test]`). Snapshots are fed
//! by hand; the first scan of a search runs through a real core's `search_logs`.

use std::cell::Cell;
use std::rc::Rc;
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
    AnyWindowHandle, AppContext as _, Bounds, Context, Entity, InteractiveElement as _,
    IntoElement, Modifiers, ParentElement as _, Point, Render, ScrollDelta, Styled as _,
    TestAppContext, Window, WindowBounds, WindowOptions, div, point, px, size,
};

use super::{LogRequest, LogView};
use crate::ui::dock::{DockItem, TabStatus};
use crate::ui::registry::{self, Close, SaveLines, SaveScope};

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
        watch_timing: Default::default(),
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
                    // Timestamps in UTC, whatever the machine's time zone.
                    view.tz = jiff::tz::TimeZone::UTC;
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
                back_off: false,
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
                pod_deleted: false,
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

    // A pod deleted after its log was read is an ordinary end: no failure, no error notice.
    feed(
        cx,
        &view,
        lines.snapshot(
            LogState::Ended {
                error: None,
                pod_deleted: true,
            },
            false,
            0,
        ),
    );
    assert_eq!(status(cx), TabStatus::Ended);
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-ended").is_none()
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

    press(cx, window, "secondary-f");
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
        let shown = crate::ui::log_model::shown_line(line, v.line_view(), &v.query, None);
        assert_eq!(&shown.text[shown.matches[0].clone()], "ERROR");
        assert_eq!(line.severity, flint_core::Severity::Error);
    });

    // Enter in the field goes to the first match; Cmd-G and Shift-Cmd-G move from there.
    press(cx, window, "enter");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(4));
    let focus = view.read_with(cx, |v, _| v.focus.clone());
    cx.update_window(window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    press(cx, window, "secondary-g");
    press(cx, window, "secondary-g");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(14));
    press(cx, window, "secondary-shift-g");
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
    press(cx, window, "secondary-g");
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

    press(cx, window, "secondary-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(copied, "line 2\nline 3\nERROR line 4\nline 5");
    // With timestamps shown, they are copied too.
    press(cx, window, "t");
    press(cx, window, "secondary-c");
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
    press(cx, window, "secondary-a");
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
            crate::ui::log_model::shown_line(line, v.line_view(), &v.query, None).text
        })
    };
    assert_eq!(shown(cx), "ERROR line 4");
    cx.update_window(window, |_, window, cx| window.click("log-timestamps", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(shown(cx), "12:00:00.000 ERROR line 4");
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
        "web (Deployment)".to_owned()
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

    press(cx, window, "secondary-a");
    press(cx, window, "secondary-c");
    let copied = cx.read_from_clipboard().and_then(|c| c.text()).unwrap();
    assert_eq!(
        copied,
        "[web-1/app] GET /a\n[web-2/app] ERROR boom\n[web-1/app] GET /b"
    );
    // P hides the prefix without reloading; copy follows what is shown.
    press(cx, window, "p");
    press(cx, window, "secondary-c");
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
    press(cx, window, "secondary-g");
    assert_eq!(view.read_with(cx, |v, _| v.current_match), Some(2500));
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(2500)).is_some_and(|line| {
            let y = f32::from(line.bounds().origin.y);
            (0. ..400.).contains(&y)
        })
    })
    .await;
}

// covers UP-46: in a narrow dock, with a search and a pause showing, Save moves into the "⋯"
// menu, which stays on screen; Save Matching Lines and Save Selected Lines also have keys and
// palette entries.
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
            && window.try_find("log-next-match").is_some()
            && window.try_find("log-more").is_some_and(|save| {
                let b = save.bounds();
                f32::from(b.origin.x + b.size.width) <= 420. && f32::from(b.size.width) > 0.
            })
    })
    .await;
    for name in ["logs::SaveMatchingLines", "logs::SaveSelectedLines"] {
        let spec = registry::find(name).unwrap();
        assert!(!spec.keys.is_empty(), "{name} has a key");
    }
    press(cx, window, "secondary-shift-s");
    cx.wait_for(window, WAIT, |window, cx| window.has_active_dialog(cx))
        .await;
}

/// A log tab inside a host that, like the workspace, closes the detail pane on Esc, so a test
/// sees whether Esc in the tab falls through to it.
struct Host {
    log: Entity<LogView>,
    closed: Rc<Cell<u32>>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let closed = self.closed.clone();
        div()
            .key_context(registry::WORKSPACE)
            .on_action(move |_: &Close, _, _| closed.set(closed.get() + 1))
            .size_full()
            .child(self.log.clone())
    }
}

// covers UP-40, UP-43: in the search field, the first Esc clears the search and the second
// returns focus to the lines; in the lines, Esc clears the selection and otherwise does
// nothing, never closing the detail pane behind the tab.
#[gpui_kit::test]
async fn esc_clears_the_search_then_leaves_it_and_never_closes_the_detail(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        registry::init(cx);
    });
    let closed = Rc::new(Cell::new(0));
    let (window, view) = cx.update(|cx| {
        let closed = closed.clone();
        let mut log = None;
        let window = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1000.), px(400.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let view = cx.new(|cx| {
                    let mut view = LogView::new(core, request(), window, cx);
                    view.stop_streaming();
                    view
                });
                log = Some(view.clone());
                cx.new(|_| Host { log: view, closed })
            },
        )
        .unwrap()
        .0;
        (window, log.unwrap())
    });
    let mut lines = Feed::new(100);
    feed(cx, &view, lines.push(0, 10).streaming());
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(9)).is_some()
    })
    .await;
    let focus = view.read_with(cx, |v, _| v.focus.clone());
    cx.update_window(window, |_, window, cx| window.focus(&focus, cx))
        .unwrap();
    let log_focused = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, _| focus.is_focused(window))
            .unwrap()
    };
    cx.update_window(window, |_, window, cx| window.click(line_id(2), cx))
        .unwrap();
    cx.run_until_parked();
    assert!(view.read_with(cx, |v, _| v.selection.is_some()));
    press(cx, window, "escape");
    assert!(view.read_with(cx, |v, _| v.selection.is_none()));
    press(cx, window, "escape");
    assert_eq!(
        closed.get(),
        0,
        "Esc in the lines never closes the detail pane"
    );
    assert!(log_focused(cx));

    // In the search field: the first Esc clears the search, the next goes back to the lines.
    press(cx, window, "secondary-f");
    cx.update_window(window, |_, window, cx| window.input("line", cx))
        .unwrap();
    cx.wait_for(window, WAIT, |_, cx| {
        let v = view.read(cx);
        !v.searching && v.search.count() == 10
    })
    .await;
    press(cx, window, "escape");
    view.read_with(cx, |v, cx| {
        assert!(v.query.text.is_empty(), "the first Esc clears the search");
        assert!(v.search_input.read(cx).value().is_empty());
    });
    assert!(!log_focused(cx), "the field keeps focus");
    press(cx, window, "escape");
    assert!(log_focused(cx), "the second Esc returns to the lines");
    assert_eq!(closed.get(), 0);
}

// covers UP-40, UP-44: at 1280 and 760 points wide (and the dock widths of those windows),
// with a workload's counts, a search, a selection pause and a scroll away from the newest
// line, the toolbar stays one row with every control on screen, and only one notice strip
// shows: a copy replaces the selection hint, and times out.
#[gpui_kit::test]
async fn the_toolbar_is_one_row_and_notices_replace_each_other(cx: &mut TestAppContext) {
    for width in [1280., 860., 760., 470.] {
        let (core, _dir) = core();
        let (window, view) = open_sized(cx, core, workload_request(), width);
        let mut buffer = LogBuffer::new(1000);
        for n in 0..200 {
            let pod = ["api-79db669d5-cd27k", "api-79db669d5-6zn94"][n % 2];
            let mut line = LogLine::parse(&format!("2024-05-01T12:00:00Z GET /{n}"), false);
            line.source = Some(Arc::new(LineSource::new(pod, "api")));
            buffer.push(line);
        }
        let stream = |pod: &str| StreamStatus {
            source: Arc::new(LineSource::new(pod, "api")),
            state: LogState::Streaming,
        };
        let pods = PodSetStatus {
            selector: Some("app=api".into()),
            pods: 2,
            streams: vec![stream("api-79db669d5-cd27k"), stream("api-79db669d5-6zn94")],
            max_streams: 50,
            ..Default::default()
        };
        feed(
            cx,
            &view,
            Arc::new(LogSnapshot {
                lines: buffer.view(),
                state: LogState::Streaming,
                paused: true,
                held_back: 53,
                version: 1,
                pods: Some(Arc::new(pods)),
            }),
        );
        view.update(cx, |v, cx| v.set_query("GET".into(), cx));
        cx.wait_for(window, WAIT, |window, cx| {
            !view.read(cx).searching && window.try_find(line_id(199)).is_some()
        })
        .await;
        cx.update_window(window, |_, window, cx| window.click(line_id(199), cx))
            .unwrap();
        cx.run_until_parked();
        press(cx, window, "home");
        press(cx, window, "secondary-c");
        let controls = if width >= crate::ui::log_model::FIT_FULL {
            "log-save"
        } else {
            "log-more"
        };
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("log-not-following").is_some()
                && window.try_find("log-paused").is_some()
                && window.try_find(controls).is_some()
        })
        .await;
        cx.update_window(window, |_, window, _| {
            let toolbar = window.find("log-toolbar").bounds();
            assert_eq!(
                f32::from(toolbar.size.height),
                crate::ui::workspace::HEADER_HEIGHT,
                "one row at {width}"
            );
            let right = window.find("log-controls").bounds();
            assert!(
                f32::from(right.origin.x + right.size.width) <= width,
                "the controls fit at {width}: {right:?}"
            );
            let control = window.find(controls).bounds();
            assert!(f32::from(control.origin.x + control.size.width) <= width);
            assert!(
                f32::from(control.origin.y) < f32::from(toolbar.origin.y + toolbar.size.height)
            );
            let state = window.find("log-state").bounds();
            assert!(
                f32::from(state.size.width) >= 40.,
                "the status keeps a slot at {width}"
            );
            // One strip: the copy's note, not also the selection hint.
            assert!(window.try_find("log-note").is_some(), "{width}");
            assert!(window.try_find("log-selection").is_none(), "{width}");
        })
        .unwrap();
        // The note times out; the selection hint comes back in its place.
        cx.executor().advance_clock(super::NOTE_TIMEOUT);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("log-note").is_none() && window.try_find("log-selection").is_some()
        })
        .await;
        cx.update_window(window, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
}

// covers UP-40: once the stream has ended, Pause and "Paused" go and every line held back
// shows; following state stays said beside Jump.
#[gpui_kit::test]
async fn an_ended_stream_has_no_pause(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut lines = Feed::new(100);
    feed(
        cx,
        &view,
        lines.push(0, 10).snapshot(LogState::Streaming, true, 4),
    );
    press(cx, window, "space");
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-paused").is_some() && window.try_find("log-pause").is_some()
    })
    .await;
    feed(
        cx,
        &view,
        lines.push(10, 14).snapshot(
            LogState::Ended {
                error: None,
                pod_deleted: false,
            },
            false,
            0,
        ),
    );
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-paused").is_none() && window.try_find("log-pause").is_none()
    })
    .await;
    view.read_with(cx, |v, _| {
        assert!(
            !v.is_paused() && !v.session.is_paused(),
            "nothing held back"
        );
        assert!(!v.can_pause());
    });
}

// covers UP-40: a 403 says so in plain words once, with the server's text under it, and the
// status stays short; a previous instance whose log the kubelet could not read yet says that,
// with Try Again, instead of showing the kubelet's answer as a line.
#[gpui_kit::test]
async fn errors_are_said_in_plain_words_with_a_retry_for_previous_logs(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open_sized(cx, core.clone(), request(), 470.);
    let mut lines = Feed::new(100);
    let forbidden = "Forbidden: pods is forbidden: User \"flint-e2e\" cannot get resource \"pods\" \
                     in API group \"\" in the namespace \"demo\"";
    feed(
        cx,
        &view,
        lines.push(0, 5).snapshot(
            LogState::Ended {
                error: Some(forbidden.into()),
                pod_deleted: false,
            },
            false,
            0,
        ),
    );
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-ended").is_some()
    })
    .await;
    cx.update_window(window, |_, window, _| {
        // The server's text wraps in the banner rather than being cut.
        let detail = window.find("log-ended-detail").bounds();
        assert!(f32::from(detail.size.width) <= 470.);
        assert!(f32::from(detail.size.height) > 18., "wrapped: {detail:?}");
    })
    .unwrap();
    let status = view.read_with(cx, |v, _| {
        crate::ui::log_model::state_text(&v.snapshot.state, true).text
    });
    assert_eq!(status, "Ended · no permission");

    let previous = LogRequest {
        previous: true,
        ..request()
    };
    let (window, view) = open_sized(cx, core, previous, 760.);
    let mut empty = Feed::new(100);
    feed(
        cx,
        &view,
        empty.snapshot(
            LogState::Ended {
                error: Some(format!(
                    "{}: unable to retrieve container logs for containerd://c54f",
                    flint_core::logs::PREVIOUS_NOT_AVAILABLE
                )),
                pod_deleted: false,
            },
            false,
            0,
        ),
    );
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-retry").is_some() && window.try_find("log-empty-detail").is_some()
    })
    .await;
    assert!(view.read_with(cx, |v, _| v.has_ended()));
    cx.update_window(window, |_, window, cx| window.click("log-retry", cx))
        .unwrap();
    cx.run_until_parked();
    view.read_with(cx, |v, _| {
        assert_eq!(
            v.snapshot.state.previous_not_available(),
            None,
            "Try Again opened the log again"
        );
    });
}

// covers UP-43: shown timestamps are one width, so the text lines up; the current match is
// drawn unlike the other matches.
#[gpui_kit::test]
async fn timestamps_line_up_and_the_current_match_stands_out(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut buffer = LogBuffer::new(100);
    for text in [
        "2024-05-01T12:00:00Z a",
        "2024-05-01T12:00:00.5Z b",
        "2024-05-01T12:00:00.123456789Z c",
        "no timestamp d",
    ] {
        buffer.push(LogLine::parse(text, false));
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
    press(cx, window, "t");
    cx.wait_for(window, WAIT, |window, _| {
        (0..4usize).all(|id| window.try_find(("log-time", id)).is_some())
    })
    .await;
    cx.update_window(window, |_, window, _| {
        let widths: Vec<f32> = (0..4usize)
            .map(|id| f32::from(window.find(("log-time", id)).bounds().size.width))
            .collect();
        assert!(
            widths.iter().all(|w| (w - widths[0]).abs() < 0.5),
            "{widths:?}"
        );
    })
    .unwrap();
    cx.update(|cx| {
        let theme = gpui_kit::component::ActiveTheme::theme(cx).clone();
        let color = theme.info;
        let current = LogView::highlight(crate::ui::log_model::Part::Match, true, color, &theme);
        let other = LogView::highlight(crate::ui::log_model::Part::Match, false, color, &theme);
        assert_ne!(current.background_color, other.background_color);
        assert!(current.font_weight.is_some() && current.underline.is_some());
    });
}

/// The entries of the open popup menu.
fn menu_entries(cx: &mut TestAppContext, window: AnyWindowHandle) -> Vec<String> {
    cx.run_until_parked();
    cx.update_window(window, |_, window, _| {
        let menu = window.within("popup-menu");
        (0usize..)
            .map_while(|ix| menu.try_find(ix))
            .map(|item| item.label().unwrap_or_default().to_owned())
            .collect()
    })
    .unwrap()
}

// covers UP-40: a long status (a crash loop's back-off, a reconnect's error) is cut inside its
// slot, with an ellipsis, rather than clipped by the slot's edge.
#[gpui_kit::test]
async fn a_long_status_is_cut_inside_its_slot(cx: &mut TestAppContext) {
    for state in [
        LogState::Reconnecting {
            error: None,
            attempt: 4,
            back_off: true,
        },
        LogState::Reconnecting {
            error: Some("the log stream broke: an unexpected and long reason".into()),
            attempt: 12,
            back_off: false,
        },
    ] {
        let (core, _dir) = core();
        let (window, view) = open_sized(cx, core, request(), 470.);
        let mut lines = Feed::new(100);
        feed(cx, &view, lines.push(0, 10).snapshot(state, true, 53));
        view.update(cx, |v, _| v.user_paused = true);
        cx.wait_for(window, WAIT, |window, _| {
            window.try_find("log-paused").is_some() && window.try_find("log-state-text").is_some()
        })
        .await;
        cx.update_window(window, |_, window, _| {
            let slot = window.find("log-state").bounds();
            let text = window.find("log-state-text").bounds();
            assert!(
                text.origin.x + text.size.width <= slot.origin.x + slot.size.width + px(0.5),
                "the status text fits its slot: {text:?} in {slot:?}"
            );
            let paused = window.find("log-paused").bounds();
            assert!(f32::from(paused.size.width) > 0.);
        })
        .unwrap();
        cx.update_window(window, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
}

// covers UP-46: in a very narrow tab, "⋯" is the last control to go: it stays on screen and
// holds Save. "Not following" is said only while a followed stream runs.
#[gpui_kit::test]
async fn the_overflow_menu_stays_reachable_and_holds_save(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open_sized(cx, core, workload_request(), 320.);
    let mut buffer = LogBuffer::new(1000);
    for n in 0..100 {
        let mut line = LogLine::parse(&format!("2024-05-01T12:00:00Z GET /{n}"), false);
        line.source = Some(Arc::new(LineSource::new("api-79db669d5-cd27k", "api")));
        buffer.push(line);
    }
    let snapshot = |state| {
        Arc::new(LogSnapshot {
            lines: buffer.view(),
            state,
            paused: false,
            held_back: 0,
            version: 1,
            pods: None,
        })
    };
    feed(cx, &view, snapshot(LogState::Streaming));
    view.update(cx, |v, cx| v.set_query("GET".into(), cx));
    cx.wait_for(window, WAIT, |window, cx| {
        !view.read(cx).searching && window.try_find(line_id(99)).is_some()
    })
    .await;
    press(cx, window, "home");
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-not-following").is_some() && window.try_find("log-more").is_some()
    })
    .await;
    cx.update_window(window, |_, window, _| {
        // "⋯" and the search's arrows (among the controls when narrow) are all on screen.
        for id in ["log-more", "log-prev-match", "log-next-match"] {
            let b = window.find(id).bounds();
            assert!(
                f32::from(b.origin.x + b.size.width) <= 320. && f32::from(b.size.width) > 0.,
                "{id} on screen: {b:?}"
            );
        }
        let main = window.find("log-main").bounds();
        let next = window.find("log-next-match").bounds();
        assert!(
            next.origin.x >= main.origin.x + main.size.width,
            "not cut with the rest"
        );
    })
    .unwrap();
    cx.update_window(window, |_, window, cx| window.click("log-more", cx))
        .unwrap();
    let entries = menu_entries(cx, window);
    assert!(
        entries.iter().any(|e| e == "Save All Lines…"),
        "{entries:?}"
    );
    press(cx, window, "escape");

    // An ended stream has nothing to follow.
    feed(
        cx,
        &view,
        snapshot(LogState::Ended {
            error: None,
            pod_deleted: false,
        }),
    );
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find("log-not-following").is_none() && window.try_find("log-jump").is_some()
    })
    .await;
}

// covers UP-43: with timestamps shown, a dated separator starts the lines and each new day;
// the toggle's tooltip names the zone.
#[gpui_kit::test]
async fn a_dated_separator_starts_each_day(cx: &mut TestAppContext) {
    let (core, _dir) = core();
    let (window, view) = open(cx, core);
    let mut buffer = LogBuffer::new(100);
    for text in [
        "2026-11-19T23:59:58Z a",
        "2026-11-19T23:59:59Z b",
        "2026-11-20T00:00:00Z c",
    ] {
        buffer.push(LogLine::parse(text, false));
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
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(line_id(2)).is_some()
    })
    .await;
    assert!(
        cx.update_window(window, |_, window, _| window
            .try_find(("log-day", 0usize))
            .is_none())
            .unwrap(),
        "no dates without timestamps"
    );
    press(cx, window, "t");
    cx.wait_for(window, WAIT, |window, _| {
        window.try_find(("log-day", 0usize)).is_some()
            && window.try_find(("log-day", 2usize)).is_some()
    })
    .await;
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find(("log-day", 1usize)).is_none());
        // The separator sits above its line.
        let day = window.find(("log-day", 2usize)).bounds();
        let line = window.find(line_id(2)).bounds();
        assert!(day.origin.y < line.origin.y);
    })
    .unwrap();
    assert!(view.read_with(cx, |v, _| !v.tz_name.is_empty()));
}
