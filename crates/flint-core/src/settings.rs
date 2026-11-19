//! `settings.toml` and `state.toml` in Flint's config directory.
//!
//! The directory is `FLINT_CONFIG_DIR` if set, else `~/Library/Application Support/Flint/`.
//! A missing file means defaults. Flint never writes to kubeconfig files.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SETTINGS_FILE: &str = "settings.toml";
pub const STATE_FILE: &str = "state.toml";

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("could not read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("could not write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("could not serialize settings: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("neither FLINT_CONFIG_DIR nor HOME is set")]
    NoConfigDir,
}

/// Which kinds are watched while a cluster is active (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    /// Watch every listable, watchable kind on the active cluster.
    #[default]
    All,
    /// Watch a kind only while a view for it is open.
    OnView,
}

/// One cluster the user exposed to AI agents (ADR-0009): the context name and the API server URL
/// it pointed to when the user exposed it. Both must match for the cluster to count as exposed,
/// so a context that is re-pointed, or has the same name in another `KUBECONFIG`, is not exposed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AiExposure {
    pub context: String,
    pub server: String,
}

/// Whether a context is exposed to AI agents, per [`Settings::ai_exposure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiExposureState {
    Hidden,
    Exposed,
    /// The context name is exposed, but for a different API server URL. It counts as hidden until
    /// the user exposes it again. `exposed_server` is the URL that was exposed.
    ServerChanged {
        exposed_server: String,
    },
}

impl AiExposureState {
    pub fn is_exposed(&self) -> bool {
        matches!(self, AiExposureState::Exposed)
    }
}

/// User settings from `settings.toml`. Unknown keys are ignored so newer files still load.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Clusters the MCP server may see (ADR-0009), as context name plus API server URL. Empty by
    /// default: nothing is exposed.
    pub ai_exposed_clusters: Vec<AiExposure>,
    /// Extra kubeconfig files, loaded after `KUBECONFIG` or `~/.kube/config`. `~` is expanded.
    pub kubeconfig_paths: Vec<PathBuf>,
    /// Overrides the `PATH` resolved from the login shell.
    pub path: Option<String>,
    /// Overrides the `helm` binary found on `PATH`.
    pub helm: Option<PathBuf>,
    /// Global watch mode.
    pub watch_mode: WatchMode,
    /// Per-cluster watch mode overrides, keyed by context name.
    pub cluster_watch_mode: BTreeMap<String, WatchMode>,
}

impl Settings {
    /// The watch mode for a cluster: its override if there is one, else the global mode.
    pub fn watch_mode_for(&self, cluster: &str) -> WatchMode {
        self.cluster_watch_mode
            .get(cluster)
            .copied()
            .unwrap_or(self.watch_mode)
    }

    /// Whether `context`, currently pointing at `server`, is exposed to AI agents. A context with
    /// no server (its cluster entry is missing) is never exposed.
    pub fn ai_exposure(&self, context: &str, server: Option<&str>) -> AiExposureState {
        let mut changed = None;
        for entry in self
            .ai_exposed_clusters
            .iter()
            .filter(|e| e.context == context)
        {
            if server.is_some_and(|s| same_server(s, &entry.server)) {
                return AiExposureState::Exposed;
            }
            changed.get_or_insert_with(|| entry.server.clone());
        }
        match changed {
            Some(exposed_server) => AiExposureState::ServerChanged { exposed_server },
            None => AiExposureState::Hidden,
        }
    }

    /// `kubeconfig_paths` with a leading `~` replaced by `home`.
    pub fn expanded_kubeconfig_paths(&self, home: Option<&Path>) -> Vec<PathBuf> {
        self.kubeconfig_paths
            .iter()
            .map(|p| expand_tilde(p, home))
            .collect()
    }
}

/// UI state from `state.toml`: what the window looked like, so the next start looks the same.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Sidebar order of clusters (context names). Clusters not listed follow in kubeconfig order.
    pub cluster_order: Vec<String>,
    /// Clusters pinned to the top of the sidebar.
    pub pinned_clusters: BTreeSet<String>,
    /// The namespace filter last used per cluster (context name). Missing or empty means all
    /// namespaces.
    pub namespace_filters: BTreeMap<String, Vec<String>>,
    /// The kind last open per cluster (context name), as a kind id (`pods`,
    /// `deployments.apps`). Reopened when the cluster is selected again, after a restart too.
    pub last_views: BTreeMap<String, String>,
    /// Pane widths the user dragged, in points. Missing means the default.
    pub panes: PaneWidths,
}

/// Widths of the window's resizable panes, in points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneWidths {
    pub clusters: Option<u32>,
    pub navigation: Option<u32>,
    pub detail: Option<u32>,
}

/// Flint's config directory, from `FLINT_CONFIG_DIR` or the macOS default.
pub fn config_dir() -> Result<PathBuf, SettingsError> {
    config_dir_from(
        std::env::var_os("FLINT_CONFIG_DIR").map(PathBuf::from),
        std::env::home_dir(),
    )
}

fn config_dir_from(
    override_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, SettingsError> {
    if let Some(dir) = override_dir.filter(|d| !d.as_os_str().is_empty()) {
        return Ok(dir);
    }
    home.map(|h| h.join("Library/Application Support/Flint"))
        .ok_or(SettingsError::NoConfigDir)
}

pub fn load_settings(dir: &Path) -> Result<Settings, SettingsError> {
    load_toml(&dir.join(SETTINGS_FILE))
}

pub fn load_state(dir: &Path) -> Result<State, SettingsError> {
    load_toml(&dir.join(STATE_FILE))
}

pub fn save_state(dir: &Path, state: &State) -> Result<(), SettingsError> {
    write_atomic(&dir.join(STATE_FILE), &toml::to_string_pretty(state)?)
}

/// Server URLs compare equal when they differ only by a trailing slash.
fn same_server(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// Exposes `context` at `server` to AI agents (`Some`), or hides it (`None`), and writes
/// `settings.toml`. Exposing replaces any older entry for the same context, so a cluster that
/// changed is exposed only at its new server. Hiding removes every entry for the context.
///
/// The file is edited as a TOML table, so keys this version does not know survive. Comments and
/// formatting are not preserved. Returns the settings as written.
pub fn set_ai_exposed(
    dir: &Path,
    context: &str,
    server: Option<&str>,
) -> Result<Settings, SettingsError> {
    edit_settings(dir, |table, path| {
        let mut current: Vec<AiExposure> = match table.get("ai_exposed_clusters") {
            Some(value) => value
                .clone()
                .try_into()
                .map_err(|source| SettingsError::Parse {
                    path: path.to_owned(),
                    source,
                })?,
            None => Vec::new(),
        };
        current.retain(|e| e.context != context);
        if let Some(server) = server {
            current.push(AiExposure {
                context: context.to_owned(),
                server: server.to_owned(),
            });
        }
        current.sort();
        table.insert(
            "ai_exposed_clusters".into(),
            toml::Value::try_from(&current)?,
        );
        Ok(())
    })
}

/// Sets the global watch mode (`cluster` is `None`), or a cluster's override (`mode` `None`
/// removes it, so the cluster follows the global mode), and writes `settings.toml`. Returns the
/// settings as written.
pub fn set_watch_mode(
    dir: &Path,
    cluster: Option<&str>,
    mode: Option<WatchMode>,
) -> Result<Settings, SettingsError> {
    let value = |m: WatchMode| toml::Value::try_from(m);
    edit_settings(dir, |table, _| {
        match cluster {
            None => {
                let mode = mode.unwrap_or_default();
                table.insert("watch_mode".into(), value(mode)?);
            }
            Some(cluster) => {
                let overrides = table
                    .entry("cluster_watch_mode")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if !overrides.is_table() {
                    *overrides = toml::Value::Table(toml::Table::new());
                }
                let overrides = overrides.as_table_mut().expect("a table");
                match mode {
                    Some(mode) => {
                        overrides.insert(cluster.to_owned(), value(mode)?);
                    }
                    None => {
                        overrides.remove(cluster);
                    }
                }
            }
        }
        Ok(())
    })
}

/// Reads `settings.toml` as a TOML table, lets `edit` change it, checks the result still parses
/// as [`Settings`], and writes it. Keys this version does not know survive; comments and
/// formatting are not preserved.
fn edit_settings(
    dir: &Path,
    edit: impl FnOnce(&mut toml::Table, &Path) -> Result<(), SettingsError>,
) -> Result<Settings, SettingsError> {
    let path = dir.join(SETTINGS_FILE);
    let mut table: toml::Table = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.clone(),
            source,
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => toml::Table::new(),
        Err(source) => return Err(SettingsError::Read { path, source }),
    };
    edit(&mut table, &path)?;
    let text = toml::to_string_pretty(&table)?;
    let settings: Settings = toml::from_str(&text).map_err(|source| SettingsError::Parse {
        path: path.clone(),
        source,
    })?;
    write_atomic(&path, &text)?;
    Ok(settings)
}

fn load_toml<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.to_owned(),
            source,
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(source) => Err(SettingsError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Writes through a temporary file and a rename, so a reader (the MCP server re-reads settings on
/// every call) never sees a half-written file.
fn write_atomic(path: &Path, text: &str) -> Result<(), SettingsError> {
    let err = |source| SettingsError::Write {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    // Write through a symlink (for example a dotfiles checkout) instead of replacing the link.
    let path = &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(err)?;
    std::fs::rename(&tmp, path).map_err(err)
}

pub(crate) fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-09
    #[test]
    fn defaults_when_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load_settings(dir.path()).unwrap();
        assert_eq!(settings, Settings::default());
        assert!(settings.ai_exposed_clusters.is_empty());
        assert!(settings.kubeconfig_paths.is_empty());
        assert_eq!(settings.path, None);
        assert_eq!(settings.helm, None);
        assert_eq!(settings.watch_mode, WatchMode::All);
        assert_eq!(load_state(dir.path()).unwrap(), State::default());
    }

    #[test]
    fn empty_file_gives_defaults() {
        let settings: Settings = toml::from_str("").unwrap();
        assert_eq!(settings, Settings::default());
    }

    // covers UP-12
    #[test]
    fn parses_all_fields_and_ignores_unknown_keys() {
        let settings: Settings = toml::from_str(
            r#"
            ai_exposed_clusters = [{ context = "kind-dev", server = "https://127.0.0.1:6443" }]
            kubeconfig_paths = ["~/extra/config", "/abs/config"]
            path = "/custom/bin:/usr/bin"
            helm = "/opt/helm"
            watch_mode = "on_view"
            some_future_key = 3

            [cluster_watch_mode]
            big-prod = "on_view"
            small = "all"
            "#,
        )
        .unwrap();
        assert!(
            settings
                .ai_exposure("kind-dev", Some("https://127.0.0.1:6443"))
                .is_exposed()
        );
        assert!(
            !settings
                .ai_exposure("prod", Some("https://127.0.0.1:6443"))
                .is_exposed()
        );
        assert_eq!(settings.path.as_deref(), Some("/custom/bin:/usr/bin"));
        assert_eq!(settings.helm.as_deref(), Some(Path::new("/opt/helm")));
        assert_eq!(settings.watch_mode, WatchMode::OnView);
        assert_eq!(settings.watch_mode_for("small"), WatchMode::All);
        assert_eq!(settings.watch_mode_for("other"), WatchMode::OnView);
        assert_eq!(
            settings.expanded_kubeconfig_paths(Some(Path::new("/home/me"))),
            vec![
                PathBuf::from("/home/me/extra/config"),
                PathBuf::from("/abs/config")
            ]
        );
    }

    // covers UP-08
    #[test]
    fn exposure_needs_name_and_server() {
        let settings: Settings = toml::from_str(
            r#"
            [[ai_exposed_clusters]]
            context = "dev"
            server = "https://dev.example:6443"
            "#,
        )
        .unwrap();
        assert!(
            settings
                .ai_exposure("dev", Some("https://dev.example:6443"))
                .is_exposed()
        );
        assert!(
            settings
                .ai_exposure("dev", Some("https://dev.example:6443/"))
                .is_exposed()
        );
        assert_eq!(
            settings.ai_exposure("dev", Some("https://prod.example:6443")),
            AiExposureState::ServerChanged {
                exposed_server: "https://dev.example:6443".into()
            }
        );
        assert_eq!(
            settings.ai_exposure("dev", None),
            AiExposureState::ServerChanged {
                exposed_server: "https://dev.example:6443".into()
            }
        );
        assert_eq!(
            settings.ai_exposure("other", Some("https://dev.example:6443")),
            AiExposureState::Hidden
        );
    }

    // covers UP-08, UP-12
    #[test]
    fn name_only_exposure_entries_are_rejected() {
        // The pre-release format listed names only. It must not load as "exposed".
        assert!(toml::from_str::<Settings>("ai_exposed_clusters = [\"dev\"]").is_err());
    }

    #[test]
    fn rejects_unknown_watch_mode() {
        assert!(toml::from_str::<Settings>("watch_mode = \"sometimes\"").is_err());
    }

    #[test]
    fn config_dir_prefers_override() {
        assert_eq!(
            config_dir_from(Some("/tmp/x".into()), Some("/home/me".into())).unwrap(),
            PathBuf::from("/tmp/x")
        );
        assert_eq!(
            config_dir_from(None, Some("/home/me".into())).unwrap(),
            PathBuf::from("/home/me/Library/Application Support/Flint")
        );
        assert!(config_dir_from(Some("".into()), None).is_err());
    }

    // covers UP-07
    #[test]
    fn set_ai_exposed_round_trips_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "watch_mode = \"on_view\"\nfuture = { a = 1 }\n",
        )
        .unwrap();
        let s = set_ai_exposed(dir.path(), "a", Some("https://a")).unwrap();
        assert!(s.ai_exposure("a", Some("https://a")).is_exposed());
        set_ai_exposed(dir.path(), "b", Some("https://b")).unwrap();
        // Re-exposing at a new server replaces the old entry.
        let s = set_ai_exposed(dir.path(), "b", Some("https://b2")).unwrap();
        assert_eq!(s.ai_exposed_clusters.len(), 2);
        assert!(s.ai_exposure("b", Some("https://b2")).is_exposed());
        assert!(!s.ai_exposure("b", Some("https://b")).is_exposed());
        let s = set_ai_exposed(dir.path(), "a", None).unwrap();
        assert_eq!(
            s.ai_exposure("a", Some("https://a")),
            AiExposureState::Hidden
        );
        assert!(s.ai_exposure("b", Some("https://b2")).is_exposed());

        let loaded = load_settings(dir.path()).unwrap();
        assert_eq!(loaded, s);
        assert_eq!(loaded.watch_mode, WatchMode::OnView);
        let raw = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(raw.contains("future"));
    }

    #[test]
    fn state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let state = State {
            cluster_order: vec!["b".into(), "a".into()],
            pinned_clusters: ["a".to_owned()].into(),
            last_views: [("kind-dev".to_owned(), "deployments.apps".to_owned())].into(),
            panes: PaneWidths {
                clusters: Some(180),
                navigation: None,
                detail: Some(520),
            },
            ..Default::default()
        };
        save_state(dir.path(), &state).unwrap();
        assert_eq!(load_state(dir.path()).unwrap(), state);
    }

    // covers UP-25
    #[test]
    fn watch_mode_is_written_globally_and_per_cluster() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "future_key = 1\n").unwrap();
        let s = set_watch_mode(dir.path(), None, Some(WatchMode::OnView)).unwrap();
        assert_eq!(s.watch_mode, WatchMode::OnView);
        let s = set_watch_mode(dir.path(), Some("big"), Some(WatchMode::All)).unwrap();
        assert_eq!(s.watch_mode_for("big"), WatchMode::All);
        assert_eq!(s.watch_mode_for("other"), WatchMode::OnView);
        let s = set_watch_mode(dir.path(), Some("big"), None).unwrap();
        assert_eq!(s.watch_mode_for("big"), WatchMode::OnView);
        let loaded = load_settings(dir.path()).unwrap();
        assert_eq!(loaded, s);
        let text = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("future_key"), "unknown keys survive: {text}");
    }

    #[test]
    fn namespace_filters_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();
        state
            .namespace_filters
            .insert("kind-dev".into(), vec!["demo".into(), "kube-system".into()]);
        save_state(dir.path(), &state).unwrap();
        assert_eq!(load_state(dir.path()).unwrap(), state);
    }
}
