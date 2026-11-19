//! [`App`]: the real Flint app, started with the test-only automation channel and driven over
//! its socket (docs/TESTING.md, "Automation channel").
//!
//! The app always gets a scratch `FLINT_CONFIG_DIR` (0700, under `/tmp` so the socket path stays
//! short), a scratch `HOME`, a `settings.toml` with a fixed `PATH` (no login shell), and exactly
//! the `KUBECONFIG` the test names. Inherited `KUBECONFIG` and `FLINT_*` variables are dropped.
//!
//! ```no_run
//! # fn example(kubeconfig: &std::path::Path) -> anyhow::Result<()> {
//! use flint_e2e::app::{App, AppOptions, Test, WAIT};
//!
//! let app = App::launch(AppOptions::new(kubeconfig))?;
//! app.select_cluster("flint-e2e-fake")?;
//! app.wait_for("/main/view", Test::Equals("connected".into()), WAIT)?;
//! app.open_view("pods")?;
//! app.wait_for("/list/rows", Test::AtLeast(3.0), WAIT)?;
//! app.screenshot("pods")?;
//! # Ok(())
//! # }
//! ```
//!
//! Only one [`App`] runs at a time in a test process: [`App::launch`] waits for the previous one
//! to be dropped, so `cargo test` can run app tests on several threads without starting several
//! windows at once.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::binaries;
use crate::scratch::Scratch;

/// A generous wait for data to arrive in the app: most waits end in well under a second, but a
/// cold debug build on a busy machine can be slow.
pub const WAIT: Duration = Duration::from_secs(20);

/// How long the app may take to open its socket.
const STARTUP: Duration = Duration::from_secs(60);

/// The exit code of a test build whose automation channel refused to start.
pub const REFUSED_EXIT_CODE: i32 = 3;

/// Where screenshots go when set (an absolute directory; created if missing). Otherwise they stay
/// in the scratch config directory and go away with it.
pub const SCREENSHOTS_ENV: &str = "FLINT_AUTOMATION_SCREENSHOTS";

/// One app at a time per test process.
static TURN: Mutex<()> = Mutex::new(());

/// How to start the app.
#[derive(Debug)]
pub struct AppOptions {
    kubeconfig: OsString,
    settings: String,
    env: Vec<(String, String)>,
    scratch: Option<Scratch>,
}

impl AppOptions {
    /// The app on exactly this kubeconfig (one file).
    pub fn new(kubeconfig: impl AsRef<Path>) -> Self {
        Self::with_kubeconfig_list(kubeconfig.as_ref().as_os_str())
    }

    /// The app on a `KUBECONFIG` list (`a:b`), as a shell would set it.
    pub fn with_kubeconfig_list(kubeconfig: impl AsRef<OsStr>) -> Self {
        AppOptions {
            kubeconfig: kubeconfig.as_ref().to_owned(),
            settings: String::new(),
            env: Vec::new(),
            scratch: None,
        }
    }

    /// Lines for `settings.toml`, after the fixed `PATH`.
    pub fn settings(mut self, settings: &str) -> Self {
        self.settings = settings.to_owned();
        self
    }

    /// One more environment variable, such as `FLINT_CLUSTER` or `FLINT_APPEARANCE`.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Starts in the scratch directories of an app that has quit ([`App::quit`]), keeping its
    /// settings and UI state, as a relaunch does. `settings()` is ignored then.
    pub fn reuse(mut self, scratch: Scratch) -> Self {
        self.scratch = Some(scratch);
        self
    }
}

/// A failed automation request: the channel's `error.code`, its message, and its data (for a
/// `wait_for` timeout, the last state).
#[derive(Debug, Clone)]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub data: Option<Value>,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

/// One `wait_for` test on the value at a JSON pointer.
#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Equals(Value),
    NotEquals(Value),
    /// A number, or an array's length, is at least this.
    AtLeast(f64),
    AtMost(f64),
    /// There and not null (`true`), or missing or null (`false`).
    Exists(bool),
    /// A string contains this text, or an array has an element matching this.
    Contains(Value),
    /// An array has an element with every field of this object.
    Any(Value),
}

impl Test {
    /// The test's name and value, as `wait_for` takes them.
    fn arg(&self) -> (&'static str, Value) {
        match self {
            Test::Equals(v) => ("equals", v.clone()),
            Test::NotEquals(v) => ("not_equals", v.clone()),
            Test::AtLeast(n) => ("at_least", json!(n)),
            Test::AtMost(n) => ("at_most", json!(n)),
            Test::Exists(b) => ("exists", json!(b)),
            Test::Contains(v) => ("contains", v.clone()),
            Test::Any(v) => ("any", v.clone()),
        }
    }
}

/// The running app. Dropping it quits the app (and kills it if it does not quit), and prints
/// the app's log if the test is failing.
pub struct App {
    child: Option<Child>,
    scratch: Option<Scratch>,
    socket: PathBuf,
    log: PathBuf,
    screenshots: PathBuf,
    binary: PathBuf,
    next_id: std::sync::atomic::AtomicU64,
    _turn: MutexGuard<'static, ()>,
}

impl App {
    /// Builds (or finds, see [`binaries::app`]) `flint` with the automation channel, starts it
    /// and waits for its socket.
    pub fn launch(options: AppOptions) -> Result<App> {
        let binary = binaries::app()?;
        let turn = TURN.lock().unwrap_or_else(PoisonError::into_inner);
        let reused = options.scratch.is_some();
        let scratch = match options.scratch {
            Some(scratch) => scratch,
            None => Scratch::new()?,
        };
        if !reused {
            scratch.write_settings(&options.settings)?;
        }
        let config_dir = std::fs::canonicalize(scratch.config_dir())?;
        let socket = config_dir.join("automation.sock");
        let shots_env = std::env::var_os(SCREENSHOTS_ENV).filter(|v| !v.is_empty());
        let screenshots = match &shots_env {
            Some(dir) => {
                let dir = PathBuf::from(dir);
                if !dir.is_absolute() {
                    bail!(
                        "{SCREENSHOTS_ENV} must be an absolute path: {}",
                        dir.display()
                    );
                }
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                std::fs::canonicalize(dir)?
            }
            None => config_dir.join("screenshots"),
        };
        let log = scratch.path().join(format!(
            "flint-{}.log",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis())
        ));
        let mut command = Command::new(&binary);
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if key == "KUBECONFIG" || key.starts_with("FLINT_") {
                command.env_remove(&*key);
            }
        }
        command
            .env("KUBECONFIG", &options.kubeconfig)
            .env("FLINT_CONFIG_DIR", &config_dir)
            .env("HOME", scratch.home())
            .env("FLINT_LOG", "info")
            .envs(options.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log)?);
        if shots_env.is_some() {
            command.env(SCREENSHOTS_ENV, &screenshots);
        }
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))?;
        let mut app = App {
            child: Some(child),
            scratch: Some(scratch),
            socket,
            log,
            screenshots,
            binary,
            next_id: Default::default(),
            _turn: turn,
        };
        app.wait_for_socket()?;
        Ok(app)
    }

    fn wait_for_socket(&mut self) -> Result<()> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child_mut().try_wait()? {
                let reason = if status.code() == Some(REFUSED_EXIT_CODE) {
                    "the automation channel refused to start"
                } else {
                    "the app exited before opening its socket"
                };
                bail!("{reason} ({status}). App log:\n{}", self.log_tail());
            }
            if UnixStream::connect(&self.socket).is_ok() {
                return Ok(());
            }
            if start.elapsed() > STARTUP {
                bail!(
                    "no automation socket at {} after {STARTUP:?}. App log:\n{}",
                    self.socket.display(),
                    self.log_tail()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child.as_mut().expect("the app is running")
    }

    /// The `flint` binary running, for `flint mcp` ([`crate::mcp::Mcp::flint`]).
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// The app's scratch `FLINT_CONFIG_DIR`, where `settings.toml` and `state.toml` live.
    pub fn config_dir(&self) -> PathBuf {
        self.scratch().config_dir()
    }

    /// The scratch `HOME` the app runs with.
    pub fn home(&self) -> PathBuf {
        self.scratch().home()
    }

    fn scratch(&self) -> &Scratch {
        self.scratch.as_ref().expect("the app's scratch directory")
    }

    /// Where screenshots are written.
    pub fn screenshots_dir(&self) -> &Path {
        &self.screenshots
    }

    /// The app's log (its stderr) so far.
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn log_tail(&self) -> String {
        let log = self.log();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(80)..].join("\n")
    }

    /// Sends one request on its own connection and returns its result.
    pub fn request(&self, cmd: &str, args: Value) -> Result<Value, AppError> {
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map_or(Duration::from_secs(60), |ms| {
                Duration::from_millis(ms) + Duration::from_secs(30)
            });
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let line = json!({"id": id, "cmd": cmd, "args": args}).to_string();
        let transport = |e: std::io::Error| AppError {
            code: "transport".into(),
            message: format!("{cmd}: {e}"),
            data: None,
        };
        let mut stream = UnixStream::connect(&self.socket).map_err(transport)?;
        stream.set_read_timeout(Some(timeout)).map_err(transport)?;
        stream
            .write_all(format!("{line}\n").as_bytes())
            .map_err(transport)?;
        let mut answer = String::new();
        BufReader::new(stream)
            .read_line(&mut answer)
            .map_err(transport)?;
        let response: Value = serde_json::from_str(&answer).map_err(|e| AppError {
            code: "transport".into(),
            message: format!("{cmd}: not a response ({e}): {answer:?}"),
            data: None,
        })?;
        if response["ok"] == true {
            Ok(response["result"].clone())
        } else {
            let error = &response["error"];
            Err(AppError {
                code: error["code"].as_str().unwrap_or("unknown").to_owned(),
                message: format!(
                    "{cmd}: {}",
                    error["message"].as_str().unwrap_or("no message")
                ),
                data: error.get("data").cloned(),
            })
        }
    }

    /// Clicks a cluster in the sidebar.
    pub fn select_cluster(&self, cluster: &str) -> Result<Value, AppError> {
        self.request("select_cluster", json!({ "cluster": cluster }))
    }

    /// Clicks a kind in the navigation (`pods`, `deployments.apps`).
    pub fn open_view(&self, kind: &str) -> Result<Value, AppError> {
        self.request("open_view", json!({ "kind": kind }))
    }

    /// Clicks a row of the open list.
    pub fn select_row(&self, namespace: Option<&str>, name: &str) -> Result<Value, AppError> {
        self.request(
            "select_row",
            json!({ "namespace": namespace, "name": name }),
        )
    }

    /// Dispatches a registered action by name (`detail::ShowYaml`, `flint::AppearanceDark`).
    /// `args` is `Value::Null` for an action without arguments.
    pub fn dispatch(&self, action: &str, args: Value) -> Result<Value, AppError> {
        let mut request = json!({ "name": action });
        if !args.is_null() {
            request["args"] = args;
        }
        self.request("dispatch_action", request)
    }

    /// Types text into the focused element, one keystroke per character.
    pub fn type_text(&self, text: &str) -> Result<Value, AppError> {
        self.request("type_text", json!({ "text": text }))
    }

    /// Presses a key or chord in GPUI syntax (`escape`, `enter`, `down`). Prefer
    /// [`App::dispatch`] for shortcuts: key bindings differ between platforms.
    pub fn press_key(&self, key: &str) -> Result<Value, AppError> {
        self.request("press_key", json!({ "key": key }))
    }

    /// The visible view tree.
    pub fn state(&self) -> Result<Value, AppError> {
        self.request("state", Value::Null)
    }

    /// Every registered action, with whether it is available now.
    pub fn actions(&self) -> Result<Value, AppError> {
        self.request("actions", Value::Null)
    }

    /// Waits until the value at `pointer` in the state passes `test`, and returns that state.
    /// A timeout's error says what the app showed.
    pub fn wait_for(
        &self,
        pointer: &str,
        test: Test,
        timeout: Duration,
    ) -> Result<Value, AppError> {
        let (name, value) = test.arg();
        let mut args = json!({
            "pointer": pointer,
            "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        });
        args[name] = value;
        self.request("wait_for", args)
    }

    /// Captures the window as `<name>.png` in [`App::screenshots_dir`] and returns its path. A
    /// file of that name from an earlier run is replaced.
    pub fn screenshot(&self, name: &str) -> Result<PathBuf, AppError> {
        let file = self.screenshots.join(format!("{name}.png"));
        if file.symlink_metadata().is_ok_and(|m| m.is_file()) {
            let _ = std::fs::remove_file(&file);
        }
        let result = self.request("screenshot", json!({ "name": name }))?;
        Ok(result["path"].as_str().map_or(file, PathBuf::from))
    }

    /// Captures the window in Flint Light and Flint Dark (`<name>-light.png`,
    /// `<name>-dark.png`), switching with the View > Appearance actions, and leaves the app in
    /// dark.
    pub fn screenshot_themes(&self, name: &str) -> Result<Vec<PathBuf>, AppError> {
        let mut shots = Vec::new();
        for (action, appearance) in [
            ("flint::AppearanceLight", "light"),
            ("flint::AppearanceDark", "dark"),
        ] {
            self.dispatch(action, Value::Null)?;
            self.wait_for("/appearance", Test::Equals(appearance.into()), WAIT)?;
            shots.push(self.screenshot(&format!("{name}-{appearance}"))?);
        }
        Ok(shots)
    }

    /// Quits the app and waits for it to exit, keeping its scratch directories for
    /// [`AppOptions::reuse`]. Returns them with the exit status.
    pub fn quit(mut self) -> Result<(Scratch, ExitStatus)> {
        let status = self.stop()?;
        let scratch = self.scratch.take().expect("the app's scratch directory");
        Ok((scratch, status))
    }

    /// Asks the app to quit; kills it if it has not exited within a few seconds.
    fn stop(&mut self) -> Result<ExitStatus> {
        let Some(mut child) = self.child.take() else {
            bail!("the app was already stopped");
        };
        if child.try_wait()?.is_none() {
            // The app may quit before it answers.
            let _ = self.dispatch("flint::Quit", Value::Null);
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait()?.is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if child.try_wait()?.is_none() {
            let _ = child.kill();
        }
        Ok(child.wait()?)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.stop();
        }
        if std::thread::panicking() {
            eprintln!(
                "---- flint log ({}) ----\n{}\n---- end of flint log ----",
                self.log.display(),
                self.log_tail()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_become_wait_for_arguments() {
        assert_eq!(Test::AtLeast(2.0).arg(), ("at_least", json!(2.0)));
        assert_eq!(Test::Exists(false).arg(), ("exists", json!(false)));
        assert_eq!(
            Test::Any(json!({"name": "web"})).arg(),
            ("any", json!({"name": "web"}))
        );
        assert_eq!(Test::Equals("x".into()).arg(), ("equals", json!("x")));
    }

    #[test]
    fn errors_name_the_code() {
        let error = AppError {
            code: "rejected".into(),
            message: "select_cluster: no such cluster".into(),
            data: None,
        };
        assert_eq!(
            error.to_string(),
            "rejected: select_cluster: no such cluster"
        );
    }

    #[test]
    fn options_keep_the_kubeconfig_list() {
        let options = AppOptions::with_kubeconfig_list("/a:/b")
            .settings("watch_mode = \"all\"\n")
            .env("FLINT_APPEARANCE", "dark");
        assert_eq!(options.kubeconfig, "/a:/b");
        assert_eq!(options.env, [("FLINT_APPEARANCE".into(), "dark".into())]);
    }
}
