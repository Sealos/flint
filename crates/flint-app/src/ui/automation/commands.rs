//! Runs automation commands against the window, through the same paths as user input:
//! registered actions for clicks and menu items, keystrokes for typing. Nothing here calls the
//! core.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use flint_core::redact::safe_error_summary;
use gpui_kit::*;
use serde_json::{Value, json};

use super::super::registry::{self, ActionError};
use super::super::workspace::Workspace;
use super::keys;
use super::protocol::{self, Command, Error, ErrorCode};

/// How often `wait_for` looks at the state again.
const POLL: Duration = Duration::from_millis(50);

/// The window commands act on.
#[derive(Clone)]
pub struct Target {
    pub window: AnyWindowHandle,
    pub workspace: Entity<Workspace>,
    /// Where screenshots go by default (`screenshots/` in it).
    pub config_dir: PathBuf,
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
        Command::TypeText { text } => {
            let handled = in_window(target, cx, |window, cx| {
                keys::keystrokes(&text)
                    .into_iter()
                    .filter(|k| window.dispatch_keystroke(k.clone(), cx))
                    .count()
            })?;
            settle(target, cx).await;
            Ok(json!({"keystrokes": text.chars().count(), "handled": handled}))
        }
        Command::PressKey { key } => {
            let handled = in_window(target, cx, |window, cx| {
                Keystroke::parse_with_os(&key, cx.platform_info().os)
                    .map(|k| window.dispatch_keystroke(k, cx))
            })?
            .map_err(|e| Error::new(ErrorCode::InvalidArgs, format!("press_key: {e}")))?;
            settle(target, cx).await;
            Ok(json!({"handled": handled}))
        }
        Command::State => state(target, cx),
        Command::WaitFor {
            predicate,
            timeout_ms,
        } => {
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let state = state(target, cx)?;
                if predicate.holds(&state) {
                    return Ok(state);
                }
                if Instant::now() >= deadline {
                    let seen = predicate.observed(&state).cloned().unwrap_or(Value::Null);
                    return Err(Error::new(
                        ErrorCode::Timeout,
                        format!(
                            "{} did not match within {timeout_ms} ms; it is {seen}",
                            predicate.pointer()
                        ),
                    ));
                }
                cx.background_executor().timer(POLL).await;
            }
        }
        Command::Screenshot { path } => {
            let path = path.unwrap_or_else(|| default_screenshot_path(&target.config_dir));
            settle(target, cx).await;
            let image = in_window(target, cx, |window, _| window.render_to_image())?
                .map_err(|e| failed("could not render the window", &e.to_string()))?;
            let (width, height) = image.dimensions();
            let job_path = path.clone();
            cx.background_spawn(async move {
                if let Some(dir) = job_path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                image
                    .save_with_format(&job_path, image::ImageFormat::Png)
                    .map_err(std::io::Error::other)
            })
            .await
            .map_err(|e| failed("could not save the screenshot", &e.to_string()))?;
            Ok(json!({"path": path, "width": width, "height": height}))
        }
    }
}

/// `screenshots/screenshot-<millis>.png` in the config directory.
fn default_screenshot_path(config_dir: &std::path::Path) -> PathBuf {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    config_dir
        .join("screenshots")
        .join(format!("screenshot-{millis}.png"))
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

/// The visible view tree as JSON.
fn state(target: &Target, cx: &mut AsyncApp) -> Result<Value, Error> {
    let state = with_workspace(target, cx, |ws, window, cx| ws.view_state(window, cx))?;
    serde_json::to_value(state).map_err(|e| failed("could not serialize the state", &e.to_string()))
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
        workspace.update(cx, |ws, cx| f(ws, window, cx))
    })
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

    use super::{Target, handle_line};
    use crate::ui::support_tests::{WAIT, open_workspace};

    async fn ask(cx: &mut TestAppContext, target: &Target, request: Value) -> Value {
        let mut async_cx = cx.to_async();
        let line = handle_line(&request.to_string(), target, &mut async_cx).await;
        assert!(!line.contains('\n'));
        serde_json::from_str(&line).unwrap()
    }

    // Commands drive the window the way a user does, and `state` shows the result.
    #[gpui_kit::test]
    async fn commands_drive_the_window(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let (window, model) = (t.window, t.model.clone());
        let target = Target {
            window,
            workspace: t.workspace.clone(),
            config_dir: dir.path().to_owned(),
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
        assert_eq!(names, ["alpha", "beta"]);

        let selected = ask(
            cx,
            &target,
            json!({"id": 2, "cmd": "select_cluster", "args": {"cluster": "beta"}}),
        )
        .await;
        assert_eq!(selected["ok"], true, "{selected}");
        let active = model.clone();
        cx.wait_for(window, WAIT, move |_, cx| {
            active.read(cx).active.as_deref() == Some("beta")
        })
        .await;
        let waited = ask(
            cx,
            &target,
            json!({"id": 3, "cmd": "wait_for", "args": {"pointer": "/active_cluster", "equals": "beta"}}),
        )
        .await;
        assert_eq!(waited["ok"], true, "{waited}");
        assert_eq!(waited["result"]["clusters"][1]["active"], true);

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
                json!({"id": 10, "cmd": "wait_for", "args": {"pointer": "/active_cluster", "equals": "alpha", "timeout_ms": 0}}),
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
}
