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
    /// The command palette, while it is open.
    pub palette: Option<PaletteState>,
    /// Desktop notifications: the setting, what the OS answered and the latest alerts.
    pub alerts: Option<super::alert_center::AlertsState>,
    /// Cluster sets: the sidebar's choice, the saved sets and the running set.
    pub sets: ClusterSetsState,
    /// The Settings view and its draft, which outlives leaving the view.
    pub settings: SettingsState,
}

/// The Settings view (#739): whether it is shown, and its draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingsState {
    /// Whether the main area shows it.
    pub open: bool,
    pub section: super::settings_view::Section,
    /// Whether edits wait for Save (the sidebar entry's dot).
    pub dirty: bool,
    /// The `settings.toml` keys of the edited fields.
    pub edited: Vec<String>,
    /// The values shown: the file with the edits on top.
    pub appearance: &'static str,
    pub desktop_notifications: bool,
    pub watch_mode: &'static str,
    pub log_buffer_lines: u32,
    pub log_buffer_mib: u32,
    pub log_total_mib: u32,
    pub kubeconfig_paths: Vec<String>,
    pub path: Option<String>,
    pub helm: Option<String>,
    pub protect_local_clusters: bool,
    /// Why a text field's value cannot be saved, by the field's element id. Save is disabled
    /// while any is set.
    pub field_errors: std::collections::BTreeMap<&'static str, String>,
    /// Why the last kubeconfig file could not be added.
    pub kubeconfig_note: Option<String>,
    /// Thresholds and Cluster overrides, as shown.
    pub thresholds: ThresholdsState,
    /// The Network section's proxy.
    pub network: NetworkState,
    /// The file changed under the draft: the Reload / Save anyway banner.
    pub changed_on_disk: bool,
    /// `settings.toml` cannot be read; Save is disabled.
    pub invalid: bool,
    pub can_save: bool,
    /// A Save waits for the core's answer; its edits are still in the draft.
    pub saving: bool,
    /// Why the last Save did not go through.
    pub note: Option<String>,
    /// The element id of the control under the keyboard cursor.
    pub cursor: Option<String>,
}

/// Settings' Network section (#743). Never a proxy's password: the saved URL is masked, and
/// what is typed is not reported, only whether it can be applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkState {
    /// The saved proxy URL as the field shows it, credentials masked; empty for none.
    pub saved_url: String,
    pub saved_no_proxy: Vec<String>,
    /// The fields were typed in since they showed the file.
    pub typed: bool,
    /// Why what is typed cannot be applied.
    pub error: Option<String>,
    /// Apply is offered: what is typed is valid and not the saved proxy.
    pub can_apply: bool,
    /// An Apply waits for the core's answer.
    pub applying: bool,
    /// The last Apply, as the section says it.
    pub report: Vec<String>,
    /// The clusters the last Apply reconnected, once all came back or the wait ran out.
    pub reconnected: Option<Vec<String>>,
    /// The clusters the last Apply could not reconnect, with why.
    pub failed: Vec<(String, String)>,
}

/// Settings' Thresholds and Cluster overrides (#741), as shown: the file with the draft on top.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThresholdsState {
    /// The global value of each metric, by its `settings.toml` key; `null` for the built-in
    /// default.
    pub global: std::collections::BTreeMap<&'static str, Option<u32>>,
    /// The Cluster overrides rows, by context name.
    pub overrides: Vec<OverrideRowState>,
    /// Values typed that their setting does not accept; they block Save.
    pub rejected: Vec<RejectedState>,
    /// The clusters Add override offers, while its list is open.
    pub candidates: Option<Vec<String>>,
    /// The element id of the threshold whose text field is open.
    pub typing: Option<String>,
}

/// One row of Cluster overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverrideRowState {
    pub cluster: String,
    /// Whether the kubeconfig defines the context now.
    pub known: bool,
    /// Whether it has a server to bind thresholds and alerts to.
    pub bound: bool,
    /// Its own value of each metric, by key; `null` follows the global value.
    pub thresholds: std::collections::BTreeMap<&'static str, Option<u32>>,
    /// `default`, `on` or `off`.
    pub alerts: &'static str,
    /// `default`, `all` or `on_view`.
    pub watch_mode: &'static str,
}

/// A value typed that its setting does not accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RejectedState {
    /// The `settings.toml` path of the field.
    pub field: String,
    pub text: String,
    pub why: String,
}

/// Cluster sets, as the sidebar and the set's view show them.
#[derive(Debug, Clone, Serialize)]
pub struct ClusterSetsState {
    pub picking: bool,
    /// The chosen contexts, in the order they were chosen.
    pub picked: Vec<String>,
    /// The names of the saved sets.
    pub saved: Vec<String>,
    pub error: Option<String>,
    pub open: Option<OpenSetState>,
}

/// The running set and its combined list.
#[derive(Debug, Clone, Serialize)]
pub struct OpenSetState {
    pub label: String,
    pub members: Vec<SetMemberState>,
    /// One per cluster while grouped by cluster, else empty.
    pub groups: Vec<SetGroupState>,
    /// Saved members the core left out when the set opened.
    pub left_out: Vec<String>,
    pub list: ListState,
    /// The set's dashboard, while it is open in place of the list.
    pub dashboard: Option<SetDashboardState>,
}

/// The open dashboard of a cluster set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetDashboardState {
    /// A card per member, worst first.
    pub cards: Vec<SetCardState>,
    /// One count per problem category across the set.
    pub totals: Vec<SetTotalState>,
    /// Names the members a total lacks, when one does.
    pub partial: Option<String>,
}

/// One member's card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetCardState {
    pub cluster: String,
    /// The name the card shows.
    pub shown: String,
    pub tag: Option<flint_core::settings::ClusterTag>,
    /// `live`, `partial`, `waiting`, `unavailable` or `blocked`.
    pub state: &'static str,
    /// What the card says about its state, if anything.
    pub note: Option<String>,
    /// `healthy`, `warning` or `unknown`, from the member's counts, CPU and memory.
    pub level: &'static str,
    /// One per problem category, as the card's counts show them.
    pub counts: Vec<SetCountState>,
}

/// One problem count on a card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetCountState {
    /// As the count's link names it (`failing_pods`).
    pub category: &'static str,
    pub label: &'static str,
    /// `None` when it could not be read.
    pub count: Option<u32>,
    /// Whether clicking it opens a list.
    pub opens: bool,
}

/// One total across the set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SetTotalState {
    pub category: &'static str,
    pub label: &'static str,
    /// The number as shown (`3`, `2+`, or a dash).
    pub value: String,
    pub known: u32,
    /// Members the total could not include.
    pub missing: u32,
    /// Whether one list can show everything it counted.
    pub opens: bool,
}

/// A member of the running set.
#[derive(Debug, Clone, Serialize)]
pub struct SetMemberState {
    pub cluster: String,
    /// What the set's bar says is wrong with it (`Changed`, `Failed`, ...).
    pub note: Option<String>,
    pub tag: Option<flint_core::settings::ClusterTag>,
}

/// One cluster's section of a grouped combined list.
#[derive(Debug, Clone, Serialize)]
pub struct SetGroupState {
    pub cluster: String,
    pub count: usize,
    pub collapsed: bool,
}

/// Where keyboard focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Focus {
    /// Nothing inside the window's views (the workspace itself).
    Workspace,
    /// The cluster sidebar.
    Sidebar,
    /// The resource navigation.
    Navigation,
    /// The detail pane, outside its YAML editor.
    Detail,
    /// The cluster overview: its panel or one of its links.
    Overview,
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
    /// The command palette's search field.
    Palette,
    /// The Settings view.
    Settings,
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

/// The open command palette: what was typed, what it asks for, and the rows it lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaletteState {
    /// `actions` (Cmd-Shift-P) or `resources` (the resource switcher, Cmd-P).
    pub mode: &'static str,
    pub query: String,
    /// The action whose argument it asks for, by name (`logs::OpenSelectorLogs`), or `null`
    /// while it lists actions.
    pub action: Option<String>,
    /// The heading over the field while it asks for an argument: the action's label and what
    /// was picked so far (`Open Logs by Label Selector… · demo`).
    pub step: Option<String>,
    /// The argument asked for, by name (`selector`), with `step`.
    pub argument: Option<String>,
    /// The rows in order, at most [`PALETTE_ROWS`].
    pub rows: Vec<PaletteRowState>,
    /// How many rows there are in all.
    pub total: usize,
    /// The selected row's index, if any row is listed.
    pub selected: Option<usize>,
    /// The line under the rows: what the selected action does, or the refusal in `error`.
    pub footer: Option<String>,
    /// Why the last Enter did nothing, if it did nothing.
    pub error: Option<String>,
    /// A second line under the footer: in the resource switcher in On view mode, that only the
    /// kinds being watched list their objects.
    pub note: Option<String>,
}

/// At most this many palette rows are in the state.
pub const PALETTE_ROWS: usize = 200;

/// A palette row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaletteRowState {
    pub label: String,
    /// An action row's action, by name.
    pub action: Option<String>,
    /// The action's category, a choice's detail, `typed` for the text typed, or in the
    /// resource switcher an object's kind (`Pods`) or `Kind` for a kind.
    pub detail: Option<String>,
    /// The action's key, as the OS writes it (`⇧⌘P`).
    pub shortcut: Option<String>,
    /// In the resource switcher: the kind's id (`deployments.apps`).
    pub kind: Option<String>,
    /// In the resource switcher: the cluster, as the sidebar names it.
    pub cluster: Option<String>,
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
    /// `reconnecting`, `idle` (connected, watches stopped) or `failed`.
    pub presence: &'static str,
    /// Why the connection failed (a sanitized summary).
    pub error: Option<String>,
    /// `hidden`, `exposed` or `server_changed`.
    pub ai: &'static str,
    /// `protected`, `local` (a known local cluster), `unlocked` (for this session) or `trusted`
    /// (ADR-0015).
    pub protection: &'static str,
    /// A kubeconfig file can't be read, so writes and new shells are paused. Apart from
    /// `connection`: the cluster can stay connected and readable meanwhile.
    pub kubeconfig_unreadable: bool,
    /// The sidebar's keyboard cursor is on it (shown with the focus ring while the sidebar
    /// has focus).
    pub cursor: bool,
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
    /// Waiting for the kubeconfig to be confirmed. Not a failure, so no Retry button.
    AwaitingConfirmation,
    /// A resource list, and maybe the detail pane.
    List,
    /// The Settings view.
    Settings,
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
    /// The navigation's keyboard cursor is on its header (shown with the focus ring while the
    /// navigation has focus).
    pub cursor: bool,
    /// The entries shown: empty while the section is collapsed.
    pub entries: Vec<NavEntryState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NavEntryState {
    /// The kind id, as `open_view` takes it.
    pub kind: String,
    pub label: String,
    pub selected: bool,
    /// The navigation's keyboard cursor is on it.
    pub cursor: bool,
    /// The watch-state marker: `loading`, `reconnecting` or `failed`.
    pub marker: Option<&'static str>,
    /// With a cluster set open, how many members serve it (`2/3`), unless all of them do.
    pub badge: Option<String>,
    /// Objects with problems (`3`), `3+` with clusters unchecked, or `?` when none could be
    /// checked. Absent when every cluster was checked and has none.
    pub problems: Option<String>,
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
    /// The Problems toggle is on.
    pub problems_only: bool,
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
    /// The YAML edit's step (`editing`, `reviewing`, `reviewed`, `applying`, `conflict`), or
    /// null with no edit under way. Never its text.
    pub edit: Option<&'static str>,
    /// What the edit's last step said.
    pub edit_error: Option<String>,
    /// Why Edit, Review or Apply cannot run now.
    pub edit_blocked: Option<String>,
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
    /// `pod_logs`, `workload_logs`, `previous_logs`, `shell` or `agent_activity`.
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
    /// For agent activity tabs.
    pub activity: Option<ActivityTabState>,
    /// For compare tabs.
    pub compare: Option<CompareTabState>,
}

/// A compare tab: what it says of the object on each cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompareTabState {
    pub title: String,
    pub verdict: String,
    pub show_same: bool,
    pub include_volatile: bool,
    pub sides: Vec<CompareSideState>,
    /// Each row shown, as `path: value, value`.
    pub rows: Vec<String>,
}

/// One cluster's column in a compare tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompareSideState {
    pub name: String,
    pub status: String,
}

/// An agent activity tab (UP-16).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActivityTabState {
    /// Each call in one line, newest first.
    pub rows: Vec<String>,
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
    /// A log stream's container has not started; `reason` is what its tab says.
    Waiting { reason: String },
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
    /// An agent activity tab: it shows the activity log as it grows.
    Watching,
}

/// A log tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogTabState {
    /// Lines shown (what the buffer held when last delivered).
    pub lines: usize,
    /// Lines that arrived while paused.
    pub held_back: u64,
    /// Older lines were dropped to keep all log tabs within the shared memory budget.
    pub trimmed: bool,
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
    /// The filtered view (UP-48).
    pub filter: FilterState,
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

/// A log tab's filtered view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilterState {
    pub matching_only: bool,
    pub errors_only: bool,
    /// Lines shown around each line that passes.
    pub context: usize,
    /// Rows shown while filtered (`None`: every line is shown).
    pub rows: Option<usize>,
    /// The first scan of a new filter runs on the core's threads.
    pub filtering: bool,
}

/// A log tab's search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchState {
    pub query: String,
    pub matches: usize,
    /// The current match's position among them, from 1.
    pub current: Option<usize>,
    /// A scan runs on the core's threads (the first of the query, or a long catch-up): the
    /// matches are not known yet, and `matches` is 0.
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
    /// Streams that ended without an error (their container stopped).
    pub ended: usize,
    /// Containers not read: more than the session reads at once.
    pub capped: usize,
    /// Every stream, by pod name and container, at most [`StreamCounts::LIMIT`].
    pub each: Vec<StreamState>,
}

impl StreamCounts {
    /// The most streams listed; a session reads at most 32 at once.
    pub const LIMIT: usize = 64;
}

/// One stream of a workload's logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamState {
    pub pod: String,
    pub container: String,
    /// `connecting`, `streaming`, `reconnecting`, `ended` or `failed`.
    pub state: &'static str,
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
    /// A toggle's state (`Wrap Lines`, the tab list's shown tab); `null` for an entry that is
    /// not one, and in another menu for an entry it draws unchecked.
    pub checked: Option<bool>,
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
        s if s.is_checking_access() => "checking access",
        s if s.is_no_access() => "no access",
        SyncState::Synced if polled => "polled",
        SyncState::Synced => "live",
        SyncState::Idle => "not watched",
        SyncState::Loading => "loading",
        SyncState::Reconnecting { .. } | SyncState::RetryingRefusal { .. } => "reconnecting",
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
    use flint_core::RefusalCause;
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
        let retrying = |cause| SyncState::RetryingRefusal {
            error: "x".into(),
            cause,
        };
        let failed = |cause| SyncState::Failed {
            error: "x".into(),
            cause,
        };
        let denied = Some(RefusalCause::Denied);
        let missing = Some(RefusalCause::NotFound);
        assert_eq!(sync_word(&retrying(denied), false), "checking access");
        assert_eq!(sync_word(&failed(denied), false), "no access");
        assert_eq!(sync_word(&retrying(missing), false), "reconnecting");
        assert_eq!(sync_word(&failed(missing), false), "failed");
        assert_eq!(sync_word(&retrying(None), false), "reconnecting");
        assert_eq!(sync_word(&failed(None), false), "failed");
    }
}
