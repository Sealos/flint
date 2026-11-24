//! [`App`]: the real Flint app, started with the test-only automation channel and driven over
//! its socket (docs/TESTING.md, "Automation channel").
//!
//! The app always gets a scratch `FLINT_CONFIG_DIR` (0700, under `/tmp` so the socket path stays
//! short), a scratch `HOME`, a `settings.toml` with a fixed `PATH` (no login shell), and exactly
//! the `KUBECONFIG` the test names (or none, with [`AppOptions::no_kubeconfig`], when the scratch
//! `HOME` has no `.kube/config`). Inherited `KUBECONFIG` and `FLINT_*` variables are dropped.
//!
//! The app never outlives the test: it runs in its own process group with its stdin on a pipe
//! the harness holds, and quits when that pipe closes, even if the test process is killed.
//!
//! ```no_run
//! # fn example(kubeconfig: &std::path::Path) -> anyhow::Result<()> {
//! use flint_e2e::app::{App, AppOptions, Test};
//!
//! let app = App::launch(AppOptions::new(kubeconfig))?;
//! app.select_cluster("kind-flint-e2e-fake")?;
//! app.wait_for("/main/view", Test::Equals("connected".into()))?;
//! app.open_view("pods")?;
//! app.wait_for("/list/rows", Test::at_least(3))?;
//! app.screenshot("pods")?;
//! # Ok(())
//! # }
//! ```
//!
//! When the app goes away ([`App::quit`], or dropping it), [`crate::teardown`] checks what every
//! test must leave true: no fake server contacted before the test selected it, or at all when it
//! never did (UP-91); the kubeconfig files untouched (UP-90); the app exited on Quit and removed
//! its socket. A violation fails the test. Tests select clusters with [`App::select_cluster`]
//! (or say so with [`App::mark_selected`]), change kubeconfig files inside
//! [`App::edit_kubeconfig`], and start on a cluster with [`AppOptions::cluster_at_startup`].
//!
//! Only one [`App`] runs at a time in a test process: [`App::launch`] waits for the previous one
//! to be dropped, so `cargo test` can run app tests on several threads without starting several
//! windows at once.

use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::binaries;
use crate::scratch::Scratch;
use crate::teardown::{self, Teardown};

/// The default wait for data to arrive in the app: most waits end in well under a second, but a
/// cold debug build on a busy machine can be slow.
pub const WAIT: Duration = Duration::from_secs(20);

/// How long the app may take to open its socket.
const STARTUP: Duration = Duration::from_secs(60);

/// How long a request may take when it is not a `wait_for`.
const ANSWER: Duration = Duration::from_secs(60);

/// How long the quit request on drop may take before the app is killed.
const QUIT_ANSWER: Duration = Duration::from_secs(2);
/// The title of the question Quit asks while a session would end (#177).
const QUIT_DIALOG: &str = "Quit Flint?";
/// How long a stalled app has to answer the teardown's quit check before the check fails.
const QUIT_CHECK: Duration = Duration::from_secs(15);
/// How long the app has to exit once it took the quit.
const QUIT_EXIT: Duration = Duration::from_secs(5);
/// How long an app that failed the check has to shut down: its own limit after stdin closes
/// (three seconds) and a margin.
const SHUT_DOWN: Duration = Duration::from_secs(4);

/// The exit code of a test build whose automation channel refused to start.
pub const REFUSED_EXIT_CODE: i32 = 3;

/// Where screenshots go when set (an absolute directory; created if missing). Otherwise they stay
/// in the scratch config directory and go away with it.
pub const SCREENSHOTS_ENV: &str = "FLINT_AUTOMATION_SCREENSHOTS";

/// The cluster the app activates once its kubeconfig is loaded.
const CLUSTER_ENV: &str = "FLINT_CLUSTER";

/// Tells the app to quit when its stdin closes (`flint::ui::automation`).
const EXIT_ON_STDIN_EOF_ENV: &str = "FLINT_AUTOMATION_EXIT_ON_STDIN_EOF";

/// Variables the harness sets itself; [`AppOptions::env`] may not change them.
const RESERVED_ENV: [&str; 4] = [
    "KUBECONFIG",
    "HOME",
    "FLINT_CONFIG_DIR",
    "FLINT_AUTOMATION_",
];

/// How much of the app's log an error carries.
const LOG_LINES: usize = 40;
/// How much of the last state an error carries.
const STATE_CHARS: usize = 3000;

/// One app at a time per test process.
static TURN: Mutex<()> = Mutex::new(());

/// How to start the app.
#[derive(Debug)]
pub struct AppOptions {
    /// `None`: `KUBECONFIG` unset.
    kubeconfig: Option<OsString>,
    settings: String,
    env: Vec<(String, String)>,
    scratch: Option<Scratch>,
    cluster_at_startup: Option<String>,
}

impl AppOptions {
    /// The app on exactly this kubeconfig (one file).
    pub fn new(kubeconfig: impl AsRef<Path>) -> Self {
        Self::with_kubeconfig_list(kubeconfig.as_ref().as_os_str())
    }

    /// The app on a `KUBECONFIG` list (`a:b`), as a shell would set it.
    pub fn with_kubeconfig_list(kubeconfig: impl AsRef<OsStr>) -> Self {
        AppOptions {
            kubeconfig: Some(kubeconfig.as_ref().to_owned()),
            settings: String::new(),
            env: Vec::new(),
            scratch: None,
            cluster_at_startup: None,
        }
    }

    /// The app with `KUBECONFIG` unset: it looks for the scratch `HOME`'s `.kube/config`, which
    /// does not exist, so it finds nothing (UP-02).
    pub fn no_kubeconfig() -> Self {
        AppOptions {
            kubeconfig: None,
            settings: String::new(),
            env: Vec::new(),
            scratch: None,
            cluster_at_startup: None,
        }
    }

    /// Lines for `settings.toml`, after the fixed `PATH`.
    pub fn settings(mut self, settings: &str) -> Self {
        self.settings = settings.to_owned();
        self
    }

    /// One more environment variable, such as `FLINT_APPEARANCE`. `KUBECONFIG`, `HOME`,
    /// `FLINT_CONFIG_DIR` and `FLINT_AUTOMATION_*` are the harness's own, and `FLINT_CLUSTER` is
    /// set with [`AppOptions::cluster_at_startup`]: [`App::launch`] refuses them.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Starts on `cluster` (`FLINT_CLUSTER`): the app contacts it without a selection, so the
    /// teardown treats it as selected from the launch.
    pub fn cluster_at_startup(mut self, cluster: &str) -> Self {
        self.cluster_at_startup = Some(cluster.to_owned());
        self
    }

    /// Starts in the scratch directories of an app that has quit ([`App::quit`]), keeping its
    /// settings and UI state, as a relaunch does. `settings()` is ignored then.
    pub fn reuse(mut self, scratch: Scratch) -> Self {
        self.scratch = Some(scratch);
        self
    }

    fn check(&self) -> Result<()> {
        for (key, _) in &self.env {
            let reserved = RESERVED_ENV.iter().any(|r| {
                if r.ends_with('_') {
                    key.starts_with(r)
                } else {
                    key == r
                }
            });
            if reserved {
                bail!("AppOptions::env cannot set {key}: the harness sets it");
            }
            if key == CLUSTER_ENV {
                bail!("AppOptions::env cannot set {key}: use AppOptions::cluster_at_startup");
            }
        }
        if let Some(k) = &self.kubeconfig
            && std::env::split_paths(k).all(|p| p.as_os_str().is_empty())
        {
            bail!("the kubeconfig names no file; use AppOptions::no_kubeconfig for none");
        }
        Ok(())
    }
}

/// A failed automation request: the channel's `error.code` and message, the data it sent (for a
/// `wait_for` timeout, the last state), and the end of the app's log. Its `Display` shows all of
/// it, trimmed, so a failure through `?` explains itself.
#[derive(Debug, Clone)]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub data: Option<Box<Value>>,
    /// The last lines of the app's log when the request failed.
    pub log_tail: String,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(data) = &self.data {
            write!(f, "\n---- last state (trimmed) ----\n{}", trimmed(data))?;
        }
        if !self.log_tail.is_empty() {
            write!(f, "\n---- app log (tail) ----\n{}", self.log_tail)?;
        }
        Ok(())
    }
}

impl std::error::Error for AppError {}

/// `data` as JSON, cut to about [`STATE_CHARS`] characters.
fn trimmed(data: &Value) -> String {
    let text = data.to_string();
    match text.char_indices().nth(STATE_CHARS) {
        Some((cut, _)) => format!("{}… ({} more bytes)", &text[..cut], text.len() - cut),
        None => text,
    }
}

/// One `wait_for` test on the value at a JSON pointer.
#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Equals(Value),
    NotEquals(Value),
    /// A number, or an array's length, is at least this ([`Test::at_least`]).
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
    /// [`Test::AtLeast`] from any number: `Test::at_least(3)`.
    pub fn at_least(n: impl Into<f64>) -> Test {
        Test::AtLeast(n.into())
    }

    /// [`Test::AtMost`] from any number.
    pub fn at_most(n: impl Into<f64>) -> Test {
        Test::AtMost(n.into())
    }

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

/// The running app. Dropping it quits the app (and kills its process group if it does not
/// quit), and prints the end of the app's log (libtest shows it for failing tests).
pub struct App {
    child: Option<Child>,
    /// Held open: the app quits when it closes.
    stdin: Option<ChildStdin>,
    scratch: Option<Scratch>,
    socket: PathBuf,
    log: PathBuf,
    screenshots: PathBuf,
    binary: PathBuf,
    next_id: std::sync::atomic::AtomicU64,
    /// The test said it is failing ([`App::mark_failing`]): the teardown prints its report
    /// instead of panicking over the test's own error.
    failing: std::sync::atomic::AtomicBool,
    /// The `KUBECONFIG` the app runs with, for [`App::write_settings`].
    kubeconfig: Option<OsString>,
    teardown: Mutex<Teardown>,
    _turn: MutexGuard<'static, ()>,
}

impl App {
    /// Builds (or finds, see [`binaries::app`]) `flint` with the automation channel, starts it
    /// and waits for its socket.
    pub fn launch(options: AppOptions) -> Result<App> {
        options.check()?;
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
        let mut teardown =
            Teardown::at_launch(kubeconfig_files(options.kubeconfig.clone(), &scratch));
        if let Some(cluster) = &options.cluster_at_startup {
            teardown.selecting(cluster);
        }
        let mut command = Command::new(&binary);
        for (key, _) in std::env::vars_os() {
            let key = key.to_string_lossy();
            if key == "KUBECONFIG" || key.starts_with("FLINT_") {
                command.env_remove(&*key);
            }
        }
        if let Some(kubeconfig) = &options.kubeconfig {
            command.env("KUBECONFIG", kubeconfig);
        }
        command
            .env("FLINT_CONFIG_DIR", &config_dir)
            .env("HOME", scratch.home())
            // Keeps the log file in the scratch home on Linux too.
            .env("XDG_STATE_HOME", scratch.home())
            .env("FLINT_LOG", "info")
            .env(EXIT_ON_STDIN_EOF_ENV, "1")
            .envs(options.env.iter().map(|(k, v)| (k, v)))
            .envs(options.cluster_at_startup.iter().map(|c| (CLUSTER_ENV, c)))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log)?)
            // Its own process group, so drop can kill it and anything it started.
            .process_group(0);
        if shots_env.is_some() {
            command.env(SCREENSHOTS_ENV, &screenshots);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", binary.display()))?;
        let stdin = child.stdin.take();
        let mut app = App {
            child: Some(child),
            stdin,
            scratch: Some(scratch),
            socket,
            log,
            screenshots,
            binary,
            next_id: Default::default(),
            failing: Default::default(),
            kubeconfig: options.kubeconfig.clone(),
            teardown: Mutex::new(teardown),
            _turn: turn,
        };
        if let Err(error) = app.wait_for_socket() {
            // The launch failed: that error is the test's failure, not the teardown's.
            app.abandon();
            return Err(error);
        }
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

    /// The process id of the running app.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("the app is running").id()
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
        lines[lines.len().saturating_sub(LOG_LINES)..].join("\n")
    }

    /// Sends one request on its own connection and returns its result.
    pub fn request(&self, cmd: &str, args: Value) -> Result<Value, AppError> {
        let timeout = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map_or(ANSWER, |ms| {
                Duration::from_millis(ms) + Duration::from_secs(30)
            });
        self.request_within(cmd, args, timeout)
    }

    /// [`App::request`] whose whole exchange (connecting, sending, reading the answer) must fit in
    /// `deadline`. The app may still run a request that timed out here.
    pub fn request_by(
        &self,
        cmd: &str,
        args: Value,
        deadline: Duration,
    ) -> Result<Value, AppError> {
        let line = self.request_line(cmd, &args);
        let answer = exchange_by(&self.socket, &line, deadline)
            .map_err(|e| self.error("transport", format!("{cmd}: {e}"), None))?;
        self.response(cmd, &answer)
    }

    fn request_within(&self, cmd: &str, args: Value, timeout: Duration) -> Result<Value, AppError> {
        let line = self.request_line(cmd, &args);
        let answer = exchange(&self.socket, &line, timeout)
            .map_err(|e| self.error("transport", format!("{cmd}: {e}"), None))?;
        self.response(cmd, &answer)
    }

    fn request_line(&self, cmd: &str, args: &Value) -> String {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        json!({"id": id, "cmd": cmd, "args": args}).to_string()
    }

    fn error(&self, code: &str, message: String, data: Option<Box<Value>>) -> AppError {
        AppError {
            code: code.to_owned(),
            message,
            data,
            log_tail: self.log_tail(),
        }
    }

    /// The result in a response line, or the error it reports.
    fn response(&self, cmd: &str, answer: &str) -> Result<Value, AppError> {
        let response: Value = serde_json::from_str(answer).map_err(|e| {
            self.error(
                "transport",
                format!("{cmd}: not a response ({e}): {answer:?}"),
                None,
            )
        })?;
        if response["ok"] == true {
            Ok(response["result"].clone())
        } else {
            let error = &response["error"];
            Err(self.error(
                error["code"].as_str().unwrap_or("unknown"),
                format!(
                    "{cmd}: {}",
                    error["message"].as_str().unwrap_or("no message")
                ),
                error.get("data").cloned().map(Box::new),
            ))
        }
    }

    /// Clicks a cluster in the sidebar. From here on the app may contact it (UP-91).
    pub fn select_cluster(&self, cluster: &str) -> Result<Value, AppError> {
        self.mark_selected(cluster);
        self.request("select_cluster", json!({ "cluster": cluster }))
    }

    /// Tells the teardown that the test selects `context` another way (the switcher, the
    /// palette), exposes it to AI, or contacts it itself, now: nothing must have contacted it
    /// yet, and anything may from here on (UP-91).
    pub fn mark_selected(&self, context: &str) {
        self.teardown().selecting(context);
    }

    /// Runs `edit`, the test's own change to the kubeconfig `files` (UP-06), and takes those files
    /// as they are after it as the ones the app must leave untouched (UP-90). Every other file
    /// keeps its baseline, and a named file that had already changed is still reported.
    pub fn edit_kubeconfig<T>(
        &self,
        files: impl IntoIterator<Item: AsRef<Path>>,
        edit: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let files: Vec<PathBuf> = files.into_iter().map(|f| f.as_ref().to_owned()).collect();
        self.teardown().kubeconfig_editing(&files);
        let result = edit();
        self.teardown().kubeconfig_edited(&files);
        result
    }

    /// Writes the app's `settings.toml` (see [`crate::scratch::write_settings`]). Kubeconfig
    /// files the new settings add (`kubeconfig_paths`) are snapshotted first, with the request
    /// counts of the servers the new file list names, before the app can read them, and a
    /// selected context the new order moves is checked from that moment (UP-90, UP-91). Tests
    /// change settings through this, not by writing the file.
    pub fn write_settings(&self, settings: &str) -> Result<()> {
        let staged = tempfile::tempdir()?;
        crate::scratch::write_settings(staged.path(), settings)?;
        let files = kubeconfig_files_in(self.kubeconfig.as_deref(), &self.home(), staged.path());
        self.teardown().settings_writing(files);
        let written = crate::scratch::write_settings(&self.config_dir(), settings);
        self.teardown().settings_written();
        written
    }

    /// Says the test is failing with an error it is about to return: dropping the app then
    /// prints the teardown's violations instead of panicking, so that error is the one shown.
    /// The teardown is otherwise fatal, whatever requests failed on the way.
    pub fn mark_failing(&self) {
        self.failing
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn teardown(&self) -> MutexGuard<'_, Teardown> {
        self.teardown.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Kills the app without the teardown checks.
    fn abandon(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            force_kill(&mut child, &[]);
            let _ = child.wait();
        }
    }

    /// Stops the app and runs the teardown checks.
    fn finish(&mut self) -> Vec<String> {
        let exit = self.stop().map_err(|e| format!("{e:#}"));
        let socket = self.socket.clone();
        self.teardown().finish(exit, &socket)
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
        self.send_action(action, args, None)
    }

    /// [`App::dispatch`] that fails with a `timeout` error naming the action and `deadline` if
    /// the whole exchange (connecting, sending, reading the answer) takes longer.
    ///
    /// The request has its own connection, closed on timeout, so a late answer cannot reach a
    /// later request. The app cannot be told to cancel: an action it already received may still
    /// run after this returns.
    pub fn dispatch_within(
        &self,
        action: &str,
        args: Value,
        deadline: Duration,
    ) -> Result<Value, AppError> {
        self.send_action(action, args, Some(deadline))
    }

    fn send_action(
        &self,
        action: &str,
        args: Value,
        deadline: Option<Duration>,
    ) -> Result<Value, AppError> {
        const CMD: &str = "dispatch_action";
        let mut request = json!({ "name": action });
        if !args.is_null() {
            request["args"] = args;
        }
        let Some(deadline) = deadline else {
            return self.request(CMD, request);
        };
        let line = self.request_line(CMD, &request);
        let answer = exchange_by(&self.socket, &line, deadline).map_err(|e| {
            let (code, message) = dispatch_failure(action, deadline, &e);
            self.error(code, message, None)
        })?;
        self.response(CMD, &answer)
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

    /// Left-clicks the drawn element `id` (a dialog's `ok`), as the mouse does, whatever holds
    /// focus.
    pub fn click(&self, id: &str) -> Result<Value, AppError> {
        self.request("click", json!({ "id": id }))
    }

    /// Right-clicks the drawn element `id` (a row's `row-<uid>`, from the state's
    /// `list.rows[].id`), as the mouse does: opens its context menu (`state.menu`).
    pub fn right_click(&self, id: &str) -> Result<Value, AppError> {
        self.request("right_click", json!({ "id": id }))
    }

    /// The visible view tree.
    pub fn state(&self) -> Result<Value, AppError> {
        self.request("state", Value::Null)
    }

    /// Every registered action, with whether it is available now.
    pub fn actions(&self) -> Result<Value, AppError> {
        self.request("actions", Value::Null)
    }

    /// Waits up to [`WAIT`] until the value at `pointer` in the state passes `test`, and returns
    /// that state. A timeout's error carries what the app showed.
    pub fn wait_for(&self, pointer: &str, test: Test) -> Result<Value, AppError> {
        self.wait_for_within(pointer, test, WAIT)
    }

    /// [`App::wait_for`] with its own timeout.
    pub fn wait_for_within(
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
            self.wait_for("/appearance", Test::Equals(appearance.into()))?;
            shots.push(self.screenshot(&format!("{name}-{appearance}"))?);
        }
        Ok(shots)
    }

    /// Quits the app and waits for it to exit, keeping its scratch directories for
    /// [`AppOptions::reuse`], and runs the teardown checks: an error lists what they found.
    pub fn quit(mut self) -> Result<Scratch> {
        let violations = self.finish();
        if !violations.is_empty() {
            bail!("{}", report(&violations));
        }
        Ok(self.scratch.take().expect("the app's scratch directory"))
    }

    /// Asks the app to quit; kills its process group if it has not exited within a few seconds.
    fn stop(&mut self) -> Result<ExitStatus> {
        let Some(mut child) = self.child.take() else {
            bail!("the app was already stopped");
        };
        if is_running(&mut child)
            && let Err(problem) = self.quit_checked(&mut child)
        {
            self.shut_down(&mut child);
            bail!("{problem}");
        }
        // Closing stdin also tells the app to go.
        self.stdin.take();
        if is_running(&mut child) {
            force_kill(&mut child, &[]);
        }
        Ok(child.wait()?)
    }

    /// Quits through [`quit_unless_dialog`](Self::quit_unless_dialog) and waits for the app to
    /// exit. Fails on a dialog the test left open, or if the app never answered and is still
    /// running: an app that could not be inspected does not count as clear.
    fn quit_checked(&self, child: &mut Child) -> Result<(), String> {
        let mut deadline = Instant::now() + QUIT_CHECK;
        let mut no_answer = None;
        let mut asked = false;
        let mut confirmed = false;
        while is_running(child) && Instant::now() < deadline {
            // Quit asks first while a shell runs or a log tab follows (#177). Confirm only that
            // question, read off the open dialog's title: any other dialog is a bug to report,
            // not one to click through.
            let dialog = if !asked {
                // A request that timed out may still run. That is safe: it quits only with no
                // dialog open, so the app exiting afterwards means there was none.
                match self.quit_unless_dialog() {
                    Ok(dialog) => {
                        asked = true;
                        no_answer = None;
                        deadline = Instant::now() + QUIT_EXIT;
                        dialog
                    }
                    Err(error) => {
                        no_answer = Some(format!("{}: {}", error.code, error.message));
                        None
                    }
                }
            } else if confirmed {
                None
            } else {
                // The app may quit while it answers.
                self.request_within("state", Value::Null, QUIT_ANSWER)
                    .ok()
                    .and_then(|state| state["dialog"]["title"].as_str().map(str::to_owned))
            };
            match dialog.as_deref() {
                None => {}
                Some(QUIT_DIALOG) => {
                    let _ = self.request_within(
                        "dispatch_action",
                        json!({"name": "ui::Confirm"}),
                        QUIT_ANSWER,
                    );
                    confirmed = true;
                }
                Some(other) => {
                    return Err(format!(
                        "the teardown found an unexpected dialog open: {other:?}"
                    ));
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        match no_answer {
            Some(error) if is_running(child) => Err(format!(
                "the teardown could not check the app for open dialogs: {error}"
            )),
            _ => Ok(()),
        }
    }

    /// Has the app quit unless a dialog is open, in one step on its side. Answers with the open
    /// dialog's title.
    fn quit_unless_dialog(&self) -> Result<Option<String>, AppError> {
        const CMD: &str = "quit_unless_dialog";
        let line = self.request_line(CMD, &Value::Null);
        let answer = exchange_by(&self.socket, &line, QUIT_ANSWER)
            .map_err(|e| self.error("transport", format!("{CMD}: {e}"), None))?;
        let result = self.response(CMD, &answer)?;
        Ok(result["dialog"].as_str().map(str::to_owned))
    }

    /// Ends an app that failed the quit check: closing stdin lets it cancel its work, for at most
    /// [`SHUT_DOWN`]. Whatever it started that is still running is then killed.
    fn shut_down(&mut self, child: &mut Child) {
        let groups = child_groups(child.id());
        self.stdin.take();
        let deadline = Instant::now() + SHUT_DOWN;
        while is_running(child) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if is_running(child) {
            force_kill(child, &groups);
        } else {
            kill_groups(&groups);
        }
        let _ = child.wait();
    }
}

/// Whether the child has not exited. A child that cannot be polled counts as gone: `wait` then
/// reports the error.
fn is_running(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// A process group the app started, such as an exec credential plugin's: those run outside the
/// app's own group, so killing that one misses them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildGroup {
    id: libc::pid_t,
    /// When its leader started, to tell it from a later process given the same id.
    started: String,
}

/// One line of `ps`.
#[derive(Debug, PartialEq, Eq)]
struct Process {
    pid: libc::pid_t,
    ppid: libc::pid_t,
    pgid: libc::pid_t,
    started: String,
}

/// Every process, or none if `ps` fails: this runs during cleanup, which must carry on.
fn processes() -> Vec<Process> {
    // One `-o` each: in `-o a=,b=` the first column's header would be the rest of the argument.
    let listed = Command::new("ps")
        .args([
            "-A", "-o", "pid=", "-o", "ppid=", "-o", "pgid=", "-o", "lstart=",
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    listed.map_or_else(
        |_| Vec::new(),
        |ps| parse_processes(&String::from_utf8_lossy(&ps.stdout)),
    )
}

fn parse_processes(listing: &str) -> Vec<Process> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let pgid = fields.next()?.parse().ok()?;
            let started = fields.collect::<Vec<_>>().join(" ");
            Some(Process {
                pid,
                ppid,
                pgid,
                started,
            })
        })
        .collect()
}

/// The groups led by a direct child of `app`.
fn groups_of(processes: &[Process], app: libc::pid_t) -> Vec<ChildGroup> {
    processes
        .iter()
        .filter(|p| p.ppid == app && p.pid == p.pgid && p.pid > 1 && p.pid != app)
        .map(|p| ChildGroup {
            id: p.pid,
            started: p.started.clone(),
        })
        .collect()
}

fn child_groups(app: u32) -> Vec<ChildGroup> {
    libc::pid_t::try_from(app).map_or_else(|_| Vec::new(), |app| groups_of(&processes(), app))
}

/// The groups still safe to signal: a process now leading the id with another start time is not
/// the one recorded. A group whose leader is gone keeps its id while any member lives.
fn still_ours<'a>(groups: &'a [ChildGroup], processes: &[Process]) -> Vec<&'a ChildGroup> {
    groups
        .iter()
        .filter(|group| {
            processes
                .iter()
                .find(|p| p.pid == group.id)
                .is_none_or(|leader| leader.started == group.started)
        })
        .collect()
}

/// SIGKILLs `groups`, skipping any whose id now belongs to something else.
fn kill_groups(groups: &[ChildGroup]) {
    if groups.is_empty() {
        return;
    }
    for group in still_ours(groups, &processes()) {
        // SAFETY: killpg only sends a signal, to a group the app started.
        unsafe {
            libc::killpg(group.id, libc::SIGKILL);
        }
    }
}

/// SIGKILLs the app's process group (the child leads it: `process_group(0)`) and the groups it
/// started, `known` included. The app is stopped first: it cannot start or reap a plugin while
/// they are listed, so each id listed is still that plugin's.
fn force_kill(child: &mut Child, known: &[ChildGroup]) {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        return;
    };
    // Only while it is unreaped is the id still the app's group.
    if is_running(child) {
        // SAFETY: killpg only sends a signal; the group is the child's own.
        unsafe {
            libc::killpg(pid, libc::SIGSTOP);
        }
        let mut groups = known.to_vec();
        for group in child_groups(child.id()) {
            if !groups.contains(&group) {
                groups.push(group);
            }
        }
        kill_groups(&groups);
        // SAFETY: as above. SIGKILL also ends a stopped process.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// The teardown's violations, as a failure says them.
fn report(violations: &[String]) -> String {
    let mut text =
        String::from("app e2e teardown checks failed (docs/TESTING.md, \"Teardown checks\"):");
    for violation in violations {
        text.push_str("\n  - ");
        text.push_str(violation);
    }
    text
}

/// The kubeconfig files the app reads, as it finds them: the `KUBECONFIG` list (or the scratch
/// `HOME`'s `.kube/config` when unset), then the `kubeconfig_paths` of its `settings.toml` as it
/// is when called.
fn kubeconfig_files(kubeconfig: Option<OsString>, scratch: &Scratch) -> teardown::Files {
    let home = scratch.home();
    let config_dir = scratch.config_dir();
    Box::new(move || kubeconfig_files_in(kubeconfig.as_deref(), &home, &config_dir))
}

/// The kubeconfig files the app reads with `KUBECONFIG` set to `kubeconfig`, `HOME` at `home`
/// and the `settings.toml` in `config_dir`.
fn kubeconfig_files_in(kubeconfig: Option<&OsStr>, home: &Path, config_dir: &Path) -> Vec<PathBuf> {
    let base = flint_core::kubeconfig::base_paths(kubeconfig, Some(home));
    match flint_core::settings::load_settings(config_dir) {
        Ok(settings) => flint_core::kubeconfig::paths_for_settings(&base, &settings, Some(home)),
        Err(_) => base,
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // After App::quit, the checks have run.
        let violations = if self.child.is_some() {
            self.finish()
        } else {
            Vec::new()
        };
        // libtest captures this and shows it only for a failing test.
        eprintln!(
            "---- flint log ({}) ----\n{}\n---- end of flint log ----",
            self.log.display(),
            self.log_tail()
        );
        if violations.is_empty() {
            return;
        }
        let report = report(&violations);
        // A second panic would abort the test binary: the first failure stands, and this one
        // is in the test's output.
        // So does a test that said it is failing (`App::mark_failing`): its error is the one to
        // read, and the report is printed after the log.
        if std::thread::panicking() || self.failing.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("{report}");
        } else {
            panic!("{report}");
        }
    }
}

/// Sends `line` on a new connection and reads one answer line, each read limited to `timeout`.
fn exchange(socket: &Path, line: &str, timeout: Duration) -> io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(format!("{line}\n").as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    Ok(answer)
}

/// [`exchange`] with one limit on all of it, so an answer arriving a byte at a time cannot
/// outlast it. Fails with [`io::ErrorKind::TimedOut`]. Connecting runs on a helper thread, as a
/// blocking connect to a full backlog has no timeout; if it connects late, nothing is sent.
fn exchange_by(socket: &Path, line: &str, limit: Duration) -> io::Result<String> {
    let start = Instant::now();
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, "deadline passed");
    // Capped because a socket refuses very long timeouts; the loops below re-arm it.
    let left = || {
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            Err(timed_out())
        } else {
            Ok(left.min(ANSWER))
        }
    };
    let retry = |e: &io::Error| {
        matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
        )
    };

    let (connected, connection) = std::sync::mpsc::channel();
    let path = socket.to_owned();
    std::thread::Builder::new()
        .name("automation-connect".into())
        .spawn(move || {
            let _ = connected.send(UnixStream::connect(path));
        })?;
    let mut stream = connection.recv_timeout(limit).map_err(|_| timed_out())??;

    let request = format!("{line}\n");
    let mut unsent = request.as_bytes();
    while !unsent.is_empty() {
        stream.set_write_timeout(Some(left()?))?;
        match stream.write(unsent) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => unsent = &unsent[n..],
            Err(e) if retry(&e) => {}
            Err(e) => return Err(e),
        }
    }

    let mut answer = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        stream.set_read_timeout(Some(left()?))?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                answer.extend_from_slice(&chunk[..n]);
                if let Some(end) = answer.iter().position(|&b| b == b'\n') {
                    answer.truncate(end + 1);
                    break;
                }
            }
            Err(e) if retry(&e) => {}
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(answer).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The error code and message of a [`App::dispatch_within`] that got no answer.
fn dispatch_failure(action: &str, deadline: Duration, error: &io::Error) -> (&'static str, String) {
    if error.kind() == io::ErrorKind::TimedOut {
        (
            "timeout",
            format!("dispatch_action: {action}: no answer within {deadline:?}"),
        )
    } else {
        ("transport", format!("dispatch_action: {action}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    /// Reads one line from a connection.
    fn read_request(stream: &UnixStream) -> String {
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        line
    }

    #[test]
    fn a_silent_app_fails_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        // Never accepts: the request sits unanswered in the socket.
        let _listener = UnixListener::bind(&path).unwrap();
        let limit = Duration::from_millis(50);
        let start = Instant::now();
        let error = exchange_by(&path, "{}", limit).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() >= limit);
        assert!(start.elapsed() < ANSWER, "waited for the default");
        let (code, message) = dispatch_failure("detail::ShowYaml", limit, &error);
        assert_eq!(code, "timeout");
        assert!(message.contains("detail::ShowYaml"), "{message}");
        assert!(message.contains("50ms"), "{message}");
    }

    // Each byte restarts a socket read timeout, so only an overall deadline ends this. The peer
    // stops when the client hangs up, or after a few seconds, so a regressed deadline ends in a
    // failed assertion instead of a hang with an unbounded answer buffer.
    #[test]
    fn a_trickling_answer_cannot_outlast_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request(&stream);
            let give_up = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < give_up && stream.write_all(b"x").is_ok() {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let error = exchange_by(&path, "{}", Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        peer.join().unwrap();
    }

    // The peer answers the first request only once the client has given up on it, the order
    // that would pair a late answer with the next request on a shared connection.
    #[test]
    fn a_late_answer_does_not_reach_the_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let peer = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(first.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line.clear();
            // End of file: the client closed this connection.
            assert_eq!(reader.read_line(&mut line).unwrap(), 0);
            let _ = first.write_all(b"late\n");
            let (second, _) = listener.accept().unwrap();
            assert_eq!(read_request(&second), "second\n");
            (&second).write_all(b"answer\n").unwrap();
        });
        let error = exchange_by(&path, "first", Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let answer = exchange_by(&path, "second", ANSWER).unwrap();
        assert_eq!(answer, "answer\n");
        peer.join().unwrap();
    }

    #[test]
    fn only_groups_led_by_the_apps_children_are_killed() {
        let listing = "\
            1 0 1 Mon Nov 23 10:00:00 2026\n\
            100 1 100 Mon Nov 23 10:00:01 2026\n\
            200 100 200 Mon Nov 23 10:00:02 2026\n\
            201 200 200 Mon Nov 23 10:00:03 2026\n\
            300 100 100 Mon Nov 23 10:00:04 2026\n\
            400 1 400 Mon Nov 23 10:00:05 2026\n\
            not a process\n";
        let processes = parse_processes(listing);
        assert_eq!(processes.len(), 6);
        let groups = groups_of(&processes, 100);
        assert_eq!(
            groups,
            [ChildGroup {
                id: 200,
                started: "Mon Nov 23 10:00:02 2026".into(),
            }]
        );
    }

    #[test]
    fn a_group_id_given_to_another_process_is_left_alone() {
        let group = |id, started: &str| ChildGroup {
            id,
            started: started.into(),
        };
        let groups = [group(200, "then"), group(201, "then"), group(202, "then")];
        // 200 is still the plugin, 201 is another process now, and 202's leader is gone.
        let now = parse_processes("200 1 200 then\n201 1 201 later\n203 1 202 then\n");
        let kept: Vec<_> = still_ours(&groups, &now).iter().map(|g| g.id).collect();
        assert_eq!(kept, [200, 202]);
    }

    #[test]
    fn tests_become_wait_for_arguments() {
        assert_eq!(Test::at_least(2).arg(), ("at_least", json!(2.0)));
        assert_eq!(Test::at_most(1.5).arg(), ("at_most", json!(1.5)));
        assert_eq!(Test::Exists(false).arg(), ("exists", json!(false)));
        assert_eq!(
            Test::Any(json!({"name": "web"})).arg(),
            ("any", json!({"name": "web"}))
        );
        assert_eq!(Test::Equals("x".into()).arg(), ("equals", json!("x")));
    }

    #[test]
    fn errors_show_the_state_and_the_log() {
        let error = AppError {
            code: "timeout".into(),
            message: "wait_for: /list/rows did not match".into(),
            data: Some(Box::new(json!({"list": {"rows": "x".repeat(10_000)}}))),
            log_tail: "INFO connecting".into(),
        };
        let text = error.to_string();
        assert!(
            text.starts_with("timeout: wait_for: /list/rows did not match"),
            "{text}"
        );
        assert!(text.contains("more bytes)"), "trimmed: {}", text.len());
        assert!(text.len() < 4000, "{}", text.len());
        assert!(text.ends_with("INFO connecting"), "{text}");
    }

    #[test]
    fn options_refuse_what_the_harness_sets() {
        let options = AppOptions::with_kubeconfig_list("/a:/b")
            .settings("watch_mode = \"all\"\n")
            .env("FLINT_APPEARANCE", "dark");
        assert_eq!(options.kubeconfig.as_deref(), Some(OsStr::new("/a:/b")));
        assert!(options.check().is_ok());
        for key in [
            "KUBECONFIG",
            "HOME",
            "FLINT_CONFIG_DIR",
            "FLINT_AUTOMATION_SCREENSHOTS",
        ] {
            let error = AppOptions::new("/k").env(key, "x").check().unwrap_err();
            assert!(error.to_string().contains(key), "{error}");
        }
        assert!(AppOptions::with_kubeconfig_list("").check().is_err());
        assert!(AppOptions::with_kubeconfig_list(":").check().is_err());
        assert!(AppOptions::no_kubeconfig().check().is_ok());
    }
}
