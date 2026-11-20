//! The visible view tree as data: what a user would read off the window, for the test
//! automation channel's `state` command and for view tests. Built only with `--features
//! automation` (and in tests); release builds have no use for it.
//!
//! Each view fills in its own part (`*_state.rs` next to it). Text that could carry secrets
//! follows the screen: Secret cells are masked as the list masks them, Secret values are never
//! included (only which keys are revealed), and error messages go through
//! [`flint_core::redact::safe_error_summary`] (connection errors arrive already reduced, see
//! [`connection_error_text`]).

use flint_core::redact::safe_error_summary;
use flint_core::{SafeErrorText, SyncState};
use serde::Serialize;

/// The whole window.
#[derive(Debug, Clone, Serialize)]
pub struct AppState {
    /// What has keyboard focus.
    pub focus: Focus,
    /// The dialog open over the window (the Expose to AI confirmation), if any.
    pub dialog: Option<DialogState>,
    /// The `appearance` setting: `system`, `light` or `dark`.
    pub appearance: &'static str,
    /// Where the contexts come from.
    pub kubeconfig: KubeconfigState,
    pub clusters: Vec<ClusterState>,
    /// The sidebar's error cards and messages (settings, kubeconfig files, the last error).
    pub banners: Vec<Banner>,
    /// The active cluster, if any.
    pub active_cluster: Option<String>,
    /// What the main area shows.
    pub main: Main,
    pub header: Option<HeaderState>,
    pub nav: Option<NavState>,
    pub list: Option<ListState>,
    pub detail: Option<DetailState>,
    /// The bottom dock and its tabs.
    pub dock: DockState,
    /// The open context menu, if any.
    pub menu: Option<MenuState>,
}

/// Where keyboard focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Focus {
    /// Nothing inside the window's views (the workspace itself).
    Workspace,
    /// The resource list's rows.
    List,
    /// The list's text filter.
    ListFilter,
    /// The namespace filter's "add a namespace" field.
    NamespaceInput,
    /// The detail pane's YAML editor.
    DetailYaml,
    /// The dock, outside its tabs (its tab bar).
    Dock,
    /// A log tab's lines.
    Log,
    /// A log tab's search field.
    LogSearch,
    /// A shell tab's terminal.
    Shell,
    /// A dialog.
    Dialog,
    /// A context menu or a drop-down.
    Menu,
    /// Something else.
    Other,
    None,
}

/// A message shown in a colored card or strip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Banner {
    /// The element id it is drawn with (`list-banner`, `settings-error`).
    pub id: String,
    pub level: Level,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Muted,
    Warning,
    Danger,
}

impl Banner {
    pub fn new(id: &str, level: Level, text: impl Into<String>) -> Self {
        Self {
            id: id.to_owned(),
            level,
            text: text.into(),
        }
    }
}

/// An open dialog: what it says and the buttons it offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DialogState {
    pub title: String,
    pub text: String,
    pub buttons: Vec<String>,
}

/// The kubeconfig files read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KubeconfigState {
    /// The core has read them once.
    pub loaded: bool,
    pub paths: Vec<KubeconfigPath>,
    /// `KUBECONFIG` as the app saw it.
    pub env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KubeconfigPath {
    pub path: String,
    pub exists: bool,
}

/// One kubeconfig context in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterState {
    pub name: String,
    /// The name as the sidebar shows it (shortened for cloud contexts).
    pub label: String,
    pub active: bool,
    /// `disconnected`, `connecting`, `connected` or `failed`.
    pub connection: &'static str,
    /// What its dot shows: `not_connected`, `connecting`, `live` (connected and watched),
    /// `idle` (connected, watches stopped) or `failed`.
    pub presence: &'static str,
    /// Why the connection failed (a sanitized summary).
    pub error: Option<String>,
    /// `hidden`, `exposed` or `server_changed`.
    pub ai: &'static str,
}

/// What the main area shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum Main {
    /// "Select a cluster".
    NoCluster,
    /// No kubeconfig file had a context: where Flint looked and what to do.
    NoContexts,
    Connecting,
    /// Connected, with no list open yet.
    Connected {
        version: String,
    },
    /// The connection failed; a Retry button is shown.
    Failed {
        error: String,
    },
    /// A resource list, and maybe the detail pane.
    List,
}

/// The header over the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeaderState {
    /// Cluster, kind, object.
    pub breadcrumb: Vec<String>,
    /// The namespace filter; empty for all namespaces.
    pub namespaces: Vec<String>,
    /// The filter button's text ("All namespaces", "demo", "3 namespaces"), when shown (a
    /// namespaced kind, and not while the connection failed).
    pub namespace_button: Option<String>,
    /// The watch mode button's text, when shown (not while the connection failed).
    pub watch_mode: Option<String>,
    /// Whether the mode is the cluster's own (`cluster`) or the default (`default`).
    pub watch_mode_source: &'static str,
}

/// The resource navigation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavState {
    /// `discovering`, `failed` or `ready`.
    pub status: &'static str,
    pub error: Option<String>,
    pub sections: Vec<NavSectionState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavSectionState {
    pub title: String,
    pub collapsed: bool,
    pub count: usize,
    /// The entries shown: empty while the section is collapsed.
    pub entries: Vec<NavEntryState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavEntryState {
    /// The kind id, as `open_view` takes it.
    pub kind: String,
    pub label: String,
    pub selected: bool,
    /// The watch-state marker: `loading`, `reconnecting` or `failed`.
    pub marker: Option<&'static str>,
}

/// The open resource list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListState {
    pub kind: String,
    pub label: String,
    /// The state chip: `live`, `polled`, `loading`, `reconnecting`, `failed`, …
    pub sync: &'static str,
    /// Rows in the list, and rows passing the filters.
    pub total: usize,
    pub shown: usize,
    /// The toolbar's count as drawn ("7", "2 of 7"): out of the rows in the selected
    /// namespaces.
    pub count: String,
    pub columns: Vec<String>,
    /// The rows passing the filters, in display order, up to [`ListState::ROW_LIMIT`].
    pub rows: Vec<RowState>,
    pub rows_truncated: bool,
    pub filter: String,
    pub sort: Option<SortState>,
    pub wide: bool,
    /// What replaces the rows, if anything: `loading`, `unavailable`, `failed`,
    /// `reconnecting`, `empty` or `no_match`.
    pub placeholder: Option<&'static str>,
    pub banners: Vec<Banner>,
}

impl ListState {
    /// The most rows `state` reports, so a list of 10,000 stays a small reply.
    pub const ROW_LIMIT: usize = 500;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RowState {
    /// The element id the row is drawn with (`row-<uid>`), for `right_click`.
    pub id: String,
    pub namespace: Option<String>,
    pub name: String,
    pub selected: bool,
    /// Cell text as drawn (Secret cells masked), one per column.
    pub cells: Vec<String>,
    /// The same cells by column name, for `wait_for`'s `any`: `{"Status": "Running"}`.
    pub cells_by_column: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SortState {
    pub column: String,
    pub descending: bool,
}

/// The detail pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DetailState {
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    /// `overview` or `yaml`.
    pub tab: &'static str,
    /// `loading`, `loaded` or `failed`.
    pub fetch: &'static str,
    pub error: Option<String>,
    pub fetching: bool,
    pub labels_open: bool,
    pub annotations_open: bool,
    pub managed_fields_shown: bool,
    /// The overview's sections and how many entries each has (labels, owners, conditions,
    /// events; a Secret's data keys), never their values. Empty until the object is fetched.
    pub sections: Vec<SectionCount>,
    /// Secret keys whose value is revealed (never the values).
    pub revealed: Vec<String>,
    pub banners: Vec<Banner>,
    /// For pods and workloads: Logs, Previous Logs and Shell, and whether each can run now
    /// (what the row menu shows, disabled entries with their reason).
    pub actions: Vec<ActionAvailability>,
}

/// Whether an action can run now, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionAvailability {
    /// As the menu names it: `Logs`, `Previous Logs`, `Shell`.
    pub action: &'static str,
    pub enabled: bool,
    pub reason: Option<String>,
}

/// A detail-pane section and how many entries it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionCount {
    pub name: &'static str,
    pub count: usize,
}

/// The bottom dock.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DockState {
    pub visible: bool,
    /// Its height in points: as drawn while visible, and what it gets when shown.
    pub height: f32,
    /// Keyboard focus is in the dock (its tab bar or a tab).
    pub focused: bool,
    /// The id of the tab shown.
    pub active: Option<u64>,
    /// Every tab, left to right.
    pub tabs: Vec<TabState>,
}

/// One dock tab.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabState {
    /// What `dock::ActivateTab` and `dock::CloseTab` take.
    pub id: u64,
    /// `pod_logs`, `workload_logs`, `previous_logs` or `shell`.
    pub kind: &'static str,
    /// The title in full (the tab cuts it in the middle).
    pub title: String,
    pub active: bool,
    pub focused: bool,
    pub cluster: TabClusterState,
    pub status: TabStatusState,
    /// The status as the tab's toolbar says it (`Streaming`, `Ended · no permission`,
    /// `Exited (code 3)`).
    pub status_text: String,
    /// For log tabs.
    pub logs: Option<LogTabState>,
    /// For shell tabs.
    pub shell: Option<ShellTabState>,
}

/// The cluster a tab's session runs on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TabClusterState {
    pub context: String,
    /// The API server it pointed at when the tab opened, without credentials.
    pub server: Option<String>,
    /// What the tab's chip says.
    pub chip: String,
    /// The context now points at another server.
    pub repointed: bool,
    /// The context left kubeconfig.
    pub removed: bool,
}

/// How a tab's session is doing, by `state`. Reasons are in plain words, as the tab says them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TabStatusState {
    /// Opening the stream or the shell.
    Connecting,
    /// A log stream is reading.
    Streaming,
    /// A log stream waits to reopen: attempt `attempt`, after `reason` (`None` for a clean end
    /// of a stream that may log again). `back_off`: the container crash-loops.
    Reconnecting {
        attempt: u32,
        back_off: bool,
        reason: Option<String>,
    },
    /// Nothing more will come, and nothing failed: all lines read, the pod deleted, or a shell
    /// the tab ended itself (`reason` says why).
    Ended {
        reason: Option<String>,
        pod_deleted: bool,
    },
    /// The session ended on an error, said in plain words.
    Failed { reason: String },
    /// A shell runs `command`.
    Running { command: String },
    /// A shell's command ended, with its exit code when the server sent one.
    Exited { code: Option<i32> },
    /// The shell's session was closed.
    Closed,
}

/// A log tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogTabState {
    /// Lines shown (what the buffer held when last delivered).
    pub lines: usize,
    /// Lines that arrived while paused.
    pub held_back: u64,
    pub paused: bool,
    /// The session follows new lines (not a previous instance).
    pub follows: bool,
    /// The view sticks to the newest line (not scrolled up).
    pub following: bool,
    pub timestamps: bool,
    /// Workload lines show their `[pod/container]` prefix.
    pub sources: bool,
    pub wrap: bool,
    pub search: SearchState,
    pub selection: Option<SelectionState>,
    /// A workload's streams.
    pub streams: Option<StreamCounts>,
    /// The newest lines shown, at most [`LogTabState::LINE_LIMIT`], oldest first.
    pub last_lines: Vec<LogLineState>,
}

impl LogTabState {
    /// The most lines `state` reports of a tab: enough to wait for a line, never the buffer.
    pub const LINE_LIMIT: usize = 50;
}

/// A log tab's search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchState {
    pub query: String,
    pub matches: usize,
    /// The current match's position among them, from 1.
    pub current: Option<usize>,
    /// The first scan of the buffer is still running.
    pub searching: bool,
}

/// Selected log lines, by line id (ids count lines from the session's start).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelectionState {
    pub first: u64,
    pub last: u64,
    pub lines: u64,
}

/// A workload's log streams, as the toolbar counts them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamCounts {
    pub pods: usize,
    pub live: usize,
    pub connecting: usize,
    /// Containers that have not started yet.
    pub waiting: usize,
    pub failed: usize,
    /// Containers not read: more than the session reads at once.
    pub capped: usize,
}

/// One log line, as the tab holds it (without its timestamp and prefix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogLineState {
    pub id: u64,
    pub text: String,
    /// `normal`, `warning` or `error`.
    pub severity: &'static str,
    /// `pod/container`, in a workload tab.
    pub source: Option<String>,
}

/// A shell tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellTabState {
    pub cols: u16,
    pub rows: u16,
    /// Keystrokes reach the command.
    pub live: bool,
    pub exit_code: Option<i32>,
    /// Where the cursor is drawn, if it is.
    pub cursor: Option<CursorState>,
    /// The view is scrolled up into the history.
    pub scrolled_back: bool,
    /// The visible screen's rows as text, trailing blanks and blank rows at the bottom left
    /// out, at most [`ShellTabState::SCREEN_LIMIT`] (the bottom ones).
    pub screen: Vec<String>,
}

impl ShellTabState {
    /// The most screen rows `state` reports.
    pub const SCREEN_LIMIT: usize = 200;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
}

/// An open context menu or drop-down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MenuState {
    /// The element it opened on: `row-menu-<uid>` for a row's menu, `null` for another menu.
    pub owner: Option<String>,
    /// What a row's menu says above its entries (why nothing can open).
    pub note: Option<String>,
    pub entries: Vec<MenuEntryState>,
}

/// A menu entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MenuEntryState {
    pub label: String,
    pub enabled: bool,
    /// Why it is disabled, as the menu says under it.
    pub reason: Option<String>,
    /// A picker's entries (containers).
    pub submenu: Vec<SubmenuEntryState>,
}

/// An entry of a menu's submenu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubmenuEntryState {
    pub label: String,
    pub enabled: bool,
}

/// The state chip's word for a list's sync state.
pub fn sync_word(state: &SyncState, polled: bool) -> &'static str {
    match state {
        SyncState::Synced if polled => "polled",
        SyncState::Synced => "live",
        SyncState::Idle => "not watched",
        SyncState::Loading => "loading",
        SyncState::Reconnecting { .. } => "reconnecting",
        SyncState::Failed { .. } => "failed",
        SyncState::Unavailable => "unavailable",
    }
}

/// An error as `state` reports it: a sanitized one-line summary.
pub fn error_text(error: &str) -> String {
    safe_error_summary(error)
}

/// A failed connection's error as the screen shows it. Core already reduced the text that came
/// from kube or the kubeconfig ([`SafeErrorText`]); hiding quoted values again would hide what
/// Flint quotes itself, the exec plugin's name.
pub fn connection_error_text(error: &SafeErrorText) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn serializes_as_plain_json() {
        let main = serde_json::to_value(Main::Connected {
            version: "v1.33.1".into(),
        })
        .unwrap();
        assert_eq!(main, json!({"view": "connected", "version": "v1.33.1"}));
        assert_eq!(
            serde_json::to_value(Main::List).unwrap(),
            json!({"view": "list"})
        );
        assert_eq!(
            serde_json::to_value(Focus::ListFilter).unwrap(),
            json!("list_filter")
        );
        assert_eq!(
            serde_json::to_value(Banner::new("list-banner", Level::Warning, "x")).unwrap(),
            json!({"id": "list-banner", "level": "warning", "text": "x"})
        );
    }

    #[test]
    fn sync_words_match_the_chip() {
        assert_eq!(sync_word(&SyncState::Synced, false), "live");
        assert_eq!(sync_word(&SyncState::Synced, true), "polled");
        assert_eq!(
            sync_word(&SyncState::Failed { error: "x".into() }, false),
            "failed"
        );
    }
}
