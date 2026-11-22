//! `settings.toml` and `state.toml` in Flint's config directory.
//!
//! The directory is `FLINT_CONFIG_DIR` if set, else the platform's config directory:
//! `~/Library/Application Support/Flint/` on macOS, and `$XDG_CONFIG_HOME/flint/` (by default
//! `~/.config/flint/`) on Linux. `state.toml` lives beside `settings.toml` on every platform, so
//! one directory (and one override) holds all of Flint's files. A missing file means defaults.
//! Flint never writes to kubeconfig files.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::thresholds::{self, ThresholdMetric, ThresholdSettings, Thresholds};

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
    /// The file parses but says something Flint will not guess at.
    #[error("invalid {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
    /// A scope change for a cluster that is no longer exposed at that server.
    #[error("\"{context}\" is no longer exposed to AI agents at that server")]
    NotExposed { context: String },
    /// A scope change made from a scope that has changed on disk since.
    #[error(
        "the AI scope of \"{context}\" changed while it was being edited; open Edit AI Scope again"
    )]
    ScopeChanged { context: String },
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

/// Light or dark, or following the system (`system`, the default). The app applies a change at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follow the system appearance (macOS, or the desktop's on Linux), live.
    #[default]
    System,
    Light,
    Dark,
}

/// A cluster as the user chose it: the context name and the API server URL it pointed to when
/// the user chose it. Both must match for the choice to hold, so a context that is re-pointed,
/// or has the same name in another `KUBECONFIG`, is not covered by it. AI exposure (ADR-0009),
/// trust and session unlocks (ADR-0015) all pin a cluster this way.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClusterPin {
    pub context: String,
    pub server: String,
}

impl ClusterPin {
    pub fn new(context: impl Into<String>, server: impl Into<String>) -> Self {
        Self {
            context: context.into(),
            server: server.into(),
        }
    }

    /// Whether this pin is `context` at `server` (a trailing slash aside).
    pub fn matches(&self, context: &str, server: &str) -> bool {
        self.context == context && same_server(&self.server, server)
    }
}

/// A cluster's colour tag (ADR-0014), from a fixed palette the theme gives a colour in light and
/// dark. Cosmetic: nothing that decides protection, trust, AI exposure or a write reads it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ClusterTag {
    #[default]
    Blue,
    Purple,
    Teal,
    Green,
    Yellow,
    Red,
}

impl ClusterTag {
    /// The palette, in menu order.
    pub const ALL: [ClusterTag; 6] = [
        ClusterTag::Blue,
        ClusterTag::Purple,
        ClusterTag::Teal,
        ClusterTag::Green,
        ClusterTag::Yellow,
        ClusterTag::Red,
    ];

    /// Its name on screen and for assistive technology: colour is never the only signal.
    pub fn label(self) -> &'static str {
        match self {
            ClusterTag::Blue => "Blue",
            ClusterTag::Purple => "Purple",
            ClusterTag::Teal => "Teal",
            ClusterTag::Green => "Green",
            ClusterTag::Yellow => "Yellow",
            ClusterTag::Red => "Red",
        }
    }
}

/// A colour tag, for a context at one API server, compared as a [`ClusterPin`] is: a context
/// re-pointed elsewhere does not show it, and shows it again back at this server.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TaggedCluster {
    pub context: String,
    pub server: String,
    pub tag: ClusterTag,
}

/// One cluster the user exposed to AI agents (ADR-0009), pinned to its server like a
/// [`ClusterPin`], and what agents may read of it (ADR-0013). An entry written before scopes
/// existed has neither list, and exposes the whole cluster as it did.
///
/// Unknown keys are refused (`namespace` for `namespaces`): a misspelt scope would otherwise be
/// ignored and expose the whole cluster, so the file is treated as unreadable instead.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiExposure {
    pub context: String,
    pub server: String,
    /// The namespaces agents may read. Empty: every namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    /// The kinds agents may read, as kind ids (`pods`, `deployments.apps`). Empty: every kind
    /// (but with a namespace scope, cluster-scoped kinds only when named here).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
}

impl AiExposure {
    /// The whole cluster exposed at `server`.
    pub fn new(context: impl Into<String>, server: impl Into<String>) -> Self {
        Self {
            context: context.into(),
            server: server.into(),
            ..Self::default()
        }
    }

    /// What agents may read of the cluster.
    pub fn scope(&self) -> AiScope {
        AiScope::new(self.namespaces.clone(), self.kinds.clone())
    }

    fn pin(&self) -> ClusterPin {
        ClusterPin::new(&self.context, &self.server)
    }
}

/// What agents may read of an exposed cluster (ADR-0013): namespaces and kinds. Empty lists
/// mean everything. With a namespace scope, cluster-scoped kinds are readable only when the
/// kinds scope names them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AiScope {
    /// Sorted, without repeats or empty names.
    pub namespaces: Vec<String>,
    /// Kind ids (`pods`, `deployments.apps`), lowercase, sorted, without repeats.
    pub kinds: Vec<String>,
}

impl AiScope {
    /// A scope of `namespaces` and `kinds`, cleaned up: names trimmed, kinds lowercased,
    /// empty ones and repeats dropped, both sorted.
    pub fn new(namespaces: Vec<String>, kinds: Vec<String>) -> Self {
        let clean = |names: Vec<String>, lower: bool| {
            let mut names: Vec<String> = names
                .into_iter()
                .map(|n| {
                    let n = n.trim();
                    if lower {
                        n.to_lowercase()
                    } else {
                        n.to_owned()
                    }
                })
                .filter(|n| !n.is_empty())
                .collect();
            names.sort();
            names.dedup();
            names
        };
        Self {
            namespaces: clean(namespaces, false),
            kinds: clean(kinds, true),
        }
    }

    /// Everything: no namespace or kind limit.
    pub fn is_everything(&self) -> bool {
        self.namespaces.is_empty() && self.kinds.is_empty()
    }

    /// Whether agents may read objects in `namespace`.
    pub fn allows_namespace(&self, namespace: &str) -> bool {
        self.namespaces.is_empty() || self.namespaces.iter().any(|n| n == namespace)
    }

    /// Whether agents may read the kind with id `kind` (`pods`, `deployments.apps`), which is
    /// `namespaced` or cluster-scoped. A cluster-scoped kind under a namespace scope must be
    /// named in the kinds scope.
    pub fn allows_kind(&self, kind: &str, namespaced: bool) -> bool {
        let named = self.kinds.iter().any(|k| k.eq_ignore_ascii_case(kind));
        if !namespaced && !self.namespaces.is_empty() {
            return named;
        }
        self.kinds.is_empty() || named
    }
}

/// One cluster the user trusts (ADR-0015): unlocked across restarts, pinned to its server.
pub type TrustedCluster = ClusterPin;

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

/// Whether a context is trusted, per [`Settings::trust`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    NotTrusted,
    Trusted,
    /// The context name is trusted, but for a different API server URL. It counts as not
    /// trusted (so the cluster is protected) until the user trusts it again. `trusted_server` is
    /// the URL that was trusted.
    ServerChanged {
        trusted_server: String,
    },
}

impl TrustState {
    pub fn is_trusted(&self) -> bool {
        matches!(self, TrustState::Trusted)
    }
}

/// Where `context`, now at `server`, stands in a list of pins: `None` when the list does not
/// name it, `Some(Ok(()))` when a pin matches, `Some(Err(pinned))` when it names the context
/// only at another server (`pinned`). A context with no server never matches.
fn pin_state<'a>(
    pins: impl IntoIterator<Item = std::borrow::Cow<'a, ClusterPin>>,
    context: &str,
    server: Option<&str>,
) -> Option<Result<(), String>> {
    let mut changed = None;
    for pin in pins.into_iter().filter(|p| p.context == context) {
        if server.is_some_and(|s| pin.matches(context, s)) {
            return Some(Ok(()));
        }
        changed.get_or_insert_with(|| pin.server.clone());
    }
    changed.map(Err)
}

/// Lines a log session keeps by default ([`Settings::log_buffer_lines`]).
pub const DEFAULT_LOG_BUFFER_LINES: u32 = 100_000;
/// The range [`Settings::log_buffer_limit`] keeps the setting in, so a typo can neither make the
/// buffer useless nor let a chatty pod use gigabytes.
pub const LOG_BUFFER_LINES_RANGE: std::ops::RangeInclusive<u32> = 1_000..=1_000_000;

/// MiB of log lines a session keeps by default ([`Settings::log_buffer_mib`]).
pub const DEFAULT_LOG_BUFFER_MIB: u32 = 64;
/// The range [`Settings::log_buffer_bytes`] keeps the setting in, in MiB.
pub const LOG_BUFFER_MIB_RANGE: std::ops::RangeInclusive<u32> = 8..=1024;

/// MiB all log sessions together keep by default ([`Settings::log_total_mib`]).
pub const DEFAULT_LOG_TOTAL_MIB: u32 = 512;
/// The range [`Settings::log_total_bytes`] keeps the setting in, in MiB.
pub const LOG_TOTAL_MIB_RANGE: std::ops::RangeInclusive<u32> = 64..=16_384;

/// User settings from `settings.toml`. Unknown keys are ignored so newer files still load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Flint Light, Flint Dark, or whichever the system uses.
    pub appearance: Appearance,
    /// Clusters the MCP server may see (ADR-0009), as context name plus API server URL. Empty by
    /// default: nothing is exposed.
    pub ai_exposed_clusters: Vec<AiExposure>,
    /// Clusters Flint may change without a session unlock (ADR-0015), as context name plus API
    /// server URL. Empty by default: every cluster but a known local one is protected.
    pub trusted_clusters: Vec<TrustedCluster>,
    /// Protect known local clusters too: contexts named as kind, minikube, Docker Desktop and
    /// OrbStack name them, with an API server on this machine. Off by default, so they start
    /// unlocked. Any other context is protected either way.
    pub protect_local_clusters: bool,
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
    /// Custom names shown for clusters, keyed by context name. Display only: kubeconfig is never
    /// written, and MCP, `FLINT_CLUSTER`, exposure and trust keep the context name. Read them
    /// through [`crate::display::ClusterNames`].
    pub cluster_names: BTreeMap<String, String>,
    /// Colour tags, per context and server. Read them through [`Settings::cluster_tag`]. An entry
    /// this version cannot read counts as no tag, and stays in the file.
    #[serde(
        deserialize_with = "readable_cluster_tags",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub cluster_tags: Vec<TaggedCluster>,
    /// Dashboard warning thresholds (ADR-0017): global values and overrides per context and
    /// server. Read them through [`Settings::thresholds_for`]. A value this version cannot use
    /// falls back on its own and is logged when the file loads.
    #[serde(skip_serializing_if = "ThresholdSettings::is_empty")]
    pub thresholds: ThresholdSettings,
    /// Lines each log session keeps; older lines are dropped. Applies to sessions opened after
    /// a change. Read it through [`Settings::log_buffer_limit`].
    pub log_buffer_lines: u32,
    /// MiB of memory each log session's lines may take; the oldest lines are dropped when either
    /// this or `log_buffer_lines` is exceeded. Applies to sessions opened after a change. Read it
    /// through [`Settings::log_buffer_bytes`].
    pub log_buffer_mib: u32,
    /// MiB all log sessions together may keep. Over it, the tabs viewed least recently drop
    /// their oldest lines first (never the tab shown, beyond its own `log_buffer_mib`), and say
    /// so. Applies at once. Read it through [`Settings::log_total_bytes`].
    pub log_total_mib: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            appearance: Appearance::default(),
            ai_exposed_clusters: Vec::new(),
            trusted_clusters: Vec::new(),
            protect_local_clusters: false,
            kubeconfig_paths: Vec::new(),
            path: None,
            helm: None,
            watch_mode: WatchMode::default(),
            cluster_watch_mode: BTreeMap::new(),
            cluster_names: BTreeMap::new(),
            cluster_tags: Vec::new(),
            thresholds: ThresholdSettings::default(),
            log_buffer_lines: DEFAULT_LOG_BUFFER_LINES,
            log_buffer_mib: DEFAULT_LOG_BUFFER_MIB,
            log_total_mib: DEFAULT_LOG_TOTAL_MIB,
        }
    }
}

impl Settings {
    /// [`Settings::log_buffer_lines`], kept within [`LOG_BUFFER_LINES_RANGE`].
    pub fn log_buffer_limit(&self) -> usize {
        self.log_buffer_lines.clamp(
            *LOG_BUFFER_LINES_RANGE.start(),
            *LOG_BUFFER_LINES_RANGE.end(),
        ) as usize
    }

    /// [`Settings::log_buffer_mib`] in bytes, kept within [`LOG_BUFFER_MIB_RANGE`].
    pub fn log_buffer_bytes(&self) -> usize {
        let mib = self
            .log_buffer_mib
            .clamp(*LOG_BUFFER_MIB_RANGE.start(), *LOG_BUFFER_MIB_RANGE.end());
        mib as usize * 1024 * 1024
    }

    /// [`Settings::log_total_mib`] in bytes, kept within [`LOG_TOTAL_MIB_RANGE`] and never
    /// below one session's own cap ([`Settings::log_buffer_bytes`]), so the tab shown never
    /// wipes every other one.
    pub fn log_total_bytes(&self) -> usize {
        let mib = self
            .log_total_mib
            .clamp(*LOG_TOTAL_MIB_RANGE.start(), *LOG_TOTAL_MIB_RANGE.end());
        (mib as usize * 1024 * 1024).max(self.log_buffer_bytes())
    }

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
        let pins = self
            .ai_exposed_clusters
            .iter()
            .map(|e| std::borrow::Cow::Owned(e.pin()));
        match pin_state(pins, context, server) {
            Some(Ok(())) => AiExposureState::Exposed,
            Some(Err(exposed_server)) => AiExposureState::ServerChanged { exposed_server },
            None => AiExposureState::Hidden,
        }
    }

    /// The scope saved for `context` at whatever server it was exposed at: what a re-pointed
    /// context's entry still says, to start from when exposing it again.
    pub fn ai_scope_saved(&self, context: &str) -> Option<AiScope> {
        self.ai_exposed_clusters
            .iter()
            .find(|e| e.context == context)
            .map(AiExposure::scope)
    }

    /// What agents may read of `context`, if it is exposed at `server` (ADR-0013). `None` when it
    /// is not exposed there.
    pub fn ai_scope(&self, context: &str, server: Option<&str>) -> Option<AiScope> {
        let server = server?;
        self.ai_exposed_clusters
            .iter()
            .find(|e| e.pin().matches(context, server))
            .map(AiExposure::scope)
    }

    /// Whether `context`, currently pointing at `server`, is trusted (ADR-0015). A context with
    /// no server (its cluster entry is missing) is never trusted.
    pub fn trust(&self, context: &str, server: Option<&str>) -> TrustState {
        let pins = self.trusted_clusters.iter().map(std::borrow::Cow::Borrowed);
        match pin_state(pins, context, server) {
            Some(Ok(())) => TrustState::Trusted,
            Some(Err(trusted_server)) => TrustState::ServerChanged { trusted_server },
            None => TrustState::NotTrusted,
        }
    }

    /// The colour tag of `context` at `server`. A context with no server has none.
    pub fn cluster_tag(&self, context: &str, server: Option<&str>) -> Option<ClusterTag> {
        let server = server?;
        self.cluster_tags
            .iter()
            .find(|e| e.context == context && same_server(&e.server, server))
            .map(|e| e.tag)
    }

    /// The dashboard thresholds in force for `context` at `server`. A context with no server gets
    /// the global values.
    pub fn thresholds_for(&self, context: &str, server: Option<&str>) -> Thresholds {
        self.thresholds.for_cluster(context, server)
    }

    /// What applies while `settings.toml` cannot be read, keeping the harmless parts of `self`
    /// (the settings in force before, or the defaults): nothing is exposed to AI agents or
    /// trusted, and known local clusters are protected too, since the file may have said
    /// `protect_local_clusters = true`. Every choice that opens something up is dropped until
    /// the file parses again.
    pub fn unreadable_fallback(self) -> Settings {
        Settings {
            ai_exposed_clusters: Vec::new(),
            trusted_clusters: Vec::new(),
            protect_local_clusters: true,
            ..self
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

/// The entries of `cluster_tags` this version can read. A tag is cosmetic, so an unknown colour
/// or a malformed entry is no tag, and never makes the settings file unreadable.
fn readable_cluster_tags<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TaggedCluster>, D::Error> {
    let toml::Value::Array(entries) = toml::Value::deserialize(deserializer)? else {
        return Ok(Vec::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| entry.try_into().ok())
        .collect())
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
    /// Pane sizes the user dragged, in points. Missing means the default.
    pub panes: PaneWidths,
    /// The sort chosen per cluster (context name) and kind id, by column key
    /// ([`crate::Column::key`]). Missing means the kind's default order.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub list_sorts: BTreeMap<String, BTreeMap<String, ListSort>>,
}

/// A list's sort as `state.toml` keeps it: the column's key ([`crate::Column::key`]), not its
/// header, and the direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSort {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
}

/// Sizes of the window's resizable panes, in points: the widths of the side panes and the
/// height of the bottom dock (`[panes]` in `state.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneWidths {
    pub clusters: Option<u32>,
    pub navigation: Option<u32>,
    pub detail: Option<u32>,
    /// The bottom dock's height (log and shell tabs).
    pub dock: Option<u32>,
}

/// Where an OS keeps per-user configuration, for [`config_dir_from`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigLayout {
    /// `~/Library/Application Support/Flint`.
    MacOs,
    /// The XDG Base Directory layout: `$XDG_CONFIG_HOME/flint`, else `~/.config/flint`.
    Xdg,
}

impl ConfigLayout {
    const HOST: ConfigLayout = if cfg!(target_os = "macos") {
        ConfigLayout::MacOs
    } else {
        ConfigLayout::Xdg
    };
}

/// Flint's config directory: `FLINT_CONFIG_DIR`, else the platform default (see the module docs).
pub fn config_dir() -> Result<PathBuf, SettingsError> {
    config_dir_from(
        ConfigLayout::HOST,
        std::env::var_os("FLINT_CONFIG_DIR").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::home_dir(),
    )
}

/// The directory Flint uses on this OS when `FLINT_CONFIG_DIR` is not set, for the given
/// `XDG_CONFIG_HOME` (read on Linux only) and home directory.
pub fn default_config_dir(
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, SettingsError> {
    config_dir_from(ConfigLayout::HOST, None, xdg_config_home, home)
}

/// The config directory: `override_dir` if set and not empty, else `layout`'s default.
fn config_dir_from(
    layout: ConfigLayout,
    override_dir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Result<PathBuf, SettingsError> {
    if let Some(dir) = override_dir.filter(|d| !d.as_os_str().is_empty()) {
        return Ok(dir);
    }
    match layout {
        ConfigLayout::MacOs => home.map(|h| h.join("Library/Application Support/Flint")),
        // The XDG spec says a relative `XDG_CONFIG_HOME` is invalid and must be ignored.
        ConfigLayout::Xdg => match xdg_config_home.filter(|d| d.is_absolute()) {
            Some(dir) => Some(dir.join("flint")),
            None => home.map(|h| h.join(".config/flint")),
        },
    }
    .ok_or(SettingsError::NoConfigDir)
}

pub fn load_settings(dir: &Path) -> Result<Settings, SettingsError> {
    let path = dir.join(SETTINGS_FILE);
    let settings = load_toml(&path)?;
    validate(&settings, &path)?;
    // Unknown keys are ignored, so a newer file still loads; a misspelling of a known key
    // (`protect_local_cluster`) would then fail open without a word. Say so in the log.
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(table) = toml::from_str::<toml::Table>(&text)
    {
        for (unknown, known) in misspelled_keys(&table) {
            tracing::warn!(
                "settings.toml: unknown key `{unknown}` is ignored; did you mean `{known}`?"
            );
        }
        if let Some(section) = table.get(thresholds::SECTION_KEY) {
            for issue in ThresholdSettings::from_toml(section).1 {
                tracing::warn!("settings.toml: {issue}");
            }
        }
    }
    Ok(settings)
}

/// Refuses what parses but cannot be taken at its word: a scope list (`namespaces`, `kinds`)
/// that names only blank entries. It was clearly meant to limit the cluster, and read as empty
/// it would expose everything, so the whole file is treated as unreadable instead.
fn validate(settings: &Settings, path: &Path) -> Result<(), SettingsError> {
    // Two entries for one context and server: the first one found would decide, and an
    // unscoped copy could undo a scope.
    let entries = &settings.ai_exposed_clusters;
    let duplicate = entries.iter().enumerate().find_map(|(i, e)| {
        entries[..i]
            .iter()
            .any(|other| other.pin().matches(&e.context, &e.server))
            .then_some(e)
    });
    if let Some(e) = duplicate {
        return Err(SettingsError::Invalid {
            path: path.to_owned(),
            reason: format!(
                "\"{}\" is listed twice in ai_exposed_clusters for the same server; keep one entry",
                e.context
            ),
        });
    }
    let blank = |names: &[String]| !names.is_empty() && names.iter().all(|n| n.trim().is_empty());
    match settings
        .ai_exposed_clusters
        .iter()
        .find(|e| blank(&e.namespaces) || blank(&e.kinds))
    {
        Some(entry) => Err(SettingsError::Invalid {
            path: path.to_owned(),
            reason: format!(
                "the AI scope of \"{}\" lists only blank names; remove the list to expose \
                 everything, or name what agents may read",
                entry.context
            ),
        }),
        None => Ok(()),
    }
}

/// The top-level keys of [`Settings`].
const SETTINGS_KEYS: &[&str] = &[
    "appearance",
    "ai_exposed_clusters",
    "trusted_clusters",
    "protect_local_clusters",
    "kubeconfig_paths",
    "path",
    "helm",
    "watch_mode",
    "cluster_watch_mode",
    "cluster_names",
    "cluster_tags",
    thresholds::SECTION_KEY,
    "log_buffer_lines",
    "log_buffer_mib",
    "log_total_mib",
];

/// The keys of `table` that are not settings but look like a misspelt one (at most two edits
/// away, or one with an `s` added or dropped), each with the key it resembles.
fn misspelled_keys(table: &toml::Table) -> Vec<(String, &'static str)> {
    table
        .keys()
        .filter(|k| !SETTINGS_KEYS.contains(&k.as_str()))
        .filter_map(|k| {
            SETTINGS_KEYS
                .iter()
                .find(|known| known.len() > 4 && edit_distance(k, known) <= 2)
                .map(|known| (k.clone(), *known))
        })
        .collect()
}

/// Levenshtein distance, by characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// [`load_settings`], or, when the file cannot be read, the defaults made safe with
/// [`Settings::unreadable_fallback`] and why it could not be read. A missing file is the
/// defaults, with no error.
pub fn load_settings_or_fallback(dir: &Path) -> (Settings, Option<String>) {
    match load_settings(dir) {
        Ok(s) => (s, None),
        Err(e) => (
            Settings::default().unreadable_fallback(),
            Some(e.safe_reason()),
        ),
    }
}

impl SettingsError {
    /// One line on what is wrong, safe to log or show to an agent: it never quotes a value from
    /// the file. A parse error names the unknown field, if any, and the line and column, ahead
    /// of the path, so cutting a long path keeps them.
    pub fn safe_reason(&self) -> String {
        let SettingsError::Parse { path, source } = self else {
            return crate::redact::safe_error_summary(&self.to_string());
        };
        let what = unknown_field(source.message()).map_or_else(
            || "invalid TOML".to_owned(),
            |f| format!("unknown field {f}"),
        );
        let text = source.to_string();
        let at = text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("TOML parse error at "))
            .filter(|at| {
                at.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b" ,".contains(&b))
            });
        let reason = match at {
            Some(at) => format!("{what} at {at} in {}", path.display()),
            None => format!("{what} in {}", path.display()),
        };
        crate::redact::safe_error_summary(&reason)
    }
}

/// The key in serde's ``unknown field `key`, expected ...``, if it is a plain name that does not
/// look like a credential. A key is the one thing such a message quotes that is not a value.
fn unknown_field(message: &str) -> Option<&str> {
    let (field, rest) = message.strip_prefix("unknown field `")?.split_once('`')?;
    // A quoted key can hold a backtick: then this is not the whole key.
    if !(rest.is_empty() || rest.starts_with(',')) {
        return None;
    }
    let plain = (1..=64).contains(&field.len())
        && field
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    (plain && crate::redact::redact_credentials(field).count == 0).then_some(field)
}

pub fn load_state(dir: &Path) -> Result<State, SettingsError> {
    load_toml(&dir.join(STATE_FILE))
}

pub fn save_state(dir: &Path, state: &State) -> Result<(), SettingsError> {
    write_atomic(&dir.join(STATE_FILE), &toml::to_string_pretty(state)?)
}

/// Server URLs compare equal when they differ only by a trailing slash.
fn same_server(a: &str, b: &str) -> bool {
    server_identity(a) == server_identity(b)
}

/// The endpoint a cluster pin compares: the API server URL as the kubeconfig has it, without a
/// trailing slash. It may hold credentials, so it is for comparing (and keying), never showing.
pub fn server_identity(server: &str) -> &str {
    server.trim_end_matches('/')
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
    scope: &AiScope,
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
                namespaces: scope.namespaces.clone(),
                kinds: scope.kinds.clone(),
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

/// Changes what agents may read of `context`, exposed at `server`, from `from` (the scope the
/// user started editing) to `scope`, and writes `settings.toml`. Unlike [`set_ai_exposed`], it
/// never exposes anything: if the file, as read for this write, no longer exposes the context
/// at that server (hidden or re-pointed meanwhile, by hand or by another Flint), nothing is
/// written and [`SettingsError::NotExposed`] says so; if its scope there is no longer `from`,
/// [`SettingsError::ScopeChanged`].
pub fn set_ai_scope(
    dir: &Path,
    context: &str,
    server: &str,
    from: &AiScope,
    scope: &AiScope,
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
        let Some(entry) = current
            .iter_mut()
            .find(|e| e.pin().matches(context, server))
        else {
            return Err(SettingsError::NotExposed {
                context: context.to_owned(),
            });
        };
        // Edited from `from`: a change on disk since (by hand, or another Flint) would be
        // overwritten unseen.
        if entry.scope() != *from {
            return Err(SettingsError::ScopeChanged {
                context: context.to_owned(),
            });
        }
        entry.namespaces = scope.namespaces.clone();
        entry.kinds = scope.kinds.clone();
        table.insert(
            "ai_exposed_clusters".into(),
            toml::Value::try_from(&current)?,
        );
        Ok(())
    })
}

/// Trusts `context` at `server` (`Some`), or stops trusting it (`None`), and writes
/// `settings.toml`, as [`set_ai_exposed`] does for exposure: trusting replaces any older entry
/// for the same context, and untrusting removes every entry for it. Returns the settings as
/// written.
pub fn set_trusted(
    dir: &Path,
    context: &str,
    server: Option<&str>,
) -> Result<Settings, SettingsError> {
    set_pin(dir, "trusted_clusters", context, server)
}

/// Pins `context` at `server` in the list of pins under `key` (`Some`, replacing the context's
/// older pins), or removes the context's pins (`None`), and writes `settings.toml`.
fn set_pin(
    dir: &Path,
    key: &str,
    context: &str,
    server: Option<&str>,
) -> Result<Settings, SettingsError> {
    edit_settings(dir, |table, path| {
        let mut current: Vec<ClusterPin> = match table.get(key) {
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
            current.push(ClusterPin::new(context, server));
        }
        current.sort();
        table.insert(key.into(), toml::Value::try_from(&current)?);
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

/// Sets the custom name of `context` (`None` removes it), and writes `settings.toml`. Returns
/// the settings as written. A name [`crate::display::clean_custom_name`] refuses is an error, and
/// so is one with the same [`crate::display::cluster_name_key`] as another cluster's, unless
/// `context` already holds that key: it may keep or re-case its own name even in a file where
/// that name already collides.
pub fn set_cluster_name(
    dir: &Path,
    context: &str,
    name: Option<&str>,
) -> Result<Settings, SettingsError> {
    edit_settings(dir, |table, path| {
        let cleaned = match name {
            Some(name) => Some(crate::display::clean_custom_name(name).ok_or_else(|| {
                SettingsError::Invalid {
                    path: path.to_owned(),
                    reason: format!(
                        "a cluster name has 1 to {} letters, digits or any of {}",
                        crate::display::MAX_CUSTOM_NAME_CHARS,
                        crate::display::CUSTOM_NAME_PUNCTUATION
                    ),
                }
            })?),
            None => None,
        };
        let names = table
            .entry("cluster_names")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !names.is_table() {
            *names = toml::Value::Table(toml::Table::new());
        }
        let entries = names.as_table_mut().expect("a table");
        // Against the file as it is now: another Flint may have taken the name since it was
        // checked, and two clusters with one name would both lose it.
        if let Some(name) = &cleaned {
            let key = crate::display::cluster_name_key(name);
            let holds_key = |value: &toml::Value| {
                value
                    .as_str()
                    .and_then(crate::display::saved_name_key)
                    .is_some_and(|saved| saved == key)
            };
            let its_own = entries.get(context).is_some_and(holds_key);
            let taken = entries
                .iter()
                .any(|(other, value)| other != context && holds_key(value));
            if taken && !its_own {
                return Err(SettingsError::Invalid {
                    path: path.to_owned(),
                    reason: format!("another cluster already goes by {name:?}"),
                });
            }
        }
        match cleaned {
            Some(name) => {
                entries.insert(context.to_owned(), toml::Value::String(name));
            }
            None => {
                entries.remove(context);
            }
        }
        if entries.is_empty() {
            table.remove("cluster_names");
        }
        Ok(())
    })
}

/// Sets the colour tag of `context` at `server` (`None` removes it), and writes `settings.toml`.
/// Its tags at other servers stay, for when the context points back, and so do entries this
/// version cannot read. Removing a tag that has no entry writes nothing, so the file keeps its
/// comments (or stays absent). Returns the settings as written.
pub fn set_cluster_tag(
    dir: &Path,
    context: &str,
    server: &str,
    tag: Option<ClusterTag>,
) -> Result<Settings, SettingsError> {
    let is_this = |entry: &toml::Value| {
        let field = |key| entry.get(key).and_then(toml::Value::as_str);
        field("context") == Some(context) && field("server").is_some_and(|s| same_server(s, server))
    };
    if tag.is_none() {
        let table: toml::Table = load_toml(&dir.join(SETTINGS_FILE))?;
        let tagged = table
            .get("cluster_tags")
            .and_then(toml::Value::as_array)
            .is_some_and(|entries| entries.iter().any(is_this));
        if !tagged {
            return load_settings(dir);
        }
    }
    edit_settings(dir, |table, _| {
        let mut entries = match table.remove("cluster_tags") {
            Some(toml::Value::Array(entries)) => entries,
            _ => Vec::new(),
        };
        entries.retain(|entry| !is_this(entry));
        if let Some(tag) = tag {
            entries.push(toml::Value::try_from(TaggedCluster {
                context: context.to_owned(),
                server: server.to_owned(),
                tag,
            })?);
        }
        if !entries.is_empty() {
            table.insert("cluster_tags".into(), toml::Value::Array(entries));
        }
        Ok(())
    })
}

/// Sets one dashboard threshold, globally (`cluster` is `None`) or for a context at one server,
/// and writes `settings.toml`. `value` `None` clears it, so it follows the next level down;
/// clearing what is not set writes nothing. A value out of range, or a `thresholds` section of
/// the wrong shape, is an error and leaves the file as it is. Returns the settings as written.
pub fn set_threshold(
    dir: &Path,
    cluster: Option<&ClusterPin>,
    metric: ThresholdMetric,
    value: Option<u32>,
) -> Result<Settings, SettingsError> {
    let path = dir.join(SETTINGS_FILE);
    let invalid = |path: &Path, error: thresholds::ThresholdWriteError| SettingsError::Invalid {
        path: path.to_owned(),
        reason: error.to_string(),
    };
    if value.is_none() {
        let mut table: toml::Table = load_toml(&path)?;
        let changed = thresholds::write_value(&mut table, cluster, metric, None)
            .map_err(|e| invalid(&path, e))?;
        if !changed {
            return load_settings(dir);
        }
    }
    edit_settings(dir, |table, path| {
        thresholds::write_value(table, cluster, metric, value)
            .map(drop)
            .map_err(|e| invalid(path, e))
    })
}

/// Sets the appearance and writes `settings.toml`. Returns the settings as written.
pub fn set_appearance(dir: &Path, appearance: Appearance) -> Result<Settings, SettingsError> {
    edit_settings(dir, |table, _| {
        table.insert("appearance".into(), toml::Value::try_from(appearance)?);
        Ok(())
    })
}

/// Sets the lines each log session keeps and writes `settings.toml`. Returns the settings as
/// written.
pub fn set_log_buffer_lines(dir: &Path, lines: u32) -> Result<Settings, SettingsError> {
    edit_settings(dir, |table, _| {
        table.insert(
            "log_buffer_lines".into(),
            toml::Value::Integer(lines.into()),
        );
        Ok(())
    })
}

/// Sets the MiB of memory each log session's lines may take and writes `settings.toml`. Returns
/// the settings as written.
pub fn set_log_buffer_mib(dir: &Path, mib: u32) -> Result<Settings, SettingsError> {
    edit_settings(dir, |table, _| {
        table.insert("log_buffer_mib".into(), toml::Value::Integer(mib.into()));
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
    validate(&settings, &path)?;
    write_atomic(&path, &text)?;
    Ok(settings)
}

fn load_toml<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|source| SettingsError::Parse {
            path: path.to_owned(),
            source,
        }),
        // A missing file is a fresh install. A symlink whose target is missing is not: the
        // user keeps the file elsewhere, and it may have said something (such as
        // `protect_local_clusters`) that defaults would quietly undo.
        Err(e) if e.kind() == io::ErrorKind::NotFound && path.symlink_metadata().is_err() => {
            Ok(T::default())
        }
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
mod cluster_tag_tests {
    use super::*;

    const A: &str = "https://a.example:6443";
    const B: &str = "https://b.example:6443";

    fn load(dir: &Path, text: &str) -> Settings {
        std::fs::write(dir.join(SETTINGS_FILE), text).unwrap();
        load_settings(dir).unwrap()
    }

    // covers UP-83: an unknown colour or a malformed entry is no tag; its neighbours and every
    // other setting still load.
    #[test]
    fn an_unreadable_tag_is_no_tag_and_other_settings_survive() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load(
            dir.path(),
            r#"
            protect_local_clusters = true
            cluster_tags = [
                { context = "new", server = "https://a.example:6443", tag = "ultraviolet" },
                { context = "no-server", tag = "red" },
                "not a table",
                { context = "good", server = "https://a.example:6443", tag = "teal" },
            ]
            [[trusted_clusters]]
            context = "good"
            server = "https://a.example:6443"
            "#,
        );
        assert!(settings.protect_local_clusters);
        assert!(settings.trust("good", Some(A)).is_trusted());
        assert_eq!(settings.cluster_tag("new", Some(A)), None);
        assert_eq!(settings.cluster_tag("no-server", Some(A)), None);
        assert_eq!(
            settings.cluster_tag("good", Some(A)),
            Some(ClusterTag::Teal)
        );
    }

    // covers UP-83
    #[test]
    fn cluster_tags_of_the_wrong_type_load_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load(dir.path(), "cluster_tags = 3\nappearance = \"dark\"\n");
        assert!(settings.cluster_tags.is_empty());
        assert_eq!(settings.appearance, Appearance::Dark);
        let written = set_cluster_tag(dir.path(), "a", A, Some(ClusterTag::Red)).unwrap();
        assert_eq!(written.cluster_tag("a", Some(A)), Some(ClusterTag::Red));
        assert_eq!(written.appearance, Appearance::Dark);
    }

    // covers UP-83: a colour chosen for one server never shows on another, and shows again when
    // the context points back.
    #[test]
    fn a_tag_follows_the_server_not_the_context_name() {
        let dir = tempfile::tempdir().unwrap();
        let tagged = set_cluster_tag(dir.path(), "ctx", A, Some(ClusterTag::Red)).unwrap();
        assert_eq!(tagged.cluster_tag("ctx", Some(A)), Some(ClusterTag::Red));
        assert_eq!(
            tagged.cluster_tag("ctx", Some("https://a.example:6443/")),
            Some(ClusterTag::Red),
            "a trailing slash is the same server"
        );
        assert_eq!(tagged.cluster_tag("ctx", Some(B)), None, "re-pointed");
        assert_eq!(tagged.cluster_tag("ctx", None), None, "no server");
        assert_eq!(tagged.cluster_tag("other", Some(A)), None);

        // A colour for the new server leaves the old one's for the way back.
        let both = set_cluster_tag(dir.path(), "ctx", B, Some(ClusterTag::Blue)).unwrap();
        assert_eq!(both.cluster_tag("ctx", Some(B)), Some(ClusterTag::Blue));
        assert_eq!(both.cluster_tag("ctx", Some(A)), Some(ClusterTag::Red));

        let cleared = set_cluster_tag(dir.path(), "ctx", B, None).unwrap();
        assert_eq!(cleared.cluster_tag("ctx", Some(B)), None);
        assert_eq!(cleared.cluster_tag("ctx", Some(A)), Some(ClusterTag::Red));
    }

    // covers UP-83: changing one tag keeps an entry from a newer version, and the rest of the file.
    #[test]
    fn writing_a_tag_keeps_what_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        load(
            dir.path(),
            r#"
            future_key = 1
            cluster_tags = [
                { context = "new", server = "https://a.example:6443", tag = "ultraviolet" },
            ]
            "#,
        );
        set_cluster_tag(dir.path(), "ctx", A, Some(ClusterTag::Green)).unwrap();
        let text = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("ultraviolet"), "{text}");
        assert!(text.contains("future_key"), "{text}");
        let settings = load_settings(dir.path()).unwrap();
        assert_eq!(
            settings.cluster_tag("ctx", Some(A)),
            Some(ClusterTag::Green)
        );

        set_cluster_tag(dir.path(), "new", A, Some(ClusterTag::Blue)).unwrap();
        let text = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(
            !text.contains("ultraviolet"),
            "replaced by the choice: {text}"
        );
    }

    // covers UP-83: the last tag removed leaves no empty key behind.
    #[test]
    fn clearing_the_last_tag_removes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        set_cluster_tag(dir.path(), "ctx", A, Some(ClusterTag::Green)).unwrap();
        set_cluster_tag(dir.path(), "ctx", A, None).unwrap();
        let text = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(!text.contains("cluster_tags"), "{text}");
    }

    // covers UP-83: clearing a tag that is not there leaves the file as it is, or absent.
    #[test]
    fn clearing_no_tag_does_not_touch_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        set_cluster_tag(dir.path(), "ctx", A, None).unwrap();
        assert!(!path.exists(), "no file is created");

        let text = "# kept by hand\nappearance = \"dark\"\n";
        std::fs::write(&path, text).unwrap();
        let settings = set_cluster_tag(dir.path(), "ctx", A, None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(settings.appearance, Appearance::Dark);
    }

    #[test]
    fn cluster_tags_is_a_known_key() {
        let table: toml::Table = toml::from_str("cluster_tags = []").unwrap();
        assert!(misspelled_keys(&table).is_empty());
    }
}

#[cfg(test)]
mod threshold_settings_tests {
    use super::*;
    use crate::thresholds::ThresholdMetric::{CpuPercent, MemoryPercent, RestartsLastHour};

    const A: &str = "https://a.example:6443";
    const B: &str = "https://b.example:6443";

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join(SETTINGS_FILE), text).unwrap();
    }

    fn read(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap()
    }

    // covers UP-67: a file from before thresholds loads with the defaults, and an unrelated save
    // adds no thresholds key and keeps what it held.
    #[test]
    fn a_file_without_thresholds_loads_and_saves_unchanged_in_meaning() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
            appearance = "dark"
            protect_local_clusters = true
            watch_mode = "on_view"
            future_key = 1
            [cluster_watch_mode]
            prod = "all"
            [[trusted_clusters]]
            context = "prod"
            server = "https://a.example:6443"
        "#;
        write(dir.path(), text);
        let before = load_settings(dir.path()).unwrap();
        assert_eq!(before.thresholds, ThresholdSettings::default());
        assert_eq!(
            before.thresholds_for("prod", Some(A)),
            Thresholds::default()
        );

        let saved = set_appearance(dir.path(), Appearance::Dark).unwrap();
        assert_eq!(saved, before);
        let written = read(dir.path());
        assert!(!written.contains("thresholds"), "{written}");
        assert!(written.contains("future_key"), "{written}");
        assert_eq!(load_settings(dir.path()).unwrap(), before);
    }

    // covers UP-67: an override replaces only what it sets, and stays with its server.
    #[test]
    fn an_override_is_bound_to_the_context_and_server() {
        let dir = tempfile::tempdir().unwrap();
        set_threshold(dir.path(), None, CpuPercent, Some(80)).unwrap();
        let pin = ClusterPin::new("ctx", A);
        let settings = set_threshold(dir.path(), Some(&pin), MemoryPercent, Some(60)).unwrap();

        let here = settings.thresholds_for("ctx", Some("https://a.example:6443/"));
        assert_eq!(here.get(MemoryPercent), 60);
        assert_eq!(here.get(CpuPercent), 80, "from the global value");
        assert_eq!(here.get(RestartsLastHour), 5, "from the default");
        for (context, server) in [("ctx", Some(B)), ("ctx", None), ("other", Some(A))] {
            let elsewhere = settings.thresholds_for(context, server);
            assert_eq!(elsewhere.get(MemoryPercent), 90, "{context} {server:?}");
            assert_eq!(elsewhere.get(CpuPercent), 80, "{context} {server:?}");
        }
        assert_eq!(load_settings(dir.path()).unwrap(), settings);
    }

    // covers UP-67: a bad section loses only itself, is reported, and survives an unrelated save.
    #[test]
    fn invalid_thresholds_fall_back_and_keep_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        for section in [
            "thresholds = 3",
            "[thresholds]\ncpu_percent = 500",
            "[thresholds]\nclusters = 3",
            "[thresholds.clusters.prod]\ncpu_percent = 50",
            "[[thresholds.clusters]]\ncontext = \"no-server\"\ncpu_percent = 50",
        ] {
            write(
                dir.path(),
                &format!("appearance = \"dark\"\nprotect_local_clusters = true\n{section}\n"),
            );
            let settings = load_settings(dir.path()).unwrap();
            assert_eq!(settings.appearance, Appearance::Dark, "{section}");
            assert!(settings.protect_local_clusters, "{section}");
            assert_eq!(
                settings.thresholds_for("no-server", Some(A)),
                Thresholds::default(),
                "{section}"
            );
            set_appearance(dir.path(), Appearance::Light).unwrap();
            let kept: toml::Table = toml::from_str(&read(dir.path())).unwrap();
            let original: toml::Table = toml::from_str(&format!("{section}\n")).unwrap();
            assert_eq!(
                kept.get("thresholds"),
                original.get("thresholds"),
                "{section}"
            );
        }
    }

    // covers UP-67: a refused write changes nothing, and a section of the wrong shape is not
    // replaced.
    #[test]
    fn a_refused_write_leaves_the_file_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let pin = ClusterPin::new("ctx", A);
        for (text, cluster, value) in [
            ("appearance = \"dark\"\n", None, Some(101)),
            ("thresholds = 3\n", None, Some(80)),
            ("[thresholds]\nclusters = 3\n", Some(&pin), Some(80)),
            ("[thresholds]\nclusters = 3\n", Some(&pin), None),
        ] {
            write(dir.path(), text);
            let error = set_threshold(dir.path(), cluster, CpuPercent, value).unwrap_err();
            assert!(matches!(error, SettingsError::Invalid { .. }), "{text}");
            assert_eq!(read(dir.path()), text);
        }
        let blank = ClusterPin::new("ctx", " ");
        write(dir.path(), "appearance = \"dark\"\n");
        assert!(set_threshold(dir.path(), Some(&blank), CpuPercent, Some(80)).is_err());
    }

    // covers UP-67: clearing leaves no empty section, keeps a newer version's keys, and a
    // repeated entry cannot bring the value back.
    #[test]
    fn clearing_removes_only_what_it_set() {
        let dir = tempfile::tempdir().unwrap();
        let pin = ClusterPin::new("ctx", A);
        set_threshold(dir.path(), Some(&pin), CpuPercent, Some(50)).unwrap();
        set_threshold(dir.path(), Some(&pin), CpuPercent, None).unwrap();
        assert!(!read(dir.path()).contains("thresholds"));

        write(
            dir.path(),
            r#"
            [thresholds]
            future_metric = 1
            [[thresholds.clusters]]
            context = "ctx"
            server = "https://a.example:6443"
            cpu_percent = 50
            future_field = 2
            [[thresholds.clusters]]
            context = "ctx"
            server = "https://a.example:6443/"
            cpu_percent = 40
            "#,
        );
        let settings = set_threshold(dir.path(), Some(&pin), CpuPercent, None).unwrap();
        assert_eq!(settings.thresholds_for("ctx", Some(A)).get(CpuPercent), 90);
        let written = read(dir.path());
        assert!(written.contains("future_metric"), "{written}");
        assert!(written.contains("future_field"), "{written}");
    }

    // covers UP-67
    #[test]
    fn clearing_what_is_not_set_does_not_touch_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        let pin = ClusterPin::new("ctx", A);
        set_threshold(dir.path(), Some(&pin), CpuPercent, None).unwrap();
        assert!(!path.exists(), "no file is created");

        let text = "# kept by hand\n[thresholds]\ncpu_percent = 80\n";
        write(dir.path(), text);
        set_threshold(dir.path(), Some(&pin), CpuPercent, None).unwrap();
        set_threshold(dir.path(), None, MemoryPercent, None).unwrap();
        assert_eq!(read(dir.path()), text);
    }

    #[test]
    fn thresholds_is_a_known_key() {
        let table: toml::Table = toml::from_str("thresholds = 3").unwrap();
        assert!(misspelled_keys(&table).is_empty());
    }
}

#[cfg(test)]
mod safe_reason_tests {
    use super::*;

    fn reason(dir: &Path, text: &str) -> String {
        std::fs::write(dir.join(SETTINGS_FILE), text).unwrap();
        load_settings(dir).unwrap_err().safe_reason()
    }

    // covers UP-12: a misspelt key is named with its place, and no value from the file.
    #[test]
    fn a_misspelt_key_is_named_with_its_line_and_no_value() {
        let dir = tempfile::tempdir().unwrap();
        let text = reason(
            dir.path(),
            "[[ai_exposed_clusters]]\ncontext = \"dev\"\nserver = \"https://x\"\n\
             namespace = [\"SECRETVALUE\"]\n",
        );
        assert!(
            text.starts_with("unknown field namespace at line 4, column 1"),
            "{text}"
        );
        assert!(!text.contains("SECRETVALUE"), "{text}");
    }

    // covers UP-12: a value in a type error is not quoted.
    #[test]
    fn a_wrong_type_quotes_no_value() {
        let dir = tempfile::tempdir().unwrap();
        let text = reason(
            dir.path(),
            "path = 1\nai_exposed_clusters = \"SECRETVALUE\"\n",
        );
        assert!(text.starts_with("invalid TOML at line"), "{text}");
        assert!(!text.contains("SECRETVALUE"), "{text}");
    }

    // covers UP-12: the reason comes first, so a long path cannot cut it off.
    #[test]
    fn a_long_path_keeps_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let long = dir.path().join("d".repeat(200)).join("e".repeat(200));
        std::fs::create_dir_all(&long).unwrap();
        let text = reason(&long, "[[ai_exposed_clusters]]\nnamespace = []\n");
        assert!(
            text.starts_with("unknown field namespace at line 2"),
            "{text}"
        );
        assert!(text.chars().count() <= 201, "{text}");
    }

    #[test]
    fn a_key_that_looks_like_a_credential_is_not_named() {
        assert_eq!(
            unknown_field("unknown field `namespace`, expected"),
            Some("namespace")
        );
        for message in [
            "unknown field `AKIAIOSFODNN7EXAMPLE`, expected",
            "unknown field `a b`, expected",
            "unknown field `namespaces``, expected",
            "invalid type: string `namespace`",
        ] {
            assert_eq!(unknown_field(message), None, "{message}");
        }
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
        assert_eq!(settings.appearance, Appearance::System);
        assert_eq!(load_state(dir.path()).unwrap(), State::default());
    }

    #[test]
    fn empty_file_gives_defaults() {
        let settings: Settings = toml::from_str("").unwrap();
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.log_buffer_lines, DEFAULT_LOG_BUFFER_LINES);
        assert_eq!(settings.log_buffer_limit(), 100_000);
    }

    // covers UP-40
    #[test]
    fn log_buffer_lines_is_typed_bounded_and_written() {
        let settings: Settings = toml::from_str("log_buffer_lines = 5000").unwrap();
        assert_eq!(settings.log_buffer_limit(), 5000);
        let tiny: Settings = toml::from_str("log_buffer_lines = 3").unwrap();
        assert_eq!(tiny.log_buffer_limit(), 1_000);
        let huge: Settings = toml::from_str("log_buffer_lines = 4000000000").unwrap();
        assert_eq!(huge.log_buffer_limit(), 1_000_000);
        assert!(toml::from_str::<Settings>("log_buffer_lines = \"lots\"").is_err());
        assert!(toml::from_str::<Settings>("log_buffer_lines = -1").is_err());

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "watch_mode = \"on_view\"\n").unwrap();
        let written = set_log_buffer_lines(dir.path(), 20_000).unwrap();
        assert_eq!(written.log_buffer_lines, 20_000);
        assert_eq!(written.watch_mode, WatchMode::OnView);
        assert_eq!(load_settings(dir.path()).unwrap(), written);
    }

    // covers UP-42
    #[test]
    fn log_total_mib_is_typed_and_bounded() {
        assert_eq!(Settings::default().log_total_bytes(), 512 * 1024 * 1024);
        let mib = |text: &str| toml::from_str::<Settings>(text).unwrap().log_total_bytes();
        assert_eq!(mib("log_total_mib = 1024"), 1024 * 1024 * 1024);
        assert_eq!(mib("log_total_mib = 1"), 64 * 1024 * 1024);
        assert_eq!(mib("log_total_mib = 99999999"), 16_384 * 1024 * 1024);
        assert!(toml::from_str::<Settings>("log_total_mib = \"lots\"").is_err());
        // Never below one tab's own cap.
        assert_eq!(
            mib("log_buffer_mib = 1024\nlog_total_mib = 512"),
            1024 * 1024 * 1024
        );
    }

    // covers UP-40
    #[test]
    fn log_buffer_mib_is_typed_bounded_and_written() {
        assert_eq!(Settings::default().log_buffer_bytes(), 64 * 1024 * 1024);
        let mib = |text: &str| toml::from_str::<Settings>(text).unwrap().log_buffer_bytes();
        assert_eq!(mib("log_buffer_mib = 128"), 128 * 1024 * 1024);
        assert_eq!(mib("log_buffer_mib = 0"), 8 * 1024 * 1024);
        assert_eq!(mib("log_buffer_mib = 999999"), 1024 * 1024 * 1024);
        assert!(toml::from_str::<Settings>("log_buffer_mib = 1.5").is_err());

        let dir = tempfile::tempdir().unwrap();
        let written = set_log_buffer_mib(dir.path(), 32).unwrap();
        assert_eq!(written.log_buffer_mib, 32);
        assert_eq!(load_settings(dir.path()).unwrap(), written);
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
            appearance = "dark"
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
        assert_eq!(settings.appearance, Appearance::Dark);
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
    fn rejects_unknown_appearance() {
        assert!(toml::from_str::<Settings>("appearance = \"sepia\"").is_err());
        assert!(toml::from_str::<Settings>("appearance = \"Dark\"").is_err());
    }

    // covers UP-32
    #[test]
    fn appearance_is_written_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "watch_mode = \"on_view\"\nfuture_key = 1\n",
        )
        .unwrap();
        let s = set_appearance(dir.path(), Appearance::Light).unwrap();
        assert_eq!(s.appearance, Appearance::Light);
        let s = set_appearance(dir.path(), Appearance::System).unwrap();
        assert_eq!(s.appearance, Appearance::System);
        let loaded = load_settings(dir.path()).unwrap();
        assert_eq!(loaded, s);
        assert_eq!(loaded.watch_mode, WatchMode::OnView);
        let text = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(text.contains("appearance = \"system\""), "{text}");
        assert!(text.contains("future_key"), "unknown keys survive: {text}");
    }

    #[test]
    fn config_dir_prefers_override() {
        for layout in [ConfigLayout::MacOs, ConfigLayout::Xdg] {
            assert_eq!(
                config_dir_from(
                    layout,
                    Some("/tmp/x".into()),
                    Some("/xdg".into()),
                    Some("/home/me".into())
                )
                .unwrap(),
                PathBuf::from("/tmp/x")
            );
            assert!(config_dir_from(layout, Some("".into()), None, None).is_err());
        }
    }

    #[test]
    fn config_dir_on_macos_is_application_support() {
        assert_eq!(
            config_dir_from(
                ConfigLayout::MacOs,
                None,
                Some("/xdg".into()),
                Some("/Users/me".into())
            )
            .unwrap(),
            PathBuf::from("/Users/me/Library/Application Support/Flint")
        );
    }

    #[test]
    fn config_dir_on_linux_follows_xdg() {
        let dir = |xdg: Option<&str>| {
            config_dir_from(
                ConfigLayout::Xdg,
                Some("".into()),
                xdg.map(PathBuf::from),
                Some("/home/me".into()),
            )
            .unwrap()
        };
        assert_eq!(dir(Some("/xdg")), PathBuf::from("/xdg/flint"));
        assert_eq!(dir(None), PathBuf::from("/home/me/.config/flint"));
        // An empty or relative XDG_CONFIG_HOME is ignored, as the spec says.
        assert_eq!(dir(Some("")), PathBuf::from("/home/me/.config/flint"));
        assert_eq!(dir(Some("rel")), PathBuf::from("/home/me/.config/flint"));
        assert!(config_dir_from(ConfigLayout::Xdg, None, None, None).is_err());
    }

    #[test]
    fn host_layout_matches_the_target() {
        let want = if cfg!(target_os = "macos") {
            ConfigLayout::MacOs
        } else {
            ConfigLayout::Xdg
        };
        assert_eq!(ConfigLayout::HOST, want);
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
        let s = set_ai_exposed(dir.path(), "a", Some("https://a"), &AiScope::default()).unwrap();
        assert!(s.ai_exposure("a", Some("https://a")).is_exposed());
        set_ai_exposed(dir.path(), "b", Some("https://b"), &AiScope::default()).unwrap();
        // Re-exposing at a new server replaces the old entry.
        let s = set_ai_exposed(dir.path(), "b", Some("https://b2"), &AiScope::default()).unwrap();
        assert_eq!(s.ai_exposed_clusters.len(), 2);
        assert!(s.ai_exposure("b", Some("https://b2")).is_exposed());
        assert!(!s.ai_exposure("b", Some("https://b")).is_exposed());
        let s = set_ai_exposed(dir.path(), "a", None, &AiScope::default()).unwrap();
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

    // covers UP-14: entries written before scopes existed load as the whole cluster; scoped
    // ones keep their lists, cleaned up; the scope is written back atomically with the pin.
    #[test]
    fn exposure_scopes_load_old_entries_and_round_trip() {
        let old: Settings = toml::from_str(
            "ai_exposed_clusters = [{ context = \"dev\", server = \"https://dev\" }]\n",
        )
        .unwrap();
        assert_eq!(
            old.ai_scope("dev", Some("https://dev")),
            Some(AiScope::default())
        );
        assert!(
            old.ai_scope("dev", Some("https://dev"))
                .unwrap()
                .is_everything()
        );
        assert_eq!(old.ai_scope("dev", Some("https://other")), None);
        assert_eq!(old.ai_scope("dev", None), None);

        let scoped: Settings = toml::from_str(
            r#"
            [[ai_exposed_clusters]]
            context = "prod"
            server = "https://prod"
            namespaces = ["shop", "billing"]
            kinds = ["Pods", "deployments.apps"]
            "#,
        )
        .unwrap();
        let scope = scoped.ai_scope("prod", Some("https://prod/")).unwrap();
        assert_eq!(scope.namespaces, ["billing", "shop"]);
        assert_eq!(scope.kinds, ["deployments.apps", "pods"]);
        assert!(
            scoped
                .ai_exposure("prod", Some("https://prod"))
                .is_exposed()
        );
        assert!(
            toml::from_str::<Settings>(
                "ai_exposed_clusters = [{ context = \"a\", server = \"s\", namespaces = \"x\" }]"
            )
            .is_err()
        );

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "future = 1\n").unwrap();
        let wanted = AiScope::new(
            vec![" shop ".into(), "shop".into(), "".into()],
            vec!["PODS".into()],
        );
        let s = set_ai_exposed(dir.path(), "prod", Some("https://prod"), &wanted).unwrap();
        assert_eq!(
            s.ai_scope("prod", Some("https://prod")),
            Some(wanted.clone())
        );
        assert_eq!(wanted.namespaces, ["shop"]);
        assert_eq!(wanted.kinds, ["pods"]);
        // Clearing the scope exposes everything again, and the empty lists are not written.
        let s = set_ai_exposed(
            dir.path(),
            "prod",
            Some("https://prod"),
            &AiScope::default(),
        )
        .unwrap();
        assert!(
            s.ai_scope("prod", Some("https://prod"))
                .unwrap()
                .is_everything()
        );
        let raw = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(
            !raw.contains("namespaces") && raw.contains("future"),
            "{raw}"
        );
        assert_eq!(load_settings(dir.path()).unwrap(), s);
    }

    // covers UP-14: a scope list of only blank names was meant to limit the cluster; read as
    // empty it would expose everything, so the file is refused (and nothing is exposed).
    #[test]
    fn a_scope_of_blank_names_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for list in ["namespaces = [\"\"]", "kinds = [\"  \"]"] {
            std::fs::write(
                dir.path().join(SETTINGS_FILE),
                format!(
                    "[[ai_exposed_clusters]]\ncontext = \"prod\"\nserver = \"https://p\"\n{list}\n"
                ),
            )
            .unwrap();
            assert!(
                matches!(
                    load_settings(dir.path()),
                    Err(SettingsError::Invalid { .. })
                ),
                "{list}"
            );
            let (fallback, error) = load_settings_or_fallback(dir.path());
            assert!(error.is_some() && fallback.ai_exposed_clusters.is_empty());
        }
        // A blank name beside real ones is just dropped.
        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "[[ai_exposed_clusters]]\ncontext = \"prod\"\nserver = \"https://p\"\nnamespaces = [\"\", \"shop\"]\n",
        )
        .unwrap();
        let s = load_settings(dir.path()).unwrap();
        assert_eq!(
            s.ai_scope("prod", Some("https://p")).unwrap().namespaces,
            ["shop"]
        );
    }

    // covers UP-14: a misspelt scope key, or two entries for one context and server, make the
    // file invalid (nothing exposed) rather than exposing the whole cluster; the error names
    // what is wrong.
    #[test]
    fn misspelt_scope_keys_and_duplicate_entries_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "[[ai_exposed_clusters]]\ncontext = \"prod\"\nserver = \"https://p\"\nnamespace = [\"shop\"]\n",
        )
        .unwrap();
        let error = load_settings(dir.path()).unwrap_err().to_string();
        assert!(error.contains("namespace"), "{error}");
        assert!(
            load_settings_or_fallback(dir.path())
                .0
                .ai_exposed_clusters
                .is_empty()
        );

        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "[[ai_exposed_clusters]]\ncontext = \"prod\"\nserver = \"https://p\"\n\
             namespaces = [\"shop\"]\n\
             [[ai_exposed_clusters]]\ncontext = \"prod\"\nserver = \"https://p/\"\n",
        )
        .unwrap();
        let error = load_settings(dir.path()).unwrap_err().to_string();
        assert!(
            error.contains("listed twice") && error.contains("prod"),
            "{error}"
        );
        // The same context at two servers is not a duplicate.
        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "[[ai_exposed_clusters]]\ncontext = \"prod\"\nserver = \"https://p\"\n\
             [[ai_exposed_clusters]]\ncontext = \"prod\"\nserver = \"https://q\"\n",
        )
        .unwrap();
        assert!(load_settings(dir.path()).is_ok());
    }

    // covers UP-14: changing a scope never exposes a cluster that was hidden or re-pointed
    // meanwhile; a re-pointed context's old scope is kept to start from.
    #[test]
    fn a_scope_change_needs_the_exposure_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let shop = AiScope::new(vec!["shop".into()], Vec::new());
        assert!(matches!(
            set_ai_scope(dir.path(), "prod", "https://p", &AiScope::default(), &shop),
            Err(SettingsError::NotExposed { .. })
        ));
        assert!(!dir.path().join(SETTINGS_FILE).exists());
        set_ai_exposed(dir.path(), "prod", Some("https://p"), &AiScope::default()).unwrap();
        let s = set_ai_scope(dir.path(), "prod", "https://p/", &AiScope::default(), &shop).unwrap();
        // Edited from a scope that changed on disk since: refused, nothing written.
        assert!(matches!(
            set_ai_scope(
                dir.path(),
                "prod",
                "https://p",
                &AiScope::default(),
                &AiScope::default()
            ),
            Err(SettingsError::ScopeChanged { .. })
        ));
        assert_eq!(
            load_settings(dir.path())
                .unwrap()
                .ai_scope("prod", Some("https://p")),
            Some(shop.clone())
        );
        assert_eq!(s.ai_scope("prod", Some("https://p")), Some(shop.clone()));
        assert!(matches!(
            set_ai_scope(
                dir.path(),
                "prod",
                "https://elsewhere",
                &shop,
                &AiScope::default()
            ),
            Err(SettingsError::NotExposed { .. })
        ));
        assert_eq!(s.ai_scope_saved("prod"), Some(shop.clone()));
        assert_eq!(s.ai_scope("prod", Some("https://elsewhere")), None);
        assert_eq!(s.ai_scope_saved("other"), None);
    }

    // covers UP-14: what a scope lets agents read.
    #[test]
    fn scopes_allow_named_namespaces_and_kinds() {
        let everything = AiScope::default();
        assert!(everything.allows_namespace("any"));
        assert!(everything.allows_kind("nodes", false));
        let kinds = AiScope::new(Vec::new(), vec!["pods".into(), "nodes".into()]);
        assert!(kinds.allows_namespace("any"));
        assert!(kinds.allows_kind("pods", true) && kinds.allows_kind("nodes", false));
        assert!(!kinds.allows_kind("secrets", true));
        let namespaces = AiScope::new(vec!["shop".into()], Vec::new());
        assert!(namespaces.allows_namespace("shop") && !namespaces.allows_namespace("kube-system"));
        assert!(namespaces.allows_kind("secrets", true));
        // Cluster-scoped kinds are hidden under a namespace scope unless named.
        assert!(!namespaces.allows_kind("nodes", false));
        assert!(!namespaces.allows_kind("namespaces", false));
        let both = AiScope::new(vec!["shop".into()], vec!["pods".into(), "nodes".into()]);
        assert!(both.allows_kind("nodes", false) && both.allows_kind("pods", true));
        assert!(!both.allows_kind("customresourcedefinitions.apiextensions.k8s.io", false));
        assert!(!both.allows_kind("services", true));
    }

    // covers UP-55: trust is pinned to the context and its server, like AI exposure.
    #[test]
    fn trust_needs_name_and_server() {
        let settings: Settings = toml::from_str(
            r#"
            protect_local_clusters = true
            [[trusted_clusters]]
            context = "staging"
            server = "https://staging.example:6443"
            "#,
        )
        .unwrap();
        assert!(settings.protect_local_clusters);
        assert!(!Settings::default().protect_local_clusters);
        assert!(Settings::default().trusted_clusters.is_empty());
        assert!(
            settings
                .trust("staging", Some("https://staging.example:6443/"))
                .is_trusted()
        );
        assert_eq!(
            settings.trust("staging", Some("https://prod.example:6443")),
            TrustState::ServerChanged {
                trusted_server: "https://staging.example:6443".into()
            }
        );
        assert!(!settings.trust("staging", None).is_trusted());
        assert_eq!(
            settings.trust("prod", Some("https://staging.example:6443")),
            TrustState::NotTrusted
        );
        // Exposure and trust are separate lists.
        assert_eq!(
            settings.ai_exposure("staging", Some("https://staging.example:6443")),
            AiExposureState::Hidden
        );
        assert!(toml::from_str::<Settings>("trusted_clusters = [\"staging\"]").is_err());
    }

    // covers UP-54: a settings.toml that does not parse opens nothing up. It may have said
    // `protect_local_clusters = true`, so a known local context is protected until the file
    // parses again; a missing or valid file keeps its exemption. Another context on a loopback
    // address is protected either way.
    #[test]
    fn an_unreadable_file_protects_local_clusters_and_trusts_nothing() {
        use crate::protection::{Protection, SessionUnlocks, protection};
        let unlocks = SessionUnlocks::default();
        let loopback = Some("https://127.0.0.1:8443");
        let of = |s: &Settings| protection(s, &unlocks, "kind-dev", loopback, None);
        let other = |s: &Settings| protection(s, &unlocks, "team-dev", loopback, None);

        let dir = tempfile::tempdir().unwrap();
        let (missing, error) = load_settings_or_fallback(dir.path());
        assert_eq!(error, None);
        assert_eq!(of(&missing), Protection::Local);
        assert_eq!(other(&missing), Protection::Protected);

        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "protect_local_clusters = true\n\
             trusted_clusters = [{ context = \"team-dev\", server = \"https://127.0.0.1:8443\" }]\n\
             watch_mode = ",
        )
        .unwrap();
        let (broken, error) = load_settings_or_fallback(dir.path());
        assert!(error.is_some());
        assert!(broken.trusted_clusters.is_empty() && broken.ai_exposed_clusters.is_empty());
        assert_eq!(of(&broken), Protection::Protected);
        assert_eq!(other(&broken), Protection::Protected);

        std::fs::write(dir.path().join(SETTINGS_FILE), "watch_mode = \"all\"\n").unwrap();
        let (valid, error) = load_settings_or_fallback(dir.path());
        assert_eq!(error, None);
        assert_eq!(of(&valid), Protection::Local);
        assert_eq!(other(&valid), Protection::Protected);
    }

    // covers UP-54: settings.toml as a symlink to a file that is gone (a dotfiles checkout not
    // mounted) is unreadable, not a fresh install, so local clusters stay protected.
    #[test]
    fn a_dangling_symlink_is_unreadable_not_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path().join("gone.toml"), dir.path().join(SETTINGS_FILE))
            .unwrap();
        assert!(matches!(
            load_settings(dir.path()),
            Err(SettingsError::Read { .. })
        ));
        let (settings, error) = load_settings_or_fallback(dir.path());
        assert!(error.is_some());
        assert!(settings.protect_local_clusters);
        // State has no such stake, but reads the same way.
        std::os::unix::fs::symlink(dir.path().join("gone2.toml"), dir.path().join(STATE_FILE))
            .unwrap();
        assert!(load_state(dir.path()).is_err());
    }

    // covers UP-54: a misspelt key is ignored like any unknown one, so it is named in the log.
    #[test]
    fn misspelt_keys_are_found_and_new_ones_are_not() {
        let table: toml::Table = toml::from_str(
            "protect_local_cluster = true\nprotect_locale_clusters = true\n\
             trusted_cluster = []\nwatch_mode = \"all\"\nsome_future_key = 3\npth = \"/bin\"\n",
        )
        .unwrap();
        let mut found = misspelled_keys(&table);
        found.sort();
        assert_eq!(
            found,
            [
                ("protect_local_cluster".to_owned(), "protect_local_clusters"),
                (
                    "protect_locale_clusters".to_owned(),
                    "protect_local_clusters"
                ),
                ("trusted_cluster".to_owned(), "trusted_clusters"),
            ]
        );
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    // covers UP-55
    // covers UP-01, UP-90: a custom name lives in settings.toml, keyed by context.
    #[test]
    fn cluster_names_round_trip_and_keep_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SETTINGS_FILE), "watch_mode = \"on_view\"\n").unwrap();
        let context = "arn:aws:eks:eu-west-1:123456789012:cluster/example";
        let s = set_cluster_name(dir.path(), context, Some("  main ")).unwrap();
        assert_eq!(s.cluster_names[context], "main");
        assert_eq!(s.watch_mode, WatchMode::OnView);
        assert_eq!(load_settings(dir.path()).unwrap(), s);
        assert!(set_cluster_name(dir.path(), context, Some("a\u{202e}b")).is_err());
        assert_eq!(
            load_settings(dir.path()).unwrap(),
            s,
            "a refused name changes nothing"
        );
        // Taken in the file meanwhile, in any case: refused, and the other name stays.
        assert!(set_cluster_name(dir.path(), "other", Some("MAIN")).is_err());
        assert_eq!(load_settings(dir.path()).unwrap(), s);
        let s = set_cluster_name(dir.path(), context, None).unwrap();
        assert!(s.cluster_names.is_empty());
        let text = std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert!(!text.contains("cluster_names"), "{text}");
    }

    // covers UP-01: separators and look-alike letters collide when a name is set.
    #[test]
    fn a_look_alike_cluster_name_is_refused_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let s = set_cluster_name(dir.path(), "a", Some("prod-eu")).unwrap();
        for name in ["prod_eu", "PROD EU", "pr\u{43e}d-eu", "\u{ff50}rod-eu"] {
            assert!(
                set_cluster_name(dir.path(), "b", Some(name)).is_err(),
                "{name}"
            );
            assert_eq!(load_settings(dir.path()).unwrap(), s, "{name}");
        }
        // Its own name may be kept or re-cased.
        let s = set_cluster_name(dir.path(), "a", Some("prod-eu")).unwrap();
        assert_eq!(s.cluster_names["a"], "prod-eu");
        let s = set_cluster_name(dir.path(), "a", Some("Prod_EU")).unwrap();
        assert_eq!(s.cluster_names["a"], "Prod_EU");
        // A non-Latin name that collides with nothing is fine.
        let s = set_cluster_name(dir.path(), "b", Some("север")).unwrap();
        assert_eq!(s.cluster_names["b"], "север");
    }

    // covers UP-01: a saved name counts as it is shown, trimmed.
    #[test]
    fn saved_names_are_compared_as_they_are_shown() {
        let dir = tempfile::tempdir().unwrap();
        let text = "[cluster_names]\na = \"prod \"\nb = \"prod-\"\n";
        std::fs::write(dir.path().join(SETTINGS_FILE), text).unwrap();
        // Would make two names that are shown apart collide.
        assert!(set_cluster_name(dir.path(), "a", Some("prod-")).is_err());
        assert!(set_cluster_name(dir.path(), "c", Some("prod")).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(),
            text
        );
    }

    // covers UP-01: a file from before the stricter check loads as it is.
    #[test]
    fn saved_names_that_collide_load_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SETTINGS_FILE);
        let text = "[cluster_names]\na = \"prod-eu\"\nb = \"prod_eu\"\nc = \"pr\u{43e}d-eu\"\n";
        std::fs::write(&path, text).unwrap();

        let s = load_settings(dir.path()).unwrap();
        assert_eq!(s.cluster_names["a"], "prod-eu");
        assert_eq!(s.cluster_names["b"], "prod_eu");
        assert_eq!(s.cluster_names["c"], "pr\u{43e}d-eu");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "not rewritten"
        );

        let shown = crate::display::ClusterNames::new(["a", "b", "c"], &s.cluster_names);
        for context in ["a", "b", "c"] {
            assert_eq!(shown.of(context), context);
        }

        // A cluster in the pair can still save its own name, and the others are left alone.
        let s = set_cluster_name(dir.path(), "a", Some("PROD-EU")).unwrap();
        assert_eq!(s.cluster_names["a"], "PROD-EU");
        assert_eq!(s.cluster_names["b"], "prod_eu");
        assert_eq!(s.cluster_names["c"], "pr\u{43e}d-eu");
    }

    #[test]
    fn set_trusted_round_trips_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SETTINGS_FILE),
            "protect_local_clusters = true\nfuture = 1\n",
        )
        .unwrap();
        set_ai_exposed(dir.path(), "a", Some("https://a"), &AiScope::default()).unwrap();
        let s = set_trusted(dir.path(), "a", Some("https://a")).unwrap();
        assert!(s.trust("a", Some("https://a")).is_trusted());
        let s = set_trusted(dir.path(), "a", Some("https://a2")).unwrap();
        assert_eq!(s.trusted_clusters, vec![ClusterPin::new("a", "https://a2")]);
        let s = set_trusted(dir.path(), "a", None).unwrap();
        assert_eq!(s.trust("a", Some("https://a2")), TrustState::NotTrusted);
        assert!(s.ai_exposure("a", Some("https://a")).is_exposed());
        assert!(s.protect_local_clusters);
        let loaded = load_settings(dir.path()).unwrap();
        assert_eq!(loaded, s);
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
                dock: Some(300),
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
    fn list_sorts_round_trip_per_cluster_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = State::default();
        let sort = ListSort {
            column: "server:Restarts".into(),
            descending: true,
        };
        state
            .list_sorts
            .entry("kind-dev".into())
            .or_default()
            .insert("pods".into(), sort);
        // An EKS context name: colons and slashes in the table key.
        let arn = "arn:aws:eks:eu-north-1:123456789012:cluster/payments";
        state.list_sorts.entry(arn.into()).or_default().insert(
            "widgets.example.com".into(),
            ListSort {
                column: "server:Phase".into(),
                descending: true,
            },
        );
        state
            .list_sorts
            .entry("kind-dev".into())
            .or_default()
            .insert(
                "deployments.apps".into(),
                ListSort {
                    column: "flint:age".into(),
                    descending: false,
                },
            );
        save_state(dir.path(), &state).unwrap();
        assert_eq!(load_state(dir.path()).unwrap(), state);
        let text = std::fs::read_to_string(dir.path().join(STATE_FILE)).unwrap();
        assert!(text.contains("server:Restarts"), "{text}");
        // `descending` may be left out.
        std::fs::write(
            dir.path().join(STATE_FILE),
            "[list_sorts.a]\npods = { column = \"flint:name\" }\n",
        )
        .unwrap();
        let loaded = load_state(dir.path()).unwrap();
        assert!(!loaded.list_sorts["a"]["pods"].descending);
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
