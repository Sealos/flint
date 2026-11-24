//! Runs automation commands against the window, through the same paths as user input:
//! registered actions for clicks and menu items, keystrokes for typing. Nothing here calls the
//! core.

use std::fs::OpenOptions;
use std::io::{Cursor, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Duration;

use flint_core::redact::safe_error_summary;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::*;
use serde_json::{Value, json};

use super::super::registry::{self, ActionError, Visibility};
use super::super::workspace::Workspace;
use super::keys;
use super::protocol::{self, Command, Error, ErrorCode};
use super::setup;

/// How often `wait_for` looks at the state again.
const POLL: Duration = Duration::from_millis(50);
/// The most text `type_text` takes, and how much it types between yields to the app.
pub const MAX_TEXT: usize = 4096;
const TEXT_CHUNK: usize = 64;
/// Before a screenshot, the window draws this many frames this far apart, so layout settles and
/// short animations (a tab underline, a dialog) finish: about half a second.
const SCREENSHOT_FRAMES: u32 = 15;
const FRAME: Duration = Duration::from_millis(33);
/// How many frames `resize_window` waits for the platform to apply the size: about two seconds.
const RESIZE_FRAMES: u32 = 60;

/// The window commands act on.
#[derive(Clone)]
pub struct Target {
    pub window: AnyWindowHandle,
    /// Weak: the channel's task lives as long as the app and must not keep the workspace alive.
    pub workspace: WeakEntity<Workspace>,
    /// Where screenshots are written.
    pub screenshots: PathBuf,
}

/// Answers one request line with one response line.
pub async fn handle_line(line: &str, target: &Target, cx: &mut AsyncApp) -> String {
    match protocol::parse(line) {
        Err((id, error)) => protocol::response(id, Err(error)),
        Ok((id, command)) => protocol::response(id, run(command, target, cx).await),
    }
}

async fn run(command: Command, target: &Target, cx: &mut AsyncApp) -> Result<Value, Error> {
    match command {
        // The registry's guards refuse an unknown cluster, a kind the cluster does not serve, or
        // a row not in the list as shown, before anything is dispatched.
        Command::SelectCluster { cluster } => {
            dispatch(
                target,
                cx,
                "workspace::SelectCluster",
                json!({ "cluster": cluster }),
            )
            .await
        }
        Command::OpenView { kind } => {
            dispatch(target, cx, "workspace::OpenView", json!({ "kind": kind })).await
        }
        Command::SelectRow { namespace, name } => {
            dispatch(
                target,
                cx,
                "resource_list::SelectRow",
                json!({ "namespace": namespace, "name": name }),
            )
            .await
        }
        Command::DispatchAction { name, args } => dispatch(target, cx, &name, args).await,
        Command::TypeText { text } => type_text(target, cx, &text).await,
        Command::PressKey { key } => {
            let handled = in_window(target, cx, |window, cx| {
                Keystroke::parse_with_os(&key, cx.platform_info().os)
                    .map(|k| window.dispatch_keystroke(keys::with_shift(k), cx))
            })?
            .map_err(|e| Error::new(ErrorCode::InvalidArgs, format!("press_key: {e}")))?;
            settle(target, cx).await;
            Ok(json!({"handled": handled}))
        }
        Command::RightClick { id } => right_click(target, cx, &id).await,
        Command::ResizeWindow { width, height } => {
            in_window(target, cx, |window, _| {
                window.resize(size(px(width), px(height)));
            })?;
            // The platform applies a resize later, from its own callback.
            let wanted = size(px(width), px(height));
            let mut viewport = in_window(target, cx, |window, _| window.viewport_size())?;
            for _ in 0..RESIZE_FRAMES {
                if viewport == wanted {
                    break;
                }
                cx.background_executor().timer(FRAME).await;
                viewport = in_window(target, cx, |window, _| window.viewport_size())?;
            }
            settle(target, cx).await;
            Ok(json!({
                "width": f32::from(viewport.width),
                "height": f32::from(viewport.height),
            }))
        }
        Command::State => state(target, cx),
        Command::Actions => actions(target, cx),
        Command::QuitUnlessDialog => quit_unless_dialog(target, cx),
        Command::ActivateWindow => activate_window(target, cx),
        Command::WaitFor {
            predicate,
            timeout_ms,
        } => {
            // The executor's clock, so tests can advance it.
            let executor = cx.background_executor().clone();
            let deadline = executor.now() + Duration::from_millis(timeout_ms);
            loop {
                let state = state(target, cx)?;
                if predicate.holds(&state) {
                    return Ok(state);
                }
                if executor.now() >= deadline {
                    return Err(Error::new(
                        ErrorCode::Timeout,
                        format!(
                            "{} did not match within {timeout_ms} ms: {}",
                            predicate.pointer(),
                            predicate.observed(&state)
                        ),
                    )
                    .with_data(state));
                }
                executor.timer(POLL).await;
            }
        }
        Command::Screenshot { name } => screenshot(target, cx, name).await,
    }
}

/// Types `text` one keystroke per character, a chunk at a time, letting the app run between
/// chunks.
async fn type_text(target: &Target, cx: &mut AsyncApp, text: &str) -> Result<Value, Error> {
    if text.len() > MAX_TEXT {
        return Err(Error::new(
            ErrorCode::InvalidArgs,
            format!("type_text takes at most {MAX_TEXT} bytes"),
        ));
    }
    let strokes = keys::keystrokes(text);
    let mut handled = 0;
    for chunk in strokes.chunks(TEXT_CHUNK) {
        handled += in_window(target, cx, |window, cx| {
            chunk
                .iter()
                .filter(|k| window.dispatch_keystroke((*k).clone(), cx))
                .count()
        })?;
        cx.background_executor().timer(Duration::ZERO).await;
    }
    settle(target, cx).await;
    Ok(json!({"keystrokes": strokes.len(), "handled": handled}))
}

/// Right-clicks the element `id` drawn in the window, as the mouse does (pointer move, button
/// down and up at its center), then draws twice: a context menu is built a frame after the
/// click. Refused when no element, or more than one, is drawn with that id, or it is out of view.
async fn right_click(target: &Target, cx: &mut AsyncApp, id: &str) -> Result<Value, Error> {
    settle(target, cx).await;
    let element = ElementId::Name(SharedString::from(id.to_owned()));
    in_window(target, cx, |window, cx| {
        let found = window.find_all(element.clone());
        match found.as_slice() {
            [] => Err(Error::new(
                ErrorCode::Rejected,
                format!("right_click: no element {id} is drawn"),
            )),
            [one] if !one.visible() => Err(Error::new(
                ErrorCode::Rejected,
                format!("right_click: {id} is drawn out of view"),
            )),
            [_] => {
                window.right_click(element, cx);
                Ok(())
            }
            many => Err(Error::new(
                ErrorCode::Rejected,
                format!("right_click: {} elements are drawn as {id}", many.len()),
            )),
        }
    })??;
    settle(target, cx).await;
    settle(target, cx).await;
    Ok(json!({"right_clicked": id}))
}

/// Draws the settled window and writes it as a PNG under the screenshots directory: a new file
/// only (never through a link, never over another file), mode 0600.
async fn screenshot(
    target: &Target,
    cx: &mut AsyncApp,
    name: Option<String>,
) -> Result<Value, Error> {
    let file = match name {
        Some(name) => {
            setup::screenshot_file(&name).map_err(|e| Error::new(ErrorCode::InvalidArgs, e))?
        }
        None => {
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            format!("screenshot-{millis}.png")
        }
    };
    let path = target.screenshots.join(&file);
    if path.symlink_metadata().is_ok() {
        return Err(Error::new(
            ErrorCode::InvalidArgs,
            format!(
                "{} exists; screenshots never replace a file",
                path.display()
            ),
        ));
    }
    for _ in 0..SCREENSHOT_FRAMES {
        settle(target, cx).await;
        cx.background_executor().timer(FRAME).await;
    }
    settle(target, cx).await;
    let image = in_window(target, cx, |window, _| window.render_to_image())?
        .map_err(|e| render_error(&e.to_string()))?;
    let (width, height) = image.dimensions();
    let job_path = path.clone();
    cx.background_spawn(async move {
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(std::io::Error::other)?;
        write_new_file(&job_path, &png)
    })
    .await
    .map_err(|e| failed("could not save the screenshot", &e.to_string()))?;
    Ok(json!({"path": path, "width": width, "height": height}))
}

/// A failed render: `not_available` where GPUI cannot render offscreen at all (its Linux
/// backend, and the headless test platform), `failed` for anything else.
fn render_error(error: &str) -> Error {
    // GPUI's messages: the platform default and the test platform's.
    let unsupported = error.contains("not implemented for this platform")
        || error.contains("no HeadlessRenderer");
    if unsupported {
        Error::new(
            ErrorCode::NotAvailable,
            format!(
                "screenshots are not available on this platform: {}",
                safe_error_summary(error)
            ),
        )
    } else {
        failed("could not render the window", error)
    }
}

/// Writes a new file, mode 0600: `O_CREAT|O_EXCL|O_NOFOLLOW`, so it never follows a link or
/// replaces a file already there.
fn write_new_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .write_all(bytes)
}

/// The registered actions, with what they take and whether they would run now.
fn actions(target: &Target, cx: &mut AsyncApp) -> Result<Value, Error> {
    in_window(target, cx, |window, cx| {
        let actions: Vec<Value> = registry::actions()
            .iter()
            .map(|spec| {
                json!({
                    "name": spec.name,
                    "label": spec.label,
                    "description": spec.description,
                    "category": format!("{:?}", spec.category),
                    "palette": spec.visibility == Visibility::Palette,
                    "args": spec.args.iter().map(|a| json!({
                        "name": a.name,
                        "description": a.description,
                        "required": a.required,
                        "kind": format!("{:?}", a.kind),
                    })).collect::<Vec<_>>(),
                    "shortcut": spec.shortcut(cx),
                    "enabled": spec.is_enabled(cx),
                    "available": spec.is_available(window, cx),
                })
            })
            .collect();
        json!({ "actions": actions })
    })
}

async fn dispatch(
    target: &Target,
    cx: &mut AsyncApp,
    name: &str,
    args: Value,
) -> Result<Value, Error> {
    in_window(target, cx, |window, cx| {
        registry::dispatch(name, args, window, cx)
    })?
    .map_err(action_error)?;
    settle(target, cx).await;
    Ok(json!({"dispatched": name}))
}

/// Draws the window now, so the next command sees what this one did: an action can move focus
/// to a view that exists only once drawn, and until then GPUI dispatches from the window's root,
/// past the views. Drawing here, as GPUI does before handling a key, also works while the window
/// is hidden or the screen is locked, when no frames come on their own.
async fn settle(target: &Target, cx: &mut AsyncApp) {
    draw(target, cx);
}

/// Draws the window now (see [`settle`]).
fn draw(target: &Target, cx: &mut AsyncApp) {
    let _ = in_window(target, cx, |window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    });
}

fn action_error(error: ActionError) -> Error {
    let code = match &error {
        ActionError::Unknown(_) => ErrorCode::NotFound,
        ActionError::InvalidArgs { .. } => ErrorCode::InvalidArgs,
        ActionError::NotAvailable(_) => ErrorCode::NotAvailable,
        ActionError::Rejected { .. } => ErrorCode::Rejected,
    };
    Error::new(code, error.to_string())
}

/// The visible view tree as JSON, from a frame drawn now: menus and dialogs are read from the
/// drawn elements, and a window in the background may get no frames of its own.
fn state(target: &Target, cx: &mut AsyncApp) -> Result<Value, Error> {
    draw(target, cx);
    let state = with_workspace(target, cx, |ws, window, cx| ws.view_state(window, cx))?;
    serde_json::to_value(state).map_err(|e| failed("could not serialize the state", &e.to_string()))
}

/// Reads the open dialog and, with none open, dispatches Quit. Nothing awaits between the two, so
/// no other command or task can open or close a dialog in between. A Quit that cannot be
/// dispatched is not an error here: the caller still ends the app another way.
fn quit_unless_dialog(target: &Target, cx: &mut AsyncApp) -> Result<Value, Error> {
    let title = state(target, cx)?["dialog"]["title"].clone();
    if title.is_null() {
        in_window(target, cx, |window, cx| {
            let _ = registry::dispatch("flint::Quit", Value::Null, window, cx);
        })?;
    }
    Ok(json!({ "dialog": title }))
}

/// Asks the platform to bring the app and the window to the front, from inside the app, and
/// answers whether GPUI reports the window as active, with what the platform says about the
/// request (macOS may refuse it). The request completes later, so callers repeat this until it
/// answers `true`.
fn activate_window(target: &Target, cx: &mut AsyncApp) -> Result<Value, Error> {
    in_window(target, cx, |window, cx| {
        if cx.active_window() == Some(target.window) {
            return json!({ "active": true });
        }
        cx.activate(true);
        window.activate_window();
        #[cfg(target_os = "macos")]
        let platform = super::activation::activate();
        #[cfg(not(target_os = "macos"))]
        let platform = Value::Null;
        json!({ "active": false, "platform": platform })
    })
}

fn in_window<R>(
    target: &Target,
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut Window, &mut App) -> R,
) -> Result<R, Error> {
    target
        .window
        .update(cx, |_, window, cx| f(window, cx))
        .map_err(|_| Error::new(ErrorCode::Failed, "the window is closed"))
}

fn with_workspace<R>(
    target: &Target,
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) -> R,
) -> Result<R, Error> {
    let workspace = target.workspace.clone();
    in_window(target, cx, |window, cx| {
        workspace
            .update(cx, |ws, cx| f(ws, window, cx))
            .map_err(|_| Error::new(ErrorCode::Failed, "the workspace is closed"))
    })?
}

/// An error from outside our own code (I/O, rendering), summarized as the UI would show it.
fn failed(what: &str, error: &str) -> Error {
    Error::new(
        ErrorCode::Failed,
        format!("{what}: {}", safe_error_summary(error)),
    )
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestAppContextExt as _;
    use serde_json::{Value, json};

    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use super::{Target, handle_line, write_new_file};
    use crate::ui::support_tests::{TestWorkspace, WAIT, open_workspace, pods, serve_pods};

    fn target(t: &TestWorkspace, screenshots: &std::path::Path) -> Target {
        Target {
            window: t.window,
            workspace: t.workspace.downgrade(),
            screenshots: screenshots.to_owned(),
        }
    }

    async fn ask(cx: &mut TestAppContext, target: &Target, request: Value) -> Value {
        let mut async_cx = cx.to_async();
        let line = handle_line(&request.to_string(), target, &mut async_cx).await;
        assert!(!line.contains('\n'));
        serde_json::from_str(&line).unwrap()
    }

    /// Runs the app until `found` answers, on a core that works on its own threads.
    fn until<T>(
        cx: &mut TestAppContext,
        mut found: impl FnMut(&mut TestAppContext) -> Option<T>,
    ) -> T {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            cx.run_until_parked();
            if let Some(value) = found(cx) {
                return value;
            }
            assert!(std::time::Instant::now() < deadline, "never happened");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // A build with this channel can be driven by any local process, so whatever reaches the
    // background sign-in toggle (a key on the focused control included) grants nothing.
    #[gpui_kit::test]
    async fn this_build_cannot_turn_a_background_sign_in_on(cx: &mut TestAppContext) {
        use flint_core::keep_watching::{BackgroundSignIn, KeepWatching};

        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("plugin-ran");
        let script = dir.path().join("plugin.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", ran.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let command = script.display().to_string();
        let t =
            crate::ui::support_tests::open_workspace_with(cx, dir.path(), move |dir, servers| {
                let path = dir.join("kubeconfig");
                let yaml = format!(
                    "apiVersion: v1\nkind: Config\nclusters:\n\
                 - {{name: a, cluster: {{server: \"{}\"}}}}\n\
                 - {{name: b, cluster: {{server: \"{}\"}}}}\n\
                 users:\n- {{name: u, user: {{token: t}}}}\n\
                 - name: x\n  user:\n    exec:\n      \
                 apiVersion: client.authentication.k8s.io/v1\n      \
                 command: \"{command}\"\n      interactiveMode: Never\n\
                 contexts:\n- {{name: kind-alpha, context: {{cluster: a, user: u}}}}\n\
                 - {{name: kind-beta, context: {{cluster: b, user: x}}}}\n",
                    servers.silent, servers.refused,
                );
                std::fs::write(&path, yaml).unwrap();
                vec![path]
            })
            .await;
        let beta = || "kind-beta".to_owned();
        let view = |cx: &mut TestAppContext| {
            t.model
                .read_with(cx, |model, _| model.keep_watching("kind-beta").cloned())
        };
        let shown = until(cx, |cx| view(cx));
        t.model.update(cx, |model, _| {
            model.set_keep_watching(beta(), shown.definition);
        });
        let waiting = until(cx, |cx| {
            view(cx).filter(|view| view.status == KeepWatching::NeedsConfirming)
        });
        assert!(waiting.shown_sign_in.is_some());

        t.model.update(cx, |model, cx| {
            model.turn_background_sign_in(beta(), waiting.shown_sign_in, cx);
        });
        let refused = until(cx, |cx| {
            t.model.read_with(cx, |model, _| model.last_error.clone())
        });
        assert!(refused.contains("automation"), "{refused}");
        for _ in 0..30 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(view(cx).unwrap().sign_in, BackgroundSignIn::Off);
        assert!(!ran.exists(), "the plugin ran");
    }

    // An open dialog is reported and nothing quits; with none open the same command quits.
    #[gpui_kit::test]
    async fn quit_unless_dialog_leaves_an_open_dialog_alone(cx: &mut TestAppContext) {
        use crate::ui::closing::Quits;
        use gpui_kit::AppContext as _;
        use gpui_kit::component::WindowExt as _;

        let quits =
            |cx: &mut TestAppContext| cx.update(|cx| cx.try_global::<Quits>().map_or(0, |q| q.0));
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let target = target(&t, dir.path());
        let quit = json!({"id": 1, "cmd": "quit_unless_dialog"});

        let opened = ask(
            cx,
            &target,
            json!({"id": 2, "cmd": "dispatch_action", "args": {
                "name": "workspace::ExposeToAi", "args": {"cluster": "kind-alpha"},
            }}),
        )
        .await;
        assert_eq!(opened["ok"], true, "{opened}");
        cx.wait_for(t.window, WAIT, |window, cx| window.has_active_dialog(cx))
            .await;
        let refused = ask(cx, &target, quit.clone()).await;
        assert!(refused["result"]["dialog"].is_string(), "{refused}");
        cx.run_until_parked();
        assert_eq!(quits(cx), 0, "a dialog is open");

        cx.update_window(t.window, |_, window, cx| {
            window.dispatch_action(Box::new(gpui_kit::component::dialog::Cancel), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let quitting = ask(cx, &target, quit).await;
        assert_eq!(quitting["result"]["dialog"], Value::Null, "{quitting}");
        cx.run_until_parked();
        assert_eq!(quits(cx), 1);
    }

    // Commands drive the window the way a user does, and `state` shows the result.
    #[gpui_kit::test]
    async fn commands_drive_the_window(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let (window, model) = (t.window, t.model.clone());
        let target = Target {
            window,
            workspace: t.workspace.downgrade(),
            screenshots: dir.path().to_owned(),
        };

        let state = ask(cx, &target, json!({"id": 1, "cmd": "state"})).await;
        assert_eq!(state["id"], 1);
        assert_eq!(state["ok"], true);
        let result = &state["result"];
        assert_eq!(result["main"], json!({"view": "no_cluster"}));
        assert_eq!(result["active_cluster"], Value::Null);
        let names: Vec<&str> = result["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["kind-alpha", "kind-beta"]);

        let selected = ask(
            cx,
            &target,
            json!({"id": 2, "cmd": "select_cluster", "args": {"cluster": "kind-beta"}}),
        )
        .await;
        assert_eq!(selected["ok"], true, "{selected}");
        let active = model.clone();
        cx.wait_for(window, WAIT, move |_, cx| {
            active.read(cx).active.as_deref() == Some("kind-beta")
        })
        .await;
        let waited = ask(
            cx,
            &target,
            json!({"id": 3, "cmd": "wait_for", "args": {"pointer": "/active_cluster", "equals": "kind-beta"}}),
        )
        .await;
        assert_eq!(waited["ok"], true, "{waited}");
        assert_eq!(waited["result"]["clusters"][1]["active"], true);
        // Nothing listens on kind-beta's port: the error says so in plain words, naming the server.
        let refused = format!(
            "connection refused by {}; is the API server running, and is the port right?",
            t.refused_server
        );
        let failed = ask(
            cx,
            &target,
            json!({"id": 3, "cmd": "wait_for", "args": {"pointer": "/main/error", "equals": refused}}),
        )
        .await;
        assert_eq!(failed["ok"], true, "{failed}");

        // Errors name what is wrong, with the request's id.
        let cases = [
            (
                json!({"id": 4, "cmd": "select_cluster", "args": {"cluster": "gamma"}}),
                "rejected",
            ),
            (
                json!({"id": 5, "cmd": "open_view", "args": {"kind": "pods"}}),
                "rejected",
            ),
            (
                json!({"id": 6, "cmd": "select_row", "args": {"name": "web"}}),
                "not_available",
            ),
            (
                json!({"id": 7, "cmd": "dispatch_action", "args": {"name": "detail::Close"}}),
                "not_available",
            ),
            (
                json!({"id": 8, "cmd": "dispatch_action", "args": {"name": "x::Y"}}),
                "not_found",
            ),
            (
                json!({"id": 9, "cmd": "press_key", "args": {"key": "not-a-key-"}}),
                "invalid_args",
            ),
            (
                json!({"id": 10, "cmd": "wait_for", "args": {"pointer": "/active_cluster", "equals": "kind-alpha", "timeout_ms": 0}}),
                "timeout",
            ),
        ];
        for (request, code) in cases {
            let id = request["id"].clone();
            let response = ask(cx, &target, request).await;
            assert_eq!(response["id"], id);
            assert_eq!(response["ok"], false, "{response}");
            assert_eq!(response["error"]["code"], code, "{response}");
        }

        // Keys go through the window's key handling.
        let typed = ask(
            cx,
            &target,
            json!({"id": 11, "cmd": "type_text", "args": {"text": "ab"}}),
        )
        .await;
        assert_eq!(typed["result"]["keystrokes"], 2, "{typed}");
        let pressed = ask(
            cx,
            &target,
            json!({"id": 12, "cmd": "press_key", "args": {"key": "escape"}}),
        )
        .await;
        assert_eq!(pressed["ok"], true, "{pressed}");
    }

    // open_view, select_row and dispatch_action succeed over the channel, as clicks would.
    #[gpui_kit::test]
    async fn view_and_row_commands_succeed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let target = target(&t, dir.path());
        let ok = |response: &Value| response["ok"] == true;

        let selected = ask(
            cx,
            &target,
            json!({"cmd": "select_cluster", "args": {"cluster": "kind-alpha"}}),
        )
        .await;
        assert!(ok(&selected), "{selected}");
        // The core reports the activation asynchronously; the guards follow the workspace.
        let workspace = t.workspace.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            workspace.read(cx).view_state_cluster() == Some("kind-alpha")
        })
        .await;
        serve_pods(cx, &t);
        let opened = ask(
            cx,
            &target,
            json!({"cmd": "open_view", "args": {"kind": "pods"}}),
        )
        .await;
        assert!(ok(&opened), "{opened}");
        // Rows by hand: the test cluster never answers.
        let list = t
            .workspace
            .read_with(cx, |ws, _| ws.list().cloned())
            .unwrap();
        list.update(cx, |list, cx| {
            list.stop_watching();
            list.set_snapshot(pods(&[("demo", "api", 0), ("demo", "web", 1)]), cx);
        });
        let shown = ask(
            cx,
            &target,
            json!({"cmd": "wait_for", "args": {"pointer": "/list/rows", "any": {"name": "web", "cells_by_column": {"Restarts": "1"}}}}),
        )
        .await;
        assert!(ok(&shown), "{shown}");
        assert_eq!(shown["result"]["list"]["rows"][1]["id"], "row-uid-web");

        // A right-click on the row opens its menu, which the state lists; ids not drawn once
        // are refused.
        let clicked = ask(
            cx,
            &target,
            json!({"cmd": "right_click", "args": {"id": "row-uid-web"}}),
        )
        .await;
        assert!(ok(&clicked), "{clicked}");
        let menu = ask(
            cx,
            &target,
            json!({"cmd": "wait_for", "args": {"pointer": "/menu/owner", "equals": "row-menu-uid-web"}}),
        )
        .await;
        assert!(ok(&menu), "{menu}");
        assert_eq!(menu["result"]["menu"]["entries"][0]["label"], "Logs");
        assert_eq!(menu["result"]["focus"], "menu");
        assert_eq!(
            menu["result"]["detail"]["name"], "web",
            "a right-click selects"
        );
        for id in ["row-uid-nope"] {
            let refused = ask(
                cx,
                &target,
                json!({"cmd": "right_click", "args": {"id": id}}),
            )
            .await;
            assert_eq!(refused["error"]["code"], "rejected", "{refused}");
        }
        // The menu is dismissed while nothing draws (no frame since): `state` draws first, so
        // it does not report a menu that is gone.
        use gpui_kit::AppContext as _;
        cx.update_window(target.window, |_, window, cx| {
            window.dispatch_action(Box::new(gpui_kit::component::dialog::Cancel), cx)
        })
        .unwrap();
        let state = ask(cx, &target, json!({"cmd": "state"})).await;
        assert_eq!(
            state["result"]["menu"],
            Value::Null,
            "{}",
            state["result"]["menu"]
        );

        let row = ask(
            cx,
            &target,
            json!({"cmd": "select_row", "args": {"namespace": "demo", "name": "web"}}),
        )
        .await;
        assert!(ok(&row), "{row}");
        let yaml = ask(
            cx,
            &target,
            json!({"cmd": "dispatch_action", "args": {"name": "detail::ShowYaml"}}),
        )
        .await;
        assert!(ok(&yaml), "{yaml}");
        let state = ask(cx, &target, json!({"cmd": "state"})).await;
        assert_eq!(state["result"]["detail"]["name"], "web");
        assert_eq!(state["result"]["detail"]["tab"], "yaml");

        // `actions` says what is available now and why not.
        let actions = ask(cx, &target, json!({"cmd": "actions"})).await;
        let find = |name: &str| {
            actions["result"]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["name"] == name)
                .cloned()
                .unwrap()
        };
        assert_eq!(find("detail::Close")["available"], true);
        assert_eq!(find("resource_list::ToggleWide")["enabled"], false);
        assert!(find("resource_list::FocusFilter")["shortcut"].is_string());
    }

    // wait_for polls on the app's clock until the state matches.
    #[gpui_kit::test]
    async fn wait_for_polls_until_the_state_matches(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let waiting = {
            let target = target(&t, dir.path());
            cx.spawn(async move |mut cx| {
                handle_line(
                    &json!({"id": 1, "cmd": "wait_for", "args": {"pointer": "/active_cluster", "equals": "kind-beta", "timeout_ms": 5000}}).to_string(),
                    &target,
                    &mut cx,
                )
                .await
            })
        };
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        t.model.update(cx, |m, _| m.activate("kind-beta".into()));
        let workspace = t.workspace.clone();
        cx.wait_for(t.window, WAIT, move |_, cx| {
            workspace.read(cx).view_state_cluster() == Some("kind-beta")
        })
        .await;
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        let response: Value = serde_json::from_str(&waiting.await).unwrap();
        assert_eq!(response["ok"], true, "{response}");
    }

    #[gpui_kit::test]
    async fn screenshot_names_stay_in_the_screenshots_directory(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let target = target(&t, dir.path());
        for name in ["../escape", "/tmp/x", "a/b"] {
            let response = ask(
                cx,
                &target,
                json!({"cmd": "screenshot", "args": {"name": name}}),
            )
            .await;
            assert_eq!(response["error"]["code"], "invalid_args", "{response}");
        }
        let response = ask(
            cx,
            &target,
            json!({"cmd": "screenshot", "args": {"path": "/tmp/x.png"}}),
        )
        .await;
        assert_eq!(response["error"]["code"], "invalid_args", "{response}");
        // GPUI's test platform has no headless renderer, so a valid name ends in `not_available`
        // here. Writing the file (new, 0600, no links) is tested on `write_new_file` below, and a
        // real screenshot by the flint-e2e app tests on macOS CI.
        let response = ask(
            cx,
            &target,
            json!({"cmd": "screenshot", "args": {"name": "view"}}),
        )
        .await;
        assert_eq!(response["error"]["code"], "not_available", "{response}");
        assert!(!dir.path().join("view.png").exists());
    }

    #[test]
    fn only_a_missing_renderer_is_not_available() {
        let code = |message: &str| super::render_error(message).code;
        assert_eq!(
            code("render_to_image not implemented for this platform"),
            super::ErrorCode::NotAvailable
        );
        assert_eq!(
            code("render_to_image not available: no HeadlessRenderer configured"),
            super::ErrorCode::NotAvailable
        );
        assert_eq!(code("Metal device lost"), super::ErrorCode::Failed);
    }

    #[test]
    fn screenshots_are_new_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        write_new_file(&path, b"png").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Never over an existing file, never through a link.
        assert!(write_new_file(&path, b"again").is_err());
        let link = dir.path().join("link.png");
        std::os::unix::fs::symlink(dir.path().join("target.png"), &link).unwrap();
        assert!(write_new_file(&link, b"x").is_err());
        assert!(!dir.path().join("target.png").exists());
    }

    #[gpui_kit::test]
    async fn text_is_capped(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let target = target(&t, dir.path());
        let long = "a".repeat(super::MAX_TEXT + 1);
        let response = ask(
            cx,
            &target,
            json!({"cmd": "type_text", "args": {"text": long}}),
        )
        .await;
        assert_eq!(response["error"]["code"], "invalid_args", "{response}");
    }

    // A timeout says what it saw, and carries the state.
    #[gpui_kit::test]
    async fn timeouts_explain_a_mistyped_pointer(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let target = target(&t, dir.path());
        let response = ask(
            cx,
            &target,
            json!({"cmd": "wait_for", "args": {"pointer": "/kubeconfig/load", "exists": true, "timeout_ms": 0}}),
        )
        .await;
        assert_eq!(response["error"]["code"], "timeout");
        let message = response["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(
                "no value at /kubeconfig/load; /kubeconfig has keys [env, loaded, paths]"
            ),
            "{message}"
        );
        assert_eq!(response["error"]["data"]["kubeconfig"]["loaded"], true);
    }
}
