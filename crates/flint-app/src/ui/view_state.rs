//! The visible view tree as data: what a user would read off the window, for the test
//! automation channel's `state` command and for view tests. Built only with `--features
//! automation` (and in tests); release builds have no use for it.
//!
//! Each view fills in its own part (`*_state.rs` next to it). Text that could carry secrets
//! follows the screen: Secret cells are masked as the list masks them, Secret values are never
//! included (only which keys are revealed), and error messages go through
//! [`flint_core::redact::safe_error_summary`].

use flint_core::SyncState;
use flint_core::redact::safe_error_summary;
use serde::Serialize;

/// The whole window.
#[derive(Debug, Clone, Serialize)]
pub struct AppState {
    /// What has keyboard focus.
    pub focus: Focus,
    /// A dialog (such as the Expose to AI confirmation) is open over the window.
    pub dialog: bool,
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
    /// Something else, such as a dialog or a menu.
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

/// One kubeconfig context in the sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterState {
    pub name: String,
    pub active: bool,
    /// `disconnected`, `connecting`, `connected` or `failed`.
    pub connection: &'static str,
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
    /// The filter button's text ("All namespaces", "demo", "3 namespaces"), when shown.
    pub namespace_button: Option<String>,
    /// The watch mode button's text.
    pub watch_mode: String,
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
    pub namespace: Option<String>,
    pub name: String,
    pub selected: bool,
    /// Cell text as drawn (Secret cells masked), one per column.
    pub cells: Vec<String>,
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
    /// Secret keys whose value is revealed (never the values).
    pub revealed: Vec<String>,
    pub banners: Vec<Banner>,
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
