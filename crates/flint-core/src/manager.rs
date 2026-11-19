//! `ClusterManager`: the core's single owner of cluster state, running as one task on the core
//! runtime. It handles [`Command`]s, connection results, and kubeconfig and settings reloads, and
//! emits [`Event`]s.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kube::Client;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::cluster::{self, CONNECT_TIMEOUT, ClusterId, ServerVersion};
use crate::handle::{
    ClusterSummary, ClustersSnapshot, Command, ConnectionState, CoreConfig, Event,
};
use crate::kubeconfig::{self, ContextInfo, LoadedKubeconfig};
use crate::settings::{self, Settings};

/// Changes to watched files often come in bursts (editor save, `kind` rewriting the file).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

/// One kubeconfig context and its connection. M1 adds the discovery cache and watch manager here.
pub(crate) struct Cluster {
    info: ContextInfo,
    fingerprint: Option<u64>,
    state: ConnectionState,
    client: Option<Client>,
    /// Id of the latest connection attempt, from the manager-wide counter, so a late result of an
    /// older attempt (even one made before the context was removed and re-added) is ignored.
    attempt: u64,
}

impl Cluster {
    fn new(info: ContextInfo, fingerprint: Option<u64>) -> Self {
        Self {
            info,
            fingerprint,
            state: ConnectionState::Disconnected,
            client: None,
            attempt: 0,
        }
    }

    fn summary(&self) -> ClusterSummary {
        ClusterSummary {
            info: self.info.clone(),
            state: self.state.clone(),
        }
    }
}

enum Internal {
    Connected {
        cluster: ClusterId,
        attempt: u64,
        result: Result<(Client, ServerVersion), String>,
    },
    FilesChanged(Changed),
}

/// Which watched files changed.
#[derive(Debug, Clone, Copy, Default)]
struct Changed {
    kubeconfig: bool,
    settings: bool,
}

pub(crate) struct ClusterManager {
    config: CoreConfig,
    events: async_channel::Sender<Event>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: Option<mpsc::UnboundedReceiver<Internal>>,
    kubeconfig: Arc<LoadedKubeconfig>,
    /// The kubeconfig files for the current settings: the base files plus `kubeconfig_paths`.
    kubeconfig_paths: Vec<PathBuf>,
    settings: Arc<Settings>,
    settings_error: Option<String>,
    /// In kubeconfig order.
    clusters: Vec<Cluster>,
    active: Option<ClusterId>,
    /// Source of connection attempt ids; never reused.
    next_attempt: u64,
    watcher: Option<FileWatcher>,
}

struct FileWatcher {
    _watcher: RecommendedWatcher,
    /// Whether the config directory is watched. It may not exist until settings are first written.
    settings_dir_watched: bool,
}

impl ClusterManager {
    pub(crate) fn new(config: CoreConfig, events: async_channel::Sender<Event>) -> Self {
        let (internal_tx, internal_rx) = mpsc::unbounded_channel();
        let kubeconfig_paths = kubeconfig::paths_for_settings(
            &config.kubeconfig_base,
            &config.settings,
            config.home.as_deref(),
        );
        Self {
            settings: Arc::new(config.settings.clone()),
            settings_error: config.settings_error.clone(),
            config,
            events,
            internal_tx,
            internal_rx: Some(internal_rx),
            kubeconfig: Arc::default(),
            kubeconfig_paths,
            clusters: Vec::new(),
            active: None,
            next_attempt: 1,
            watcher: None,
        }
    }

    pub(crate) async fn run(mut self, commands: async_channel::Receiver<Command>) {
        let mut internal_rx = self.internal_rx.take().expect("run is called once");
        self.emit_settings();
        self.reload_kubeconfig().await;
        self.rebuild_watcher();
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Ok(command) => self.handle_command(command).await,
                    // Every CoreHandle is gone.
                    Err(_) => break,
                },
                Some(msg) = internal_rx.recv() => self.handle_internal(msg).await,
            }
        }
    }

    fn emit(&self, event: Event) {
        // An unbounded channel only fails when the receiver is gone, i.e. the front end quit.
        let _ = self.events.try_send(event);
    }

    fn emit_settings(&self) {
        self.emit(Event::SettingsChanged {
            settings: self.settings.clone(),
            error: self.settings_error.clone(),
        });
    }

    async fn handle_command(&mut self, command: Command) {
        match command {
            Command::Activate(id) => self.activate(id),
            Command::SetAiExposed { cluster, server } => self.set_ai_exposed(cluster, server).await,
            Command::ReloadKubeconfig => self.reload_kubeconfig().await,
        }
    }

    async fn set_ai_exposed(&mut self, cluster: ClusterId, server: Option<String>) {
        if let Some(server) = &server {
            // The user confirmed this server. If the context moved since, do not expose the new
            // one on the strength of that confirmation.
            let current = self
                .clusters
                .iter()
                .find(|c| c.info.name == cluster)
                .and_then(|c| c.info.server.as_deref());
            if current != Some(server.as_str()) {
                self.emit(Event::Error(format!(
                    "\"{cluster}\" now points to a different server; it was not exposed. Review it and try again."
                )));
                return;
            }
        }
        if let Some(error) = &self.settings_error {
            // Rewriting a file that does not parse would fail anyway; say why.
            self.emit(Event::Error(format!(
                "settings.toml must be fixed before exposure can change: {error}"
            )));
            return;
        }
        let dir = self.config.config_dir.clone();
        let result = tokio::task::spawn_blocking(move || {
            settings::set_ai_exposed(&dir, &cluster, server.as_deref())
        })
        .await;
        match result {
            Ok(Ok(settings)) => {
                self.apply_settings(Ok(settings)).await;
                if self
                    .watcher
                    .as_ref()
                    .is_some_and(|w| !w.settings_dir_watched)
                {
                    // The write may have created the config directory.
                    self.rebuild_watcher();
                }
            }
            Ok(Err(e)) => self.emit(Event::Error(e.to_string())),
            Err(e) => self.emit(Event::Error(format!("settings write failed: {e}"))),
        }
    }

    async fn reload_settings(&mut self) {
        let dir = self.config.config_dir.clone();
        match tokio::task::spawn_blocking(move || settings::load_settings(&dir)).await {
            Ok(result) => self.apply_settings(result.map_err(|e| e.to_string())).await,
            Err(e) => self.emit(Event::Error(format!("settings reload failed: {e}"))),
        }
    }

    /// Takes new settings (or the error loading them, which means nothing is exposed), emits them
    /// if they changed, and reloads kubeconfig if the file list changed.
    async fn apply_settings(&mut self, result: Result<Settings, String>) {
        let (settings, error) = match result {
            Ok(s) => (s, None),
            Err(e) => {
                // Expose nothing (as the MCP server does), but keep the other settings, so a
                // typo does not make clusters from extra kubeconfig files disappear.
                tracing::warn!("settings.toml unreadable; nothing is exposed to AI: {e}");
                let kept = Settings {
                    ai_exposed_clusters: Vec::new(),
                    ..(*self.settings).clone()
                };
                (kept, Some(e))
            }
        };
        if *self.settings == settings && self.settings_error == error {
            return;
        }
        self.settings = Arc::new(settings);
        self.settings_error = error;
        self.emit_settings();
        let paths = kubeconfig::paths_for_settings(
            &self.config.kubeconfig_base,
            &self.settings,
            self.config.home.as_deref(),
        );
        if paths != self.kubeconfig_paths {
            self.kubeconfig_paths = paths;
            self.rebuild_watcher();
            self.reload_kubeconfig().await;
        }
    }

    async fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Connected {
                cluster,
                attempt,
                result,
            } => {
                let Some(slot) = self.cluster_mut(&cluster) else {
                    return;
                };
                if slot.attempt != attempt {
                    return;
                }
                match result {
                    Ok((client, version)) => {
                        slot.client = Some(client);
                        slot.state = ConnectionState::Connected(version);
                    }
                    Err(message) => {
                        slot.client = None;
                        slot.state = ConnectionState::Failed(message);
                    }
                }
                let state = slot.state.clone();
                self.emit(Event::ConnectionChanged { cluster, state });
            }
            Internal::FilesChanged(changed) => {
                if changed.settings {
                    // First, since settings can change the kubeconfig file list.
                    self.reload_settings().await;
                }
                if changed.kubeconfig {
                    self.reload_kubeconfig().await;
                }
            }
        }
    }

    fn cluster_mut(&mut self, id: &str) -> Option<&mut Cluster> {
        self.clusters.iter_mut().find(|c| c.info.name == id)
    }

    fn activate(&mut self, id: ClusterId) {
        if self.cluster_mut(&id).is_none() {
            self.emit(Event::Error(format!("cluster \"{id}\" no longer exists")));
            return;
        }
        if self.active.as_deref() != Some(&id) {
            self.active = Some(id.clone());
            self.emit(Event::ActiveChanged(Some(id.clone())));
        }
        self.ensure_connected(&id);
    }

    /// Starts a connection unless one is up or in progress.
    fn ensure_connected(&mut self, id: &str) {
        let kubeconfig = self.kubeconfig.clone();
        let internal = self.internal_tx.clone();
        let attempt = self.next_attempt;
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        if matches!(
            slot.state,
            ConnectionState::Connecting | ConnectionState::Connected(_)
        ) {
            let state = slot.state.clone();
            self.emit(Event::ConnectionChanged {
                cluster: id.to_owned(),
                state,
            });
            return;
        }
        self.next_attempt += 1;
        let Some(slot) = self.cluster_mut(id) else {
            return;
        };
        slot.attempt = attempt;
        slot.state = ConnectionState::Connecting;
        slot.client = None;
        let cluster = id.to_owned();
        self.emit(Event::ConnectionChanged {
            cluster: cluster.clone(),
            state: ConnectionState::Connecting,
        });
        tracing::info!(cluster, "connecting");
        tokio::spawn(async move {
            // Run the attempt as its own task so a panic inside kube or an auth plugin handler
            // becomes a failed connection instead of a cluster stuck in "connecting".
            let name = cluster.clone();
            let attempt_task = tokio::spawn(async move {
                cluster::connect_and_check(&kubeconfig.merged, &name, CONNECT_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())
            });
            let result = attempt_task
                .await
                .unwrap_or_else(|e| Err(format!("connection task failed: {e}")));
            if let Err(e) = &result {
                tracing::warn!(cluster, error = %e, "connection failed");
            }
            let _ = internal.send(Internal::Connected {
                cluster,
                attempt,
                result,
            });
        });
    }

    async fn reload_kubeconfig(&mut self) {
        let paths = self.kubeconfig_paths.clone();
        let loaded = match tokio::task::spawn_blocking(move || kubeconfig::load(&paths)).await {
            Ok(loaded) => Arc::new(loaded),
            Err(e) => {
                self.emit(Event::Error(format!("kubeconfig reload failed: {e}")));
                return;
            }
        };
        tracing::info!(
            contexts = loaded.contexts.len(),
            files = loaded.paths.len(),
            "kubeconfig loaded"
        );
        for (path, err) in &loaded.errors {
            // The full error can quote lines of the file, credentials included; log one line.
            tracing::warn!(
                path = %path.display(),
                error = %kubeconfig::error_summary(err),
                "kubeconfig file skipped"
            );
        }

        let mut old: HashMap<String, Cluster> = self
            .clusters
            .drain(..)
            .map(|c| (c.info.name.clone(), c))
            .collect();
        let mut changed_active = false;
        for info in &loaded.contexts {
            let fingerprint = cluster::context_fingerprint(&loaded.merged, &info.name);
            let slot = match old.remove(&info.name) {
                Some(mut existing) if existing.fingerprint == fingerprint => {
                    existing.info = info.clone();
                    existing
                }
                Some(_) => {
                    // The context's connection details changed: the client is stale. The fresh
                    // slot has attempt 0, which no in-flight attempt uses.
                    if self.active.as_deref() == Some(&info.name) {
                        changed_active = true;
                    }
                    Cluster::new(info.clone(), fingerprint)
                }
                None => Cluster::new(info.clone(), fingerprint),
            };
            self.clusters.push(slot);
        }
        self.kubeconfig = loaded.clone();

        let snapshot = ClustersSnapshot {
            clusters: self.clusters.iter().map(Cluster::summary).collect(),
            errors: loaded.errors.clone(),
            paths: loaded.paths.clone(),
        };
        self.emit(Event::ClustersChanged(Arc::new(snapshot)));

        if let Some(active) = self.active.clone() {
            if old.contains_key(&active) {
                // The active cluster was removed from kubeconfig.
                self.active = None;
                self.emit(Event::ActiveChanged(None));
                self.emit(Event::Error(format!(
                    "\"{active}\" was removed from your kubeconfig files."
                )));
            } else if changed_active {
                self.ensure_connected(&active);
            }
        }
    }

    /// (Re)creates the file watcher for the current kubeconfig files and `settings.toml`, if
    /// watching is enabled. Dropping the old watcher stops it.
    fn rebuild_watcher(&mut self) {
        if !self.config.watch_files {
            return;
        }
        self.watcher = None;
        self.watcher = self.watch_files();
    }

    /// Watches the directories holding the kubeconfig files and `settings.toml`, so files that
    /// are replaced by rename or created later are noticed. Each file is matched by its path and
    /// its resolved path (symlink targets, `/var` versus `/private/var`).
    fn watch_files(&self) -> Option<FileWatcher> {
        // Create the config directory, so a settings.toml written there later (by hand or by
        // another Flint) is noticed. Failing is fine: the app's own writes create it too.
        let _ = std::fs::create_dir_all(&self.config.config_dir);
        let kube_files: HashSet<PathBuf> = self
            .kubeconfig_paths
            .iter()
            .flat_map(|p| watch_names(p))
            .collect();
        let settings_path = self.config.config_dir.join(settings::SETTINGS_FILE);
        let settings_files: HashSet<PathBuf> = watch_names(&settings_path).into_iter().collect();
        let dirs: HashSet<PathBuf> = kube_files
            .iter()
            .chain(&settings_files)
            .filter_map(|f| f.parent().map(Path::to_path_buf))
            .collect();

        let (raw_tx, raw_rx) = mpsc::unbounded_channel::<Changed>();
        let (kube_watched, settings_watched) = (kube_files.clone(), settings_files.clone());
        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                let changed = Changed {
                    kubeconfig: event.paths.iter().any(|p| kube_watched.contains(p)),
                    settings: event.paths.iter().any(|p| settings_watched.contains(p)),
                };
                if changed.kubeconfig || changed.settings {
                    let _ = raw_tx.send(changed);
                }
            }) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot watch kubeconfig and settings files");
                    return None;
                }
            };
        let mut watched_dirs = HashSet::new();
        for dir in &dirs {
            match watcher.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    watched_dirs.insert(dir.clone());
                }
                Err(e) => tracing::debug!(dir = %dir.display(), error = %e, "not watching dir"),
            }
        }
        let settings_dir_watched = settings_files
            .iter()
            .filter_map(|f| f.parent())
            .any(|d| watched_dirs.contains(d));
        tokio::spawn(debounce(raw_rx, self.internal_tx.clone()));
        Some(FileWatcher {
            _watcher: watcher,
            settings_dir_watched,
        })
    }
}

/// The paths a file may be reported under: as given, resolved (if it exists), and with its
/// directory resolved (if only the directory exists yet).
fn watch_names(path: &Path) -> Vec<PathBuf> {
    let mut names = vec![path.to_path_buf()];
    if let Ok(target) = std::fs::canonicalize(path) {
        names.push(target);
    }
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
        && let Ok(dir) = std::fs::canonicalize(dir)
    {
        names.push(dir.join(name));
    }
    names
}

/// Ends when the watcher is dropped (its sender closes) or the manager stops.
async fn debounce(mut raw: mpsc::UnboundedReceiver<Changed>, out: mpsc::UnboundedSender<Internal>) {
    while let Some(first) = raw.recv().await {
        tokio::time::sleep(RELOAD_DEBOUNCE).await;
        let mut changed = first;
        while let Ok(more) = raw.try_recv() {
            changed.kubeconfig |= more.kubeconfig;
            changed.settings |= more.settings;
        }
        if out.send(Internal::FilesChanged(changed)).is_err() {
            break;
        }
    }
}
