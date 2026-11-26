//! Helpers shared by view tests that need the whole window: the workspace over kubeconfig
//! contexts on 127.0.0.1, core events fed in as the core sends them, and rows fed by hand.
//!
//! Views keep their state private. The few hooks these helpers need are inherent impls in test
//! files that are children of the view's module: [`Workspace`]'s in `workspace_tests.rs`,
//! [`ClustersModel`]'s in `clusters_tests.rs`.

use std::cell::RefCell;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use flint_core::{
    Cell, ClusterDiscovery, Column, ColumnRole, ColumnType, ConnectionState, CoreConfig,
    CoreHandle, Event, KindId, KindInfo, KindSnapshot, NavEntry, NavSection, Navigation, Row,
    SectionKind, ServerVersion, Settings, SyncState,
};
use gpui_kit::base::test_support::ElementSnapshot;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::{TestAppContextExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, ElementId, Entity, Point, Subscription,
    TestAppContext, WeakEntity, WindowBounds, WindowOptions, px, size,
};
use serde_json::{Value, json};

use super::clusters::ClustersModel;
use super::detail::DetailView;
use super::list::ResourceList;
use super::registry::{self, ActionError};
use super::ui_state::UiState;
use super::workspace::Workspace;

/// In a `DiscoveryChanged` given to [`TestWorkspace::feed`]: the cluster's current slot.
pub const CURRENT_GENERATION: u64 = u64::MAX;

/// How long a test waits for the UI to settle. Waiting advances the executor's clock in 10 ms
/// steps; it never sleeps.
pub const WAIT: Duration = Duration::from_secs(10);

/// The main window, once the core has loaded kubeconfig. With [`open_workspace`] it shows two
/// contexts: `kind-alpha`'s server accepts connections and never answers, so it stays "connecting"
/// and its views show; `kind-beta`'s refuses them. Nothing outside this process is reached.
pub struct TestWorkspace {
    pub window: AnyWindowHandle,
    pub workspace: Entity<Workspace>,
    pub model: Entity<ClustersModel>,
    /// The silent server's URL, as kubeconfig names it (`kind-alpha`'s server).
    pub server: String,
    /// `kind-beta`'s server: a local port nothing listens on.
    pub refused_server: String,
    /// The silent server. It never answers; tests can check that nothing connected to it.
    listener: TcpListener,
    /// The config dir, holding `settings.toml` and `state.toml`.
    pub dir: PathBuf,
    /// Every connection state each cluster has shown, in order (repeats collapsed).
    states: Rc<RefCell<Vec<(String, ConnectionState)>>>,
    _record_states: Subscription,
}

/// The workspace over `kind-alpha` (silent) and `kind-beta` (refused), with default settings.
pub async fn open_workspace(cx: &mut TestAppContext, dir: &Path) -> TestWorkspace {
    open_workspace_with(cx, dir, alpha_and_beta).await
}

/// Runs [`clear_proxy_variables`] as the test binary loads, before `main` and any thread.
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
static CLEAR_PROXY_VARIABLES: extern "C" fn() = clear_proxy_variables;

/// View tests point clusters at loopback fixtures, and many rely on an attempt that stays
/// connecting. The client refuses a proxy before it dials (and, built without kube's proxy
/// features, ignores `NO_PROXY`), so these variables would fail those attempts at once.
extern "C" fn clear_proxy_variables() {
    for name in ["HTTPS_PROXY", "https_proxy"] {
        // SAFETY: this runs before `main`, while the process has a single thread.
        unsafe { std::env::remove_var(name) };
    }
}

// The proxy variables of the environment the tests were started in do not reach them. Checked
// in a child started with both set, since CI runs without them.
#[test]
fn view_tests_run_without_proxy_variables() {
    const CHILD: &str = "FLINT_TEST_PROXY_VARIABLES_CHILD";
    if std::env::var_os(CHILD).is_some() {
        for name in ["HTTPS_PROXY", "https_proxy"] {
            assert_eq!(std::env::var_os(name), None, "{name}");
        }
        return;
    }
    let (_, module) = module_path!().split_once("::").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            &format!("{module}::view_tests_run_without_proxy_variables"),
            "--exact",
        ])
        .env(CHILD, "1")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("https_proxy", "http://127.0.0.1:9")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout.contains("1 passed"),
        "the child ran no test: {stdout}"
    );
}

/// The local servers a test's kubeconfig can point at.
pub struct Servers {
    /// Accepts connections and never answers.
    pub silent: String,
    /// A port nothing listens on: connections are refused.
    pub refused: String,
}

/// One kubeconfig file with `kind-alpha` on the silent server, and `kind-beta` on the refused one.
pub fn alpha_and_beta(dir: &Path, servers: &Servers) -> Vec<PathBuf> {
    let kubeconfig = dir.join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        kubeconfig_yaml(&[
            ("kind-alpha", &servers.silent),
            ("kind-beta", &servers.refused),
        ]),
    )
    .unwrap();
    vec![kubeconfig]
}

/// A kubeconfig with one context per (name, server), each with its own cluster entry.
pub fn kubeconfig_yaml(contexts: &[(&str, &str)]) -> String {
    let mut clusters = String::new();
    let mut entries = String::new();
    for (i, (name, server)) in contexts.iter().enumerate() {
        clusters.push_str(&format!(
            "- name: c{i}-{name}\n  cluster: {{server: \"{server}\"}}\n"
        ));
        entries.push_str(&format!(
            "- name: \"{name}\"\n  context: {{cluster: \"c{i}-{name}\", user: u}}\n"
        ));
    }
    format!(
        "apiVersion: v1\nkind: Config\nclusters:\n{clusters}users:\n- name: u\n  user: {{token: \
         t}}\ncontexts:\n{entries}"
    )
}

/// The workspace over the kubeconfig files `kubeconfigs` writes into the config dir `dir` (it
/// gets the local servers' URLs), with the settings in `dir/settings.toml` read as the app reads
/// them at launch. Waits until the core has loaded kubeconfig.
pub async fn open_workspace_with(
    cx: &mut TestAppContext,
    dir: &Path,
    kubeconfigs: impl FnOnce(&Path, &Servers) -> Vec<PathBuf>,
) -> TestWorkspace {
    cx.executor().allow_parking();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );
    // A port the OS just handed out, released again: nothing listens there.
    let refused_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let servers = Servers {
        silent: server.clone(),
        refused: format!("https://127.0.0.1:{refused_port}"),
    };
    let kubeconfig_base = kubeconfigs(dir, &servers);
    let (window, workspace, model) = open_window_over(cx, dir, kubeconfig_base);
    let states: Rc<RefCell<Vec<(String, ConnectionState)>>> = Rc::default();
    let record = states.clone();
    let record_states = cx.update(|cx| {
        cx.observe(&model, move |model, cx| {
            let mut states = record.borrow_mut();
            for c in &model.read(cx).clusters {
                let last = states
                    .iter()
                    .rev()
                    .find(|(name, _)| *name == c.info.name)
                    .map(|(_, s)| s);
                if last != Some(&c.state) {
                    states.push((c.info.name.clone(), c.state.clone()));
                }
            }
        })
    });
    let loaded = model.clone();
    cx.wait_for(window, WAIT, move |_, cx| loaded.read(cx).loaded)
        .await;
    TestWorkspace {
        states,
        _record_states: record_states,
        window,
        workspace,
        model,
        server,
        refused_server: servers.refused,
        listener,
        dir: dir.to_owned(),
    }
}

/// The main window over the kubeconfig files `kubeconfig_base` (which may not exist or hold no
/// contexts), with the settings in `dir/settings.toml` read as the app reads them at launch.
pub(super) fn open_window_over(
    cx: &mut TestAppContext,
    dir: &Path,
    kubeconfig_base: Vec<PathBuf>,
) -> (AnyWindowHandle, Entity<Workspace>, Entity<ClustersModel>) {
    let (settings, settings_error) = match flint_core::settings::load_settings(dir) {
        Ok(settings) => (settings, None),
        Err(e) => (Settings::default(), Some(e.to_string())),
    };
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.to_owned(),
        settings: settings.clone(),
        settings_error,
        kubeconfig_base,
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    })
    .unwrap();
    let appearance = settings.appearance;
    cx.update(|cx| {
        gpui_kit::init(cx);
        registry::init(cx);
        // The app menu's actions (Quit, Close Window), handled as in the app.
        super::install_menus(appearance, cx);
    });
    let ui_state = UiState::load(dir.to_owned());
    cx.update(|cx| {
        let model = cx.new(|cx| ClustersModel::new(core, settings, None, cx));
        let workspace_model = model.clone();
        let (window, workspace) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1100.), px(700.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| Workspace::new(workspace_model, ui_state, None, None, window, cx))
            },
        )
        .unwrap();
        (window, workspace, model)
    })
}

/// The main window over a kubeconfig file with no contexts, once it is loaded.
pub async fn open_empty_workspace(
    cx: &mut TestAppContext,
    dir: &Path,
) -> (AnyWindowHandle, Entity<Workspace>, Entity<ClustersModel>) {
    cx.executor().allow_parking();
    let kubeconfig = dir.join("kubeconfig");
    std::fs::write(&kubeconfig, "apiVersion: v1\nkind: Config\n").unwrap();
    let (window, workspace, model) = open_window_over(cx, dir, vec![kubeconfig]);
    let loaded = model.clone();
    cx.wait_for(window, WAIT, move |_, cx| loaded.read(cx).loaded)
        .await;
    (window, workspace, model)
}

/// Opens a running shell on `kind-alpha` in the dock, over a session the test drives.
pub fn open_fake_shell(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
) -> (u64, flint_core::exec::fake::FakeExec) {
    open_fake_shell_in(cx, t, "web")
}

/// [`open_fake_shell`] into pod `pod`.
pub fn open_fake_shell_in(
    cx: &mut TestAppContext,
    t: &TestWorkspace,
    pod: &str,
) -> (u64, flint_core::exec::fake::FakeExec) {
    use crate::ui::dock_model::TabCluster;
    use crate::ui::terminal_model::ShellRequest;
    use crate::ui::terminal_view::TerminalView;
    let request = ShellRequest {
        cluster: "kind-alpha".into(),
        namespace: "demo".into(),
        pod: pod.into(),
        container: Some("app".into()),
    };
    let (session, mut fake) = flint_core::exec::fake::FakeExec::open(
        request.exec_request(flint_core::TerminalSize::default()),
    );
    let server = t.server.clone();
    let id = cx
        .update_window(t.window, |_, window, cx| {
            let dock = t.workspace.read(cx).dock_view();
            dock.update(cx, |dock, cx| {
                dock.open(
                    crate::ui::dock::TabKey::Shell(request.clone()),
                    TabCluster::new("kind-alpha", Some(&server)),
                    |window, cx| {
                        crate::ui::dock::DockTab::Shell(
                            cx.new(|cx| TerminalView::new(session, request, window, cx)),
                        )
                    },
                    window,
                    cx,
                )
            })
        })
        .unwrap();
    fake.set_state(flint_core::ExecState::Running {
        command: vec!["bash".into()],
    });
    fake.pump();
    cx.run_until_parked();
    (id, fake)
}

impl TestWorkspace {
    /// Dispatches the registered action `name` from the focused element, as automation and the
    /// palette do, and lets the UI settle.
    pub fn dispatch(
        &self,
        cx: &mut TestAppContext,
        name: &str,
        args: Value,
    ) -> Result<(), ActionError> {
        let result = cx
            .update_window(self.window, |_, window, cx| {
                registry::dispatch(name, args, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        result
    }

    /// The registered actions available from the focused element.
    pub fn available(&self, cx: &mut TestAppContext) -> Vec<&'static str> {
        cx.update_window(self.window, |_, window, cx| {
            registry::available(window, cx).map(|s| s.name).collect()
        })
        .unwrap()
    }

    /// Selects `cluster` through the registry and waits until the workspace shows it.
    pub async fn activate(&self, cx: &mut TestAppContext, cluster: &'static str) {
        self.dispatch(
            cx,
            "workspace::SelectCluster",
            json!({ "cluster": cluster }),
        )
        .unwrap();
        let workspace = self.workspace.clone();
        cx.wait_for(self.window, WAIT, move |_, cx| {
            workspace.read(cx).shown_cluster() == Some(cluster)
        })
        .await;
    }

    /// Feeds `event` to the model as if the core sent it.
    pub fn feed(&self, cx: &mut TestAppContext, event: Event) {
        // A discovery fed for [`CURRENT_GENERATION`] is for the cluster's current slot.
        let event = match event {
            Event::DiscoveryChanged {
                cluster,
                generation: CURRENT_GENERATION,
                discovery,
                error,
            } => {
                let generation = self.model.read_with(cx, |m, _| {
                    m.clusters
                        .iter()
                        .find(|c| c.info.name == cluster)
                        .map_or(0, |c| c.generation)
                });
                Event::DiscoveryChanged {
                    cluster,
                    generation,
                    discovery,
                    error,
                }
            }
            other => other,
        };
        self.model.update(cx, |model, cx| model.feed(event, cx));
        cx.run_until_parked();
    }

    /// Waits until `cluster`'s connection state satisfies `wanted`.
    pub async fn wait_for_state(
        &self,
        cx: &mut TestAppContext,
        cluster: &'static str,
        wanted: fn(&ConnectionState) -> bool,
    ) {
        let model = self.model.clone();
        cx.wait_for(self.window, WAIT, move |_, cx| {
            model
                .read(cx)
                .clusters
                .iter()
                .any(|c| c.info.name == cluster && wanted(&c.state))
        })
        .await;
    }

    /// How many times `cluster` has started connecting.
    pub fn connection_attempts(&self, cluster: &str) -> usize {
        self.states
            .borrow()
            .iter()
            .filter(|(name, s)| name == cluster && matches!(s, ConnectionState::Connecting))
            .count()
    }

    /// Waits until `cluster` has started connecting `attempts` times.
    pub async fn wait_for_attempts(
        &self,
        cx: &mut TestAppContext,
        cluster: &'static str,
        attempts: usize,
    ) {
        let states = self.states.clone();
        cx.wait_for(self.window, WAIT, move |_, _| {
            states
                .borrow()
                .iter()
                .filter(|(name, s)| name == cluster && matches!(s, ConnectionState::Connecting))
                .count()
                >= attempts
        })
        .await;
    }

    /// Activates `kind-alpha` and makes it connected with `sections` as its navigation, as the core
    /// reports a connect and a discovery. Also after a switch back to `kind-alpha`: its state is
    /// reset first, so only the core's own "connecting" for this activation satisfies the wait,
    /// and that cannot land after the connect fed here.
    pub async fn connect_alpha(&self, cx: &mut TestAppContext, sections: Vec<NavSection>) {
        self.feed(
            cx,
            Event::ConnectionChanged {
                cluster: "kind-alpha".into(),
                state: ConnectionState::Disconnected,
            },
        );
        self.activate(cx, "kind-alpha").await;
        self.wait_for_state(cx, "kind-alpha", |s| {
            matches!(s, ConnectionState::Connecting)
        })
        .await;
        self.feed(
            cx,
            Event::ConnectionChanged {
                cluster: "kind-alpha".into(),
                state: ConnectionState::Connected(ServerVersion {
                    git_version: "v1.33.1".into(),
                    platform: "linux/arm64".into(),
                }),
            },
        );
        self.feed(
            cx,
            Event::DiscoveryChanged {
                cluster: "kind-alpha".into(),
                generation: crate::ui::support_tests::CURRENT_GENERATION,
                discovery: Some(Arc::new(discovery(sections))),
                error: None,
            },
        );
    }

    /// Opens `kind` through the registry and feeds its list `snapshot` by hand, in place of
    /// the silent cluster's watch.
    pub async fn open_kind(
        &self,
        cx: &mut TestAppContext,
        kind: &str,
        snapshot: Arc<KindSnapshot>,
    ) {
        self.dispatch(cx, "workspace::OpenView", json!({ "kind": kind }))
            .unwrap();
        self.feed_list(cx, snapshot).await;
    }

    /// Feeds the open list `snapshot` with its watch stopped, and waits until the list shows
    /// that very snapshot (filtered and sorted).
    pub async fn feed_list(&self, cx: &mut TestAppContext, snapshot: Arc<KindSnapshot>) {
        let list = self.list(cx).expect("a list is open");
        let fed = snapshot.clone();
        list.update(cx, |list, cx| {
            list.stop_watching();
            list.set_snapshot(snapshot, cx);
        });
        cx.wait_for(self.window, WAIT, move |_, cx| {
            Arc::ptr_eq(list.read(cx).shown().0, &fed)
        })
        .await;
        cx.run_until_parked();
    }

    /// Connects `kind-alpha` with the standard navigation and opens Pods through the registry, with
    /// `rows` fed by hand.
    pub async fn open_pods(&self, cx: &mut TestAppContext, rows: &[(&str, &str, i64)]) {
        self.connect_alpha(cx, standard_sections()).await;
        self.open_kind(cx, "pods", pods(rows)).await;
    }

    pub fn list(&self, cx: &mut TestAppContext) -> Option<Entity<ResourceList>> {
        self.workspace.read_with(cx, |ws, _| ws.list_view())
    }

    pub fn detail(&self, cx: &mut TestAppContext) -> Option<Entity<DetailView>> {
        self.workspace.read_with(cx, |ws, _| ws.detail_view())
    }

    /// The name of the list's selected object.
    pub fn selected(&self, cx: &mut TestAppContext) -> Option<String> {
        let list = self.list(cx)?;
        list.read_with(cx, |l, _| l.selected_name())
    }

    /// How many rows the list shows after its filters.
    pub fn row_count(&self, cx: &mut TestAppContext) -> usize {
        self.list(cx)
            .map_or(0, |l| l.read_with(cx, |l, _| l.shown().1))
    }

    /// Focuses the workspace itself, as when nothing inside has focus.
    pub fn focus_workspace(&self, cx: &mut TestAppContext) {
        let focus = self
            .workspace
            .read_with(cx, |ws, _| ws.focus_handle_for_tests());
        cx.update_window(self.window, |_, window, cx| window.focus(&focus, cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// Clicks the element `id`, as the user does.
    pub fn click(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) {
        let id = id.into();
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx)
        })
        .unwrap();
        cx.run_until_parked();
    }

    /// Presses `key` (for example `down` or `cmd-1`) where focus is.
    pub fn press(&self, cx: &mut TestAppContext, key: &str) {
        cx.update_window(self.window, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// Presses the key the registry binds to the action `name`, so tests follow the bindings
    /// across platforms (a `secondary-` binding is Cmd on macOS and Ctrl elsewhere).
    pub fn press_key_of(&self, cx: &mut TestAppContext, name: &str) {
        let spec = registry::find(name).unwrap_or_else(|| panic!("no action {name}"));
        let key = spec
            .keys
            .first()
            .unwrap_or_else(|| panic!("{name} has no key"));
        self.press(cx, key);
    }

    /// Types `text` where focus is.
    pub fn type_text(&self, cx: &mut TestAppContext, text: &str) {
        cx.update_window(self.window, |_, window, cx| window.input(text, cx))
            .unwrap();
        cx.run_until_parked();
    }

    /// The observed element `id` in a freshly drawn frame, if it is there.
    pub fn find(
        &self,
        cx: &mut TestAppContext,
        id: impl Into<ElementId>,
    ) -> Option<ElementSnapshot> {
        let id = id.into();
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id)
        })
        .unwrap()
    }

    /// Waits until the element `id` is drawn.
    pub async fn wait_for_element(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) {
        let id = id.into();
        cx.wait_for(self.window, WAIT, move |window, _| {
            window.try_find(id.clone()).is_some()
        })
        .await;
    }

    /// The label of the element `id` (a button's text).
    pub fn label(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) -> Option<String> {
        self.find(cx, id)?.label().map(str::to_owned)
    }

    /// The description of the element `id` (a button's tooltip).
    pub fn description(&self, cx: &mut TestAppContext, id: impl Into<ElementId>) -> Option<String> {
        self.find(cx, id)?.description().map(str::to_owned)
    }

    /// Waits until the element `id` has the label `label`.
    pub async fn wait_for_label(
        &self,
        cx: &mut TestAppContext,
        id: impl Into<ElementId>,
        label: &str,
    ) {
        let (id, label) = (id.into(), label.to_owned());
        cx.wait_for(self.window, WAIT, move |window, _| {
            window
                .try_find(id.clone())
                .is_some_and(|e| e.label() == Some(label.as_str()))
        })
        .await;
    }

    pub fn has_dialog(&self, cx: &mut TestAppContext) -> bool {
        cx.update_window(self.window, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    }

    /// Whether a confirmation waits for an answer: a dialog other than the Settings modal.
    pub fn has_confirmation(&self, cx: &mut TestAppContext) -> bool {
        cx.update_window(self.window, |_, window, cx| {
            super::confirm::any_open(window, cx)
        })
        .unwrap()
    }

    /// Waits until no confirmation waits for an answer (Settings may stay open under it).
    pub async fn wait_for_no_confirmation(&self, cx: &mut TestAppContext) {
        cx.wait_for(self.window, WAIT, |window, cx| {
            !super::confirm::any_open(window, cx)
        })
        .await;
    }

    /// The ids of the observed elements whose id starts with `prefix`, top to bottom.
    pub fn ids_with_prefix(&self, cx: &mut TestAppContext, prefix: &str) -> Vec<String> {
        use gpui_kit::test::TestQueryExt as _;
        cx.update_window(self.window, |_, window, cx| {
            window.render_frame(cx);
            window
                .elements()
                .into_iter()
                .filter_map(|e| match e.path().last() {
                    Some(gpui_kit::ElementId::Name(name)) if name.starts_with(prefix) => {
                        Some(name.to_string())
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap()
    }

    /// `settings.toml` as written, or empty.
    pub fn settings_toml(&self) -> String {
        std::fs::read_to_string(self.dir.join("settings.toml")).unwrap_or_default()
    }

    /// Waits until `kind-alpha`'s connection attempt has reached the silent server, returning that
    /// connection, or has failed without reaching it. While the connection is held, open and
    /// unanswered, the attempt neither fails nor retries.
    pub async fn wait_for_alpha_attempt(&self, cx: &mut TestAppContext) -> Option<TcpStream> {
        let listener = self.listener.try_clone().unwrap();
        listener.set_nonblocking(true).unwrap();
        let states = self.states.clone();
        let accepted: Rc<RefCell<Option<TcpStream>>> = Rc::default();
        let slot = accepted.clone();
        cx.wait_for(self.window, WAIT, move |_, _| {
            // Read before accepting, so a connection made by an attempt seen as failed is taken.
            let failed = states
                .borrow()
                .iter()
                .any(|(name, s)| name == "kind-alpha" && matches!(s, ConnectionState::Failed(_)));
            match listener.accept() {
                Ok((stream, _)) => {
                    *slot.borrow_mut() = Some(stream);
                    true
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => failed,
                Err(e) => panic!("accept: {e}"),
            }
        })
        .await;
        accepted.take()
    }

    /// Whether anything has connected to the silent server (`kind-alpha`'s) so far.
    pub fn server_was_contacted(&self) -> bool {
        self.listener.set_nonblocking(true).unwrap();
        match self.listener.accept() {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("accept: {e}"),
        }
    }
}

/// Asserts that the window holds a focused text field (`what`) as gpui-component registers it:
/// the strong handle a closed view's field must be released from
/// (`ui::focus::release_unfocused_input`).
pub fn assert_focused_input(cx: &mut TestAppContext, window: AnyWindowHandle, what: &str) {
    let registered = cx
        .update_window(window, |_, window, cx| window.has_focused_input(cx))
        .unwrap();
    assert!(registered, "{what} is the window's focused text field");
}

/// Gives up the test's strong handle to `entity`, keeping a weak one to assert its release.
pub fn weaken<T: 'static>(entity: Entity<T>) -> WeakEntity<T> {
    entity.downgrade()
}

/// A navigation entry for `plural` in `group` (`""`: the core group).
pub fn entry(group: &str, plural: &str, kind: &str, label: &str, namespaced: bool) -> NavEntry {
    NavEntry {
        id: KindId::new(group, plural),
        label: label.into(),
        kind: kind.into(),
        api_version: if group.is_empty() {
            "v1".into()
        } else {
            format!("{group}/v1")
        },
        namespaced,
        listable: true,
        watchable: true,
    }
}

pub fn section(title: &str, kind: SectionKind, entries: Vec<NavEntry>) -> NavSection {
    NavSection {
        title: title.into(),
        kind,
        entries,
    }
}

/// A discovery with exactly these navigation sections, serving their kinds (listable and
/// watchable), so kind names resolve as they do against a real cluster.
pub fn discovery(sections: Vec<NavSection>) -> ClusterDiscovery {
    let kinds = sections
        .iter()
        .flat_map(|s| s.entries.iter())
        .map(|e| {
            let (group, version) = match e.api_version.split_once('/') {
                Some((group, version)) => (group.to_owned(), version.to_owned()),
                None => (String::new(), e.api_version.clone()),
            };
            KindInfo {
                resource: kube::core::ApiResource {
                    group,
                    version,
                    api_version: e.api_version.clone(),
                    kind: e.kind.clone(),
                    plural: e.id.plural().to_owned(),
                },
                namespaced: e.namespaced,
                verbs: vec!["get".into(), "list".into(), "watch".into()],
                singular: e.kind.to_lowercase(),
                short_names: Vec::new(),
                scale: Default::default(),
            }
        })
        .collect();
    ClusterDiscovery {
        kinds,
        navigation: Navigation { sections },
        incomplete_groups: Vec::new(),
    }
}

/// The navigation most view tests use: Workloads (Pods, Deployments), Network (Services),
/// Config (Secrets), Cluster (Nodes, cluster-scoped), and the custom resources of
/// `example.com` (Widgets).
pub fn standard_sections() -> Vec<NavSection> {
    vec![
        section(
            "Workloads",
            SectionKind::BuiltIn,
            vec![
                entry("", "pods", "Pod", "Pods", true),
                entry("apps", "deployments", "Deployment", "Deployments", true),
            ],
        ),
        section(
            "Network",
            SectionKind::BuiltIn,
            vec![entry("", "services", "Service", "Services", true)],
        ),
        section(
            "Config",
            SectionKind::BuiltIn,
            vec![entry("", "secrets", "Secret", "Secrets", true)],
        ),
        section(
            "Cluster",
            SectionKind::BuiltIn,
            vec![entry("", "nodes", "Node", "Nodes", false)],
        ),
        section(
            "example.com",
            SectionKind::ApiGroup,
            vec![entry("example.com", "widgets", "Widget", "Widgets", true)],
        ),
    ]
}

fn column(name: &str, role: ColumnRole, ty: ColumnType, priority: i32) -> Column {
    Column {
        name: name.into(),
        ty,
        format: "".into(),
        description: "".into(),
        priority,
        role,
    }
}

/// A Pods list of (namespace, name, restarts), synced, in snapshot order.
pub fn pods(rows: &[(&str, &str, i64)]) -> Arc<KindSnapshot> {
    objects(KindId::pods(), rows, SyncState::Synced, 1)
}

/// A list of `kind` with (namespace, name, restarts) rows in `state`, in snapshot order. With
/// every namespace `""`, the kind is cluster-scoped.
pub fn objects(
    kind: KindId,
    rows: &[(&str, &str, i64)],
    state: SyncState,
    version: u64,
) -> Arc<KindSnapshot> {
    let namespaced = rows.is_empty() || rows.iter().any(|(ns, _, _)| !ns.is_empty());
    let mut columns = vec![column("Name", ColumnRole::Name, ColumnType::String, 0)];
    if namespaced {
        columns.push(column(
            "Namespace",
            ColumnRole::Namespace,
            ColumnType::String,
            0,
        ));
    }
    columns.push(column(
        "Restarts",
        ColumnRole::Value,
        ColumnType::Integer,
        0,
    ));
    let mut rows: Vec<Arc<Row>> = rows
        .iter()
        .map(|(ns, name, restarts)| {
            let mut cells = vec![Cell::Name];
            if namespaced {
                cells.push(Cell::Namespace);
            }
            cells.push(Cell::Int(*restarts));
            Arc::new(Row {
                uid: format!("uid-{name}").into(),
                name: (*name).into(),
                namespace: (!ns.is_empty()).then(|| (*ns).into()),
                resource_version: version.to_string().into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: cells.into_boxed_slice(),
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Arc::new(KindSnapshot {
        cluster: "kind-alpha".into(),
        kind,
        namespaced,
        columns: columns.into(),
        rows,
        state,
        version,
        ..Default::default()
    })
}

/// Gives `kind-alpha` a navigation with Pods, as if its discovery had arrived.
#[cfg(feature = "automation")]
pub fn serve_pods(cx: &mut TestAppContext, t: &TestWorkspace) {
    use flint_core::{ClusterDiscovery, KindInfo};

    use super::clusters::Discovered;

    // Built from a kind, as real discovery is, so the core resolver (`OpenView`, FLINT_VIEW)
    // finds `pods` and `po` too.
    let pods = KindInfo {
        resource: kube::discovery::ApiResource {
            group: String::new(),
            version: "v1".into(),
            api_version: "v1".into(),
            kind: "Pod".into(),
            plural: "pods".into(),
        },
        namespaced: true,
        verbs: ["get", "list", "watch"].map(String::from).to_vec(),
        singular: "pod".into(),
        short_names: vec!["po".into()],
        scale: Default::default(),
    };
    let discovery = ClusterDiscovery::new(vec![pods]);
    assert!(
        discovery.nav_entry("pods").is_ok(),
        "stub discovery must list Pods in the navigation"
    );
    t.model.update(cx, |model, cx| {
        let generation = model
            .clusters
            .iter()
            .find(|c| c.info.name == "kind-alpha")
            .map_or(0, |c| c.generation);
        model.discovery.insert(
            "kind-alpha".into(),
            Discovered {
                discovery: Some(Arc::new(discovery)),
                error: None,
                generation,
            },
        );
        cx.notify();
    });
    cx.run_until_parked();
}
