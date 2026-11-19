//! The automation protocol: one JSON request per line, one JSON response per line.
//!
//! A request is `{"id": …, "cmd": "…", "args": {…}}`. `id` is any JSON value and is echoed
//! back; `args` may be left out for commands without arguments. A response is
//! `{"id": …, "ok": true, "result": …}` or `{"id": …, "ok": false, "error": {"code", "message"}}`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::predicate::Predicate;

/// A parsed request.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Make a kubeconfig context the active cluster, as clicking it does.
    SelectCluster { cluster: String },
    /// Open a kind's list on the active cluster, as clicking it in the navigation does.
    OpenView { kind: String },
    /// Select an object in the open list, as clicking its row does.
    SelectRow {
        namespace: Option<String>,
        name: String,
    },
    /// Dispatch a registered action by name, with its arguments.
    DispatchAction { name: String, args: Value },
    /// Type text into the focused element, one keystroke per character.
    TypeText { text: String },
    /// Press a key or chord, in GPUI syntax: `enter`, `escape`, `cmd-w`, `down`.
    PressKey { key: String },
    /// The visible view tree.
    State,
    /// Wait until `predicate` holds on `state`, or fail after `timeout_ms`.
    WaitFor {
        predicate: Predicate,
        timeout_ms: u64,
    },
    /// Capture the window as a PNG named `name` in the screenshots directory (a timestamp when
    /// left out). Only a plain file name: see [`super::setup::screenshot_file`].
    Screenshot { name: Option<String> },
    /// The registered actions, with whether each is available now: for finding out why a
    /// dispatch says `not_available`.
    Actions,
}

/// How long `wait_for` waits by default, and at most.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_TIMEOUT_MS: u64 = 120_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    id: Value,
    cmd: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClusterArgs {
    cluster: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KindArgs {
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowArgs {
    #[serde(default)]
    namespace: Option<String>,
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionArgs {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyArgs {
    key: String,
}

/// `wait_for`'s arguments: a pointer, one test (see [`super::predicate::TESTS`]) and maybe a
/// timeout. Checked by hand: serde cannot flatten with `deny_unknown_fields`.
fn wait_args(args: Value) -> Result<(Predicate, u64), Error> {
    let invalid =
        |message: String| Error::new(ErrorCode::InvalidArgs, format!("wait_for: {message}"));
    let Value::Object(mut map) = args else {
        return Err(invalid(
            "takes {\"pointer\", <test>, \"timeout_ms\"?}".to_owned(),
        ));
    };
    let pointer = match map.remove("pointer") {
        Some(Value::String(pointer)) => pointer,
        _ => return Err(invalid("needs a \"pointer\" string".to_owned())),
    };
    let timeout_ms = match map.remove("timeout_ms") {
        None => DEFAULT_TIMEOUT_MS,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| invalid("timeout_ms takes milliseconds".to_owned()))?,
    };
    let mut tests = map.into_iter();
    let (Some((test, value)), None) = (tests.next(), tests.next()) else {
        return Err(invalid(format!(
            "needs exactly one test: {}",
            super::predicate::TESTS.join(", ")
        )));
    };
    let predicate = Predicate::from_args(pointer, &test, value).map_err(invalid)?;
    Ok((predicate, timeout_ms.min(MAX_TIMEOUT_MS)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenshotArgs {
    #[serde(default)]
    name: Option<String>,
}

/// What went wrong, for the response's `error.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line is not a request.
    BadRequest,
    UnknownCommand,
    InvalidArgs,
    /// No such action.
    NotFound,
    /// The action exists but nothing in the window handles it now.
    NotAvailable,
    /// The action is available, but these arguments would do nothing (no such cluster, kind,
    /// row or column).
    Rejected,
    /// `wait_for` gave up.
    Timeout,
    /// The window is gone, or a screenshot could not be taken.
    Failed,
}

/// A failed request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    /// More about the failure, as data: `wait_for` puts the last state here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Box<Value>>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(Box::new(data));
        self
    }
}

/// The request's id, if the line is JSON with one (for answering a request not run).
pub fn request_id(line: &str) -> Value {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .unwrap_or(Value::Null)
}

/// Parses one request line into its id and command. On failure the id is still returned when
/// it could be read, so the error response can carry it.
pub fn parse(line: &str) -> Result<(Value, Command), (Value, Error)> {
    let request: Request = serde_json::from_str(line).map_err(|e| {
        (
            request_id(line),
            Error::new(ErrorCode::BadRequest, e.to_string()),
        )
    })?;
    let id = request.id;
    match command(&request.cmd, request.args) {
        Ok(command) => Ok((id, command)),
        Err(error) => Err((id, error)),
    }
}

fn command(cmd: &str, args: Value) -> Result<Command, Error> {
    Ok(match cmd {
        "select_cluster" => {
            let a: ClusterArgs = args_of(cmd, args)?;
            Command::SelectCluster { cluster: a.cluster }
        }
        "open_view" => {
            let a: KindArgs = args_of(cmd, args)?;
            Command::OpenView { kind: a.kind }
        }
        "select_row" => {
            let a: RowArgs = args_of(cmd, args)?;
            Command::SelectRow {
                namespace: a.namespace,
                name: a.name,
            }
        }
        "dispatch_action" => {
            let a: ActionArgs = args_of(cmd, args)?;
            Command::DispatchAction {
                name: a.name,
                args: a.args,
            }
        }
        "type_text" => {
            let a: TextArgs = args_of(cmd, args)?;
            Command::TypeText { text: a.text }
        }
        "press_key" => {
            let a: KeyArgs = args_of(cmd, args)?;
            Command::PressKey { key: a.key }
        }
        "state" => {
            no_args(cmd, &args)?;
            Command::State
        }
        "actions" => {
            no_args(cmd, &args)?;
            Command::Actions
        }
        "wait_for" => {
            let (predicate, timeout_ms) = wait_args(args)?;
            Command::WaitFor {
                predicate,
                timeout_ms,
            }
        }
        "screenshot" => {
            let a: ScreenshotArgs = if args.is_null() {
                ScreenshotArgs { name: None }
            } else {
                args_of(cmd, args)?
            };
            Command::Screenshot { name: a.name }
        }
        other => {
            return Err(Error::new(
                ErrorCode::UnknownCommand,
                format!("unknown command \"{other}\""),
            ));
        }
    })
}

fn args_of<T: DeserializeOwned>(cmd: &str, args: Value) -> Result<T, Error> {
    serde_json::from_value(args)
        .map_err(|e| Error::new(ErrorCode::InvalidArgs, format!("{cmd}: {e}")))
}

fn no_args(cmd: &str, args: &Value) -> Result<(), Error> {
    match args {
        Value::Null => Ok(()),
        Value::Object(map) if map.is_empty() => Ok(()),
        _ => Err(Error::new(
            ErrorCode::InvalidArgs,
            format!("{cmd} takes no arguments"),
        )),
    }
}

/// A response line (without the newline).
pub fn response(id: Value, outcome: Result<Value, Error>) -> String {
    #[derive(Serialize)]
    struct Response {
        id: Value,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<Error>,
    }
    let response = match outcome {
        Ok(result) => Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            id,
            ok: false,
            result: None,
            error: Some(error),
        },
    };
    serde_json::to_string(&response).expect("a response serializes")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ok(line: &str) -> Command {
        parse(line).unwrap().1
    }

    fn err(line: &str) -> (Value, ErrorCode) {
        let (id, error) = parse(line).unwrap_err();
        (id, error.code)
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(
            ok(r#"{"id":1,"cmd":"select_cluster","args":{"cluster":"kind-flint-dev"}}"#),
            Command::SelectCluster {
                cluster: "kind-flint-dev".into()
            }
        );
        assert_eq!(
            ok(r#"{"id":2,"cmd":"open_view","args":{"kind":"pods"}}"#),
            Command::OpenView {
                kind: "pods".into()
            }
        );
        assert_eq!(
            ok(r#"{"cmd":"select_row","args":{"namespace":"demo","name":"web"}}"#),
            Command::SelectRow {
                namespace: Some("demo".into()),
                name: "web".into()
            }
        );
        assert_eq!(
            ok(r#"{"cmd":"dispatch_action","args":{"name":"detail::Refresh"}}"#),
            Command::DispatchAction {
                name: "detail::Refresh".into(),
                args: Value::Null
            }
        );
        assert_eq!(
            ok(r#"{"cmd":"type_text","args":{"text":"web"}}"#),
            Command::TypeText { text: "web".into() }
        );
        assert_eq!(
            ok(r#"{"cmd":"press_key","args":{"key":"escape"}}"#),
            Command::PressKey {
                key: "escape".into()
            }
        );
        assert_eq!(ok(r#"{"id":"s","cmd":"state"}"#), Command::State);
        assert_eq!(ok(r#"{"cmd":"actions"}"#), Command::Actions);
        assert_eq!(
            ok(r#"{"cmd":"wait_for","args":{"pointer":"/list/shown","at_least":2}}"#),
            Command::WaitFor {
                predicate: Predicate::new("/list/shown").at_least(2.0),
                timeout_ms: DEFAULT_TIMEOUT_MS
            }
        );
        assert_eq!(
            ok(r#"{"cmd":"screenshot","args":{"name":"pods-dark"}}"#),
            Command::Screenshot {
                name: Some("pods-dark".into())
            }
        );
        assert_eq!(
            ok(r#"{"cmd":"screenshot"}"#),
            Command::Screenshot { name: None }
        );
    }

    #[test]
    fn caps_the_wait() {
        let Command::WaitFor { timeout_ms, .. } = ok(
            r#"{"cmd":"wait_for","args":{"pointer":"/x","exists":true,"timeout_ms":999999999}}"#,
        ) else {
            panic!("not a wait");
        };
        assert_eq!(timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn reports_errors_with_the_id() {
        assert_eq!(err("not json"), (Value::Null, ErrorCode::BadRequest));
        assert_eq!(
            err(r#"{"id":7,"cmd":"state","extra":1}"#),
            (json!(7), ErrorCode::BadRequest)
        );
        assert_eq!(
            err(r#"{"id":7,"cmd":"fly"}"#),
            (json!(7), ErrorCode::UnknownCommand)
        );
        assert_eq!(
            err(r#"{"id":"a","cmd":"open_view","args":{}}"#),
            (json!("a"), ErrorCode::InvalidArgs)
        );
        assert_eq!(
            err(r#"{"id":2,"cmd":"screenshot","args":{"path":"/tmp/x.png"}}"#),
            (json!(2), ErrorCode::InvalidArgs)
        );
        assert_eq!(
            err(r#"{"id":1,"cmd":"state","args":{"x":1}}"#),
            (json!(1), ErrorCode::InvalidArgs)
        );
    }

    #[test]
    fn responses_are_one_json_line() {
        let line = response(json!(3), Ok(json!({"rows": 2})));
        assert_eq!(line, r#"{"id":3,"ok":true,"result":{"rows":2}}"#);
        let line = response(
            json!("x"),
            Err(Error::new(ErrorCode::NotFound, "no such kind")),
        );
        assert_eq!(
            line,
            r#"{"id":"x","ok":false,"error":{"code":"not_found","message":"no such kind"}}"#
        );
        assert!(!line.contains('\n'));
    }
}
