//! Helpers shared by view tests that need the whole window.

use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use flint_core::{
    Cell, Column, ColumnRole, ColumnType, CoreConfig, CoreHandle, KindId, KindSnapshot, Row,
    Settings, SyncState,
};
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

use super::clusters::ClustersModel;
use super::registry;
use super::ui_state::UiState;
use super::workspace::Workspace;

pub const WAIT: Duration = Duration::from_secs(10);

/// The main window over two contexts, once both are loaded. `alpha`'s server accepts
/// connections and never answers, so it stays "connecting" and its views show; `beta`'s
/// refuses them. Nothing outside this process is reached.
pub struct TestWorkspace {
    pub window: AnyWindowHandle,
    pub workspace: Entity<Workspace>,
    pub model: Entity<ClustersModel>,
    /// Keeps `alpha`'s silent server listening.
    _server: TcpListener,
}

pub async fn open_workspace(cx: &mut TestAppContext, dir: &Path) -> TestWorkspace {
    cx.executor().allow_parking();
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let kubeconfig = dir.join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        format!(
            "apiVersion: v1
kind: Config
clusters:
- name: silent
  cluster: {{server: \"https://127.0.0.1:{port}\"}}
- name: refused
  cluster: {{server: \"https://127.0.0.1:9\"}}
users:
- name: u
  user: {{token: t}}
contexts:
- name: alpha
  context: {{cluster: silent, user: u}}
- name: beta
  context: {{cluster: refused, user: u}}
"
        ),
    )
    .unwrap();
    let core = CoreHandle::start(CoreConfig {
        config_dir: dir.to_owned(),
        settings: Settings::default(),
        settings_error: None,
        kubeconfig_base: vec![kubeconfig],
        home: None,
        watch_files: false,
    })
    .unwrap();
    cx.update(|cx| {
        gpui_kit::init(cx);
        registry::init(cx);
    });
    let ui_state = UiState::load(dir.to_owned());
    let (window, workspace, model) = cx.update(|cx| {
        let model = cx.new(|cx| ClustersModel::new(core, Settings::default(), None, cx));
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
    });
    let loaded = model.clone();
    cx.wait_for(window, WAIT, move |_, cx| {
        loaded.read(cx).clusters.len() == 2
    })
    .await;
    TestWorkspace {
        window,
        workspace,
        model,
        _server: server,
    }
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
    let columns: Arc<[Column]> = vec![
        column("Name", ColumnRole::Name, ColumnType::String, 0),
        column("Namespace", ColumnRole::Namespace, ColumnType::String, 0),
        column("Restarts", ColumnRole::Value, ColumnType::Integer, 0),
    ]
    .into();
    let mut rows: Vec<Arc<Row>> = rows
        .iter()
        .map(|(ns, name, restarts)| {
            Arc::new(Row {
                uid: format!("uid-{name}").into(),
                name: (*name).into(),
                namespace: Some((*ns).into()),
                resource_version: "1".into(),
                generation: None,
                created: Some(0),
                labels: Box::new([]),
                cells: Box::new([Cell::Name, Cell::Namespace, Cell::Int(*restarts)]),
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Arc::new(KindSnapshot {
        cluster: "alpha".into(),
        kind: KindId::pods(),
        namespaced: true,
        columns,
        rows,
        state: SyncState::Synced,
        version: 1,
        ..Default::default()
    })
}

/// Gives `alpha` a navigation with Pods, as if its discovery had arrived.
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
    };
    let discovery = ClusterDiscovery::new(vec![pods]);
    assert!(
        discovery.nav_entry("pods").is_ok(),
        "stub discovery must list Pods in the navigation"
    );
    t.model.update(cx, |model, cx| {
        model.discovery.insert(
            "alpha".into(),
            Discovered {
                discovery: Some(Arc::new(discovery)),
                error: None,
            },
        );
        cx.notify();
    });
    cx.run_until_parked();
}
