//! The action registry: every interactive control in the app as a named GPUI action, with a
//! label, a description, a category, typed arguments and default keys.
//!
//! Clicks, key bindings, the menu bar, the command palette and test automation all end in the
//! same handlers, through the same checks, so one list says everything a user can do and how.
//!
//! - **Declaring.** Every action type is defined here and listed once in `REGISTRY` with its
//!   [`ActionSpec`]: label, description, [`Category`], [`Visibility`], [`Arg`]s and keys.
//! - **Handling.** Views own the handlers. Each view lists them once in a `bind_actions` function
//!   generic over [`Bind`]; [`OnView`] registers them on the view's root, and the workspace
//!   registers them again through [`Forward`] (holding the view weakly), so an action works
//!   wherever focus is in the window.
//! - **Checking.** The workspace installs [`Guards`]: per action, whether it is enabled now and
//!   whether given arguments would do anything. [`dispatch`], [`perform`] and [`dispatch_in`]
//!   check them first and return an [`ActionError`] instead of dispatching a no-op. Handlers call
//!   the same check functions, so a key press, which bypasses the guards, behaves the same.
//! - **Clicking.** A control's click handler is [`on_click`] with its action: it goes through
//!   [`perform`], from the workspace's focus target, exactly as the palette or automation would.
//!   `scripts/check-click-actions.sh` fails on a click handler in `ui/` that does not.
//! - **Listing.** [`actions()`], [`find`], [`available`] / [`available_in`] and
//!   [`ActionSpec::shortcut`] (the live binding) are what a command palette needs.
//! - **Keys.** [`init`] binds every action's default keys; call it once at startup.
//!
//! The guards read the workspace entity, so [`dispatch`], [`perform`] and [`dispatch_in`] must
//! not be called while the workspace is being updated (from inside one of its listeners). Click
//! handlers built with [`on_click`] are plain closures, which is why they are safe.

use std::any::TypeId;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::LazyLock;

use gpui_kit::base::actions::{Cancel, Confirm};
use gpui_kit::component::input;
use gpui_kit::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

pub use super::alert_center::{
    InvestigateAlert, OpenAlert, SetClusterAlerts, ToggleDesktopNotifications,
};
use super::compare::COMPARE_VIEW;
pub use super::compare::{
    CompareObject, OpenCompare, RefreshCompare, ToggleCompareSame, ToggleCompareVolatile,
};
pub use super::confirm::ConfirmAlternative;
pub use super::dashboard::set::{OpenSetMember, OpenSetProblem};
use super::list::combined::{
    SelectCombinedRow, ShowAllCombinedRows, SortCombined, ToggleClusterFilter, ToggleClusterGroup,
    ToggleCombinedProblems, ToggleCombinedWide, ToggleGroupByCluster,
};
pub use super::pending_threshold::SetPendingThreshold;
pub use super::settings_logs_paths::{
    AddKubeconfigPath, ChooseKubeconfigPaths, EditHelmPath, EditLogBufferLines, EditLogBufferMib,
    EditLogTotalMib, EditPathOverride, EditProtectLocalClusters, RemoveKubeconfigPath,
};
pub use super::settings_network::{ApplyProxy, EditNoProxy, EditProxyUrl};
pub use super::settings_thresholds::{
    AddClusterOverride, CancelThresholdEntry, EditClusterAlerts, EditClusterWatchMode,
    EditThreshold, ResetClusterOverrides, TypeThreshold,
};
pub use super::settings_view::{
    ActivateSettingsControl, CloseSettings, EditAppearance, EditDesktopNotifications,
    EditWatchMode, NextSettingsControl, OpenSettings, PreviousSettingsControl, RevertSettings,
    SaveSettings, ShowSettingsSection,
};

actions!(
    flint,
    [
        /// Quit Flint.
        Quit,
        /// Close the front window, asking first while a session would end.
        CloseWindow,
        /// Follow the system light and dark mode (the `appearance` setting).
        AppearanceSystem,
        /// Always use Flint Light.
        AppearanceLight,
        /// Always use Flint Dark.
        AppearanceDark,
        /// Say which Flint this is: its version, in a dialog.
        About,
        /// Hide Flint's windows (macOS).
        Hide,
        /// Hide every other app's windows (macOS).
        HideOthers,
        /// Show every app's windows again (macOS).
        ShowAll,
        /// Minimize the front window.
        MinimizeWindow,
        /// Zoom the front window, or return it to its earlier size.
        ZoomWindow,
        /// Open Flint's project page in the browser.
        OpenHelp
    ]
);
actions!(
    workspace,
    [
        /// Open the active cluster's overview.
        OpenClusterOverview
    ]
);
actions!(
    palette,
    [
        /// Open the command palette, or close it when it is open.
        ToggleCommandPalette,
        /// Open the resource switcher (the palette over the active cluster's kinds and the
        /// objects in memory), or close it when it is open.
        ToggleResourceSwitcher,
        /// Move the palette's selection up.
        PaletteUp,
        /// Move the palette's selection down.
        PaletteDown,
        /// Run the palette's selected entry (or go on to the argument it needs).
        PaletteConfirm,
        /// Close the palette, giving focus back to where it was; while it asks for an argument, go
        /// back a step instead.
        PaletteDismiss,
        /// Go back a step (Backspace in the empty search field while the palette asks for an
        /// argument); otherwise the field deletes as usual.
        PaletteBack
    ]
);
actions!(
    workspace,
    [
        /// Connect (and discover) the active cluster again after a failure.
        RetryConnection,
        /// Stop the active cluster's connection attempt and its automatic retries.
        CancelConnection,
        /// Clear the namespace filter.
        ShowAllNamespaces,
        /// Open the namespace filter's picker, with focus inside it.
        OpenNamespaceFilter,
        /// Move focus to the namespace picker's next control, wrapping around.
        NamespacePickerNext,
        /// Move focus to the namespace picker's previous control, wrapping around.
        NamespacePickerPrevious,
        /// Watch every kind on the active cluster.
        WatchAllKinds,
        /// Watch the kinds of open views and the problem kinds, on the active cluster.
        WatchOpenViewsOnly,
        /// Let the active cluster follow the default watch mode.
        UseDefaultWatchMode,
        /// Make "all kinds" the default watch mode.
        DefaultWatchAllKinds,
        /// Make "on view" the default watch mode.
        DefaultWatchOpenViewsOnly,
        /// Go back to the kind and object shown before.
        GoBack,
        /// In a narrow window, return from the detail to the list, keeping the selection.
        BackToList,
        /// Move focus to the next pane: clusters, navigation, list, detail, dock.
        FocusNextPane,
        /// Move focus to the previous pane.
        FocusPreviousPane
    ]
);
actions!(
    sidebar,
    [
        /// Move the cluster sidebar's selection up.
        SelectPreviousCluster,
        /// Move the cluster sidebar's selection down.
        SelectNextCluster,
        /// Make the cluster selected in the sidebar the active one, and connect to it.
        ActivateSelectedCluster
    ]
);
actions!(
    navigation,
    [
        /// Move the navigation's selection up.
        SelectPreviousKind,
        /// Move the navigation's selection down.
        SelectNextKind,
        /// Open the kind selected in the navigation, or open or close the selected section.
        OpenSelectedKind,
        /// Open the selected section of the navigation.
        ExpandSection,
        /// Close the selected section, or the section of the selected kind (and select the
        /// section).
        CollapseSection
    ]
);
actions!(
    resource_list,
    [
        SelectPrev,
        SelectNext,
        SelectFirst,
        SelectLast,
        /// Clear the selection (Esc in the list).
        CloseDetail,
        /// Show or hide the extra `-o wide` columns.
        ToggleWide,
        /// Show only the rows with problems.
        ToggleProblemsOnly,
        /// Move focus to the list's text filter.
        FocusFilter,
        /// Move focus from the list's text filter back to its rows (Esc or Enter there).
        FocusRows,
        /// Down in the list's text filter: back to the rows, selecting the first if none is.
        SelectFromFilter,
        /// Enter on the list's rows: open the selected row, as clicking it does.
        OpenSelectedRow
    ]
);
actions!(
    detail,
    [
        /// Fetch the shown object again.
        Refresh,
        /// Close the detail pane.
        Close,
        ShowOverview,
        ShowYaml,
        ToggleManagedFields,
        ToggleLabels,
        ToggleAnnotations,
        /// Edit the object's YAML.
        EditYaml,
        /// Run a server dry run of the edit and show its diff.
        ReviewYamlEdit,
        /// Apply the reviewed edit, after a confirmation.
        ApplyYamlEdit,
        /// Reload an object that changed, keep the edit, and review it again.
        ReloadYamlEdit,
        /// Go back from the diff to the editor.
        BackToYamlEdit,
        /// Discard the edit.
        CancelYamlEdit
    ]
);

actions!(
    detail,
    [
        /// Open the logs of the selected pod (its default container) or workload (every pod,
        /// merged) in the dock.
        OpenLogs,
        /// Open the logs of the previous instance of the selected pod's default container.
        OpenPreviousLogs
    ]
);
actions!(
    detail,
    [
        /// Open a shell in the default container of the selected pod, or of the selected
        /// workload's first ready pod, in the dock.
        OpenShell,
        /// Forward a local port to the selected pod or service, from the port-forward panel.
        ForwardPort
    ]
);
actions!(
    port_forward,
    [
        /// Show the active cluster's port-forward panel.
        ShowPortForwards,
        /// Start the forward the panel's form describes.
        StartPortForward,
        /// Stop every port-forward in this window.
        StopAllPortForwards
    ]
);

/// Stop one port-forward, or dismiss its row once it has ended.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = port_forward, no_json)]
#[serde(deny_unknown_fields)]
pub struct StopPortForward {
    pub id: u64,
}

/// Open a running port-forward's local address in the browser.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = port_forward, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenForwardedPort {
    pub id: u64,
}
actions!(
    workspace,
    [
        /// Delete the selected object, after a review and a confirmation.
        DeleteObject,
        /// Ask for a replica count for the selected workload, then review and confirm it.
        ScaleObject,
        /// Restart the selected workload's pods, after a review and a confirmation.
        RestartObject,
        /// Stop new pods landing on the selected node, after a review and a confirmation.
        CordonNode,
        /// Let new pods land on the selected node again, after a review and a confirmation.
        UncordonNode,
        /// Ask what a drain of the selected node may evict, then review and confirm it.
        DrainNode,
        /// Open the report of the latest drain that ended.
        ShowDrainReport
    ]
);
actions!(
    terminal,
    [
        /// Copy the text selected in the shell tab.
        CopyText,
        /// Paste the clipboard's text into the shell tab, bracketed when the program asked.
        PasteText,
        /// Scroll the shell tab up one screen into its history.
        ScrollPageUp,
        /// Scroll the shell tab down one screen.
        ScrollPageDown,
        /// Scroll the shell tab to the oldest line kept.
        ScrollToTop,
        /// Scroll the shell tab back to the bottom, where new output appears.
        ScrollToBottom
    ]
);
actions!(
    dock,
    [
        /// Show or hide the bottom dock.
        ToggleDock,
        /// Close the dock's active tab, ending its session.
        CloseActiveTab,
        /// List every dock tab, with its cluster and status, to show one.
        ShowTabList,
        /// Show the dock's next tab.
        NextTab,
        /// Show the dock's previous tab.
        PrevTab,
        /// Open the active log tab's stream again once it ended or failed, in a new tab beside it.
        ReconnectLogs,
        /// Start a new shell once the active shell tab's exited or failed, in a new tab beside it.
        RestartShell
    ]
);
actions!(
    logs,
    [
        /// Pause following (new lines are held back), or resume it.
        TogglePause,
        /// Scroll to the newest line and keep following.
        JumpToEnd,
        /// Scroll to the oldest line shown.
        JumpToStart,
        /// Show or hide each line's timestamp.
        ToggleTimestamps,
        /// Wrap long lines, or cut them at the edge.
        ToggleWrap,
        /// Show or hide a workload line's pod and container.
        ToggleSources,
        /// Show only the lines matching the search (with context), or every line.
        ToggleMatchingOnly,
        /// Show only warnings and errors, or every line.
        ToggleErrorsOnly,
        /// Read the log search as a regular expression, or as plain text.
        ToggleLogRegex,
        /// Move focus to the log search field.
        FocusSearch,
        /// Go to the next line matching the search.
        NextMatch,
        /// Go to the previous line matching the search.
        PrevMatch,
        /// Select the line above (or the newest line).
        SelectPrevLine,
        /// Select the line below (or the newest line).
        SelectNextLine,
        /// Extend the selection one line up.
        ExtendSelectionUp,
        /// Extend the selection one line down.
        ExtendSelectionDown,
        /// Select every line shown.
        SelectAllLines,
        /// Clear the line selection.
        ClearSelection,
        /// Copy the selected lines as shown.
        CopyLines,
        /// Save every line shown to a file.
        SaveLogs,
        /// Save the lines matching the search to a file.
        SaveMatchingLines,
        /// Save the selected lines to a file.
        SaveSelectedLines,
        /// Esc in the log search field: clear the search, or (when empty) go back to the lines.
        LeaveSearch
    ]
);

/// Make a cluster (kubeconfig context) the active one.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectCluster {
    pub cluster: String,
}

/// Ask to expose a cluster to AI agents: opens the confirmation naming its server.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ExposeToAi {
    pub cluster: String,
}

/// Turn keeping a cluster watched in the background on or off. This never lets a sign-in command
/// run: that is the cluster's own toggle, which is deliberately not a registered action.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleKeepWatching {
    pub cluster: String,
}

/// Stop everything Flint runs for a cluster until it is selected again. Asks first while it would
/// end a live shell.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct DisconnectCluster {
    pub cluster: String,
}

/// Hide a cluster from AI agents, or remove a stale exposure.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct HideFromAi {
    pub cluster: String,
}

/// Change which namespaces and kinds AI agents may read of an exposed cluster: opens the
/// dialog with its pickers (ADR-0013).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct EditAiScope {
    pub cluster: String,
}

/// Show what AI agents asked Flint's MCP server for on a cluster: its agent activity, in a dock
/// tab that updates while they work (UP-16).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ShowAgentActivity {
    pub cluster: String,
}

/// Show a cluster's Helm releases, read-only, in a dock tab: details, history, values with
/// secrets masked, and the manifest.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ShowHelmReleases {
    pub cluster: String,
}

/// Select a release in the Helm releases tab shown, as clicking it does.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectHelmRelease {
    pub namespace: String,
    pub name: String,
}

actions!(
    workspace,
    [
        /// Read the releases in the Helm releases tab again.
        RefreshHelmReleases,
        /// Show the selected Helm release's details.
        ShowHelmDetails,
        /// Show the selected Helm release's history.
        ShowHelmHistory,
        /// Show the selected Helm release's values, with secrets masked.
        ShowHelmValues,
        /// Show the selected Helm release's manifest, with secrets masked.
        ShowHelmManifest,
    ]
);

actions!(
    ai_scope,
    [
        /// In the AI scope pickers: let agents read every namespace.
        ScopeAllNamespaces,
        /// In the AI scope pickers: let agents read every kind.
        ScopeAllKinds,
        /// In the AI scope pickers: let agents read only the namespaces selected.
        ScopeSelectedNamespaces,
        /// In the AI scope pickers: let agents read only the kinds selected.
        ScopeSelectedKinds
    ]
);

/// In the AI scope pickers: add a namespace to the scope, or take it out.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = ai_scope, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleScopeNamespace {
    pub namespace: String,
}

/// In the AI scope pickers: add a kind (`pods`, `deployments.apps`) to the scope, or take it out.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = ai_scope, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleScopeKind {
    pub kind: String,
}

/// Ask to unlock a protected cluster until Flint quits: opens the confirmation naming its
/// server (ADR-0015).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct UnlockCluster {
    pub cluster: String,
}

/// What a drain may also evict, beyond what it evicts by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainOption {
    /// Pods with `emptyDir` volumes, whose data is lost.
    #[default]
    DeleteEmptydirData,
    /// Pods without a controller, which nothing recreates.
    Force,
}

/// In the drain dialog: turn one of its options on or off.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = resource_actions, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleDrainOption {
    pub option: DrainOption,
}

/// In a fleet action's target list: take a cluster out of the run, or put it back.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleFleetTarget {
    pub cluster: String,
}

actions!(
    cluster_sets,
    [
        /// In a fleet apply's target list: go back to the manifest, as typed, to edit and
        /// review it again.
        EditFleetManifestAgain
    ]
);

/// Choose the name Flint shows for a cluster: opens a dialog naming its context and server.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct RenameCluster {
    pub cluster: String,
}

/// Set a cluster's colour tag, for the server it points at now. `endpoint`, from a menu drawn for
/// that server, refuses the choice if the context has moved since.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetClusterTag {
    pub cluster: String,
    pub tag: flint_core::settings::ClusterTag,
    #[serde(default)]
    pub endpoint: Option<u64>,
}

/// Remove a cluster's colour tag at the server it points at now. `endpoint` as in
/// [`SetClusterTag`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ClearClusterTag {
    pub cluster: String,
    #[serde(default)]
    pub endpoint: Option<u64>,
}

/// Lock a cluster unlocked for this session again (Lock Again).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct LockCluster {
    pub cluster: String,
}

/// Ask to trust a cluster, keeping it unlocked across restarts: opens the confirmation naming
/// its server (ADR-0015).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct TrustCluster {
    pub cluster: String,
}

/// Stop trusting a cluster, or remove a stale trust, and lock it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct UntrustCluster {
    pub cluster: String,
}

/// Open the list of a kind on the active cluster. `kind` is matched like `FLINT_VIEW`: `pods`,
/// `deployments`, `deployments.apps`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenView {
    pub kind: String,
    /// Only if this is still the active cluster (its context name), as the resource switcher
    /// listed it; refused otherwise.
    #[serde(default)]
    pub cluster: Option<String>,
    /// With `cluster`: and its context still points at this API server.
    #[serde(default)]
    pub server: Option<String>,
}

/// Open the list of a kind on the active cluster with one of its objects selected, and its
/// details shown. `kind` is matched as for [`OpenView`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenObject {
    pub kind: String,
    #[serde(default)]
    pub namespace: Option<String>,
    pub name: String,
    /// Only if this is still the active cluster, as for [`OpenView`].
    #[serde(default)]
    pub cluster: Option<String>,
    #[serde(default)]
    pub server: Option<String>,
}

/// Open or close a navigation section, by its title.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleNavSection {
    pub section: String,
}

/// Replace the active cluster's namespace filter (empty: all namespaces).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetNamespaces {
    pub namespaces: Vec<String>,
}

/// Add a namespace to the filter, or remove it if it is in it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleNamespace {
    pub namespace: String,
}

/// Add a namespace to the filter, by name (also one the user may not list).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct AddNamespace {
    pub namespace: String,
}

/// A resizable pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneName {
    #[default]
    Clusters,
    Navigation,
    Detail,
}

/// Set a pane's width in points, as dragging its edge does (with the same limits).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetPaneWidth {
    pub pane: PaneName,
    pub width: f32,
}

actions!(
    cluster_sets,
    [
        /// Start or stop choosing several clusters in the sidebar.
        ToggleClusterPicking,
        /// Run the clusters chosen in the sidebar as a set and open their combined list.
        OpenPickedClusters,
        /// Stop the running set and close its combined list.
        CloseClusterSet,
        /// Show a card per cluster of the running set, worst first.
        OpenClusterSetDashboard,
        /// Go back from the set's dashboard to its combined list.
        ShowClusterSetList
    ]
);
actions!(
    cluster_sets,
    [
        /// Restart the selected workload on every member of the running set, one at a time,
        /// after a per-member plan and one confirmation.
        RestartAcrossSet,
        /// Ask for a replica count, then scale the selected workload on every member of the
        /// running set, one at a time, after a per-member plan and one confirmation.
        ScaleAcrossSet,
        /// Edit a manifest of the selected object, then apply it on every member of the running
        /// set, one at a time, after a per-member dry run and one confirmation.
        ApplyAcrossSet,
        /// Delete the selected object on every member of the running set, one at a time, after
        /// a per-member dry run and one confirmation.
        DeleteAcrossSet
    ]
);

/// Add a cluster to the sidebar's choice, or take it out.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct TogglePickedCluster {
    pub cluster: String,
}

/// Save the clusters chosen in the sidebar under a name.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct SaveClusterSet {
    pub name: String,
}

/// Run a saved set and open its combined list.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenClusterSet {
    pub name: String,
}

/// Ask for a new name for a saved set.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct RenameClusterSet {
    pub name: String,
}

/// Ask to delete a saved set.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct DeleteClusterSet {
    pub name: String,
}

/// Select the row of an object in the open list, as clicking it does.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = resource_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectRow {
    #[serde(default)]
    pub namespace: Option<String>,
    pub name: String,
}

/// Sort by a column, as clicking its header does: ascending, descending, then unsorted. The
/// choice is remembered for the cluster and kind.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = resource_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct SortBy {
    /// A drawn column's header (`Restarts`): the first drawn column with it is sorted.
    pub column: String,
    /// The column's key instead (`server:Restarts`, `flint:age`), as a header click sends it,
    /// so a header shared by two columns sorts the one clicked. Never matched against headers,
    /// nor `column` against keys.
    #[serde(default)]
    pub key: Option<String>,
}

/// Reveal or hide one value of a Secret, by key.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = detail, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleSecretValue {
    pub key: String,
}

/// Open the logs of one container of the shown pod, or of its previous instance.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = detail, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenContainerLogs {
    pub container: String,
    #[serde(default)]
    pub previous: bool,
}

/// Open a shell in one container of the shown pod.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = detail, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenContainerShell {
    pub container: String,
}

/// Open the merged logs of the pods matching a label selector, on the active cluster.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = logs, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenSelectorLogs {
    pub namespace: String,
    pub selector: String,
}

/// Select the text of a shell tab's screen from one cell to another (inclusive), as dragging
/// over them does. Rows and columns count from the top-left cell, 0 and up.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = terminal, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectText {
    pub start_row: u16,
    pub start_col: u16,
    pub end_row: u16,
    pub end_col: u16,
}

/// Set the bottom dock's height in points, as dragging its top edge does (same limits).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetDockHeight {
    pub height: f32,
}

/// Show a dock tab, by its id, as clicking it does.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = dock, no_json)]
#[serde(deny_unknown_fields)]
pub struct ActivateTab {
    pub tab: u64,
}

/// Close a dock tab, by its id, as its close button does.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = dock, no_json)]
#[serde(deny_unknown_fields)]
pub struct CloseTab {
    pub tab: u64,
}

/// Select a log line by its id, or extend the selection to it (Shift-click).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = logs, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectLine {
    pub line: u64,
    #[serde(default)]
    pub extend: bool,
}

/// Extend a selection being dragged with the mouse to a line.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = logs, no_json)]
#[serde(deny_unknown_fields)]
pub struct DragSelection {
    pub line: u64,
}

/// Which log lines to save.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveScope {
    /// Every line shown.
    #[default]
    All,
    /// The lines matching the search.
    Matches,
    /// The selected lines.
    Selection,
}

/// Save log lines to a file, as shown and not redacted, with or without timestamps (by default
/// as the timestamp toggle shows them).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = logs, no_json)]
#[serde(deny_unknown_fields)]
pub struct SaveLines {
    pub lines: SaveScope,
    #[serde(default)]
    pub timestamps: Option<bool>,
}

/// How many lines to show before and after each line a filtered log tab shows (0 to 100), as
/// `grep -C` does.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = logs, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetLogContext {
    pub lines: u32,
}

/// Run the palette entry shown in row `row` (a click on it). `rows` is the version of the
/// rows the click was drawn on: when they have changed since (a live refresh reordered them),
/// the click is dropped rather than pick another entry. Without it, the rows shown now.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = palette, no_json)]
#[serde(deny_unknown_fields)]
pub struct PalettePick {
    pub row: usize,
    #[serde(default)]
    pub rows: Option<u64>,
}

/// Which part of the app handles an action, and so where it is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Always available (application menu).
    App,
    /// The main window.
    Workspace,
    /// The cluster sidebar.
    Sidebar,
    /// The resource navigation of the active cluster.
    Navigation,
    /// The open resource list.
    List,
    /// Choosing, saving and opening a set of clusters.
    ClusterSets,
    /// The open list over several clusters.
    CombinedList,
    /// The detail pane.
    Detail,
    /// The open cluster overview.
    Dashboard,
    /// The Settings view and its draft (opening it, too).
    Settings,
    /// An open dialog (the Expose to AI confirmation).
    Dialog,
    /// The bottom dock and its tabs.
    Dock,
    /// The log tab shown in the dock.
    Logs,
    /// The shell tab shown in the dock.
    Terminal,
    /// The compare tab shown in the dock.
    Compare,
    /// The open command palette.
    Palette,
    /// The focused text field (gpui-component's own actions: Cut, Copy, Paste, Select All).
    TextField,
}

/// What a palette groups an action under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Flint,
    Cluster,
    Namespace,
    View,
    List,
    Detail,
    Logs,
    Terminal,
}

impl Category {
    /// As the palette shows it beside an action's label.
    pub fn name(self) -> &'static str {
        match self {
            Category::Flint => "Flint",
            Category::Cluster => "Cluster",
            Category::Namespace => "Namespace",
            Category::View => "View",
            Category::List => "List",
            Category::Detail => "Detail",
            Category::Logs => "Logs",
            Category::Terminal => "Terminal",
        }
    }
}

/// Whether a palette offers an action. Hidden ones exist for automation, or for controls that
/// make little sense typed (a pane width, a column header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Palette,
    Hidden,
}

/// What an argument takes, so a palette can offer a picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    Text,
    Number,
    Bool,
    /// A list of strings.
    List,
    /// One of these words.
    Choice(&'static [&'static str]),
    /// A kubeconfig context name.
    Cluster,
    /// A namespace of the active cluster.
    Namespace,
    /// A kind id of the active cluster (`pods`, `deployments.apps`).
    Kind,
    /// A column of the open list.
    Column,
    /// A container of the pod the detail pane shows.
    Container,
}

/// One argument of an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
    pub kind: ArgKind,
}

/// Why an action could not be built or dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// No action has this name.
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
    Unknown(String),
    /// The arguments do not fit the action.
    InvalidArgs { action: String, message: String },
    /// Nothing in the window handles the action now, or it is disabled (no list is open, no
    /// cluster is active).
    NotAvailable(String),
    /// The action is available, but these arguments would do nothing (no such cluster, row or
    /// column).
    Rejected { action: String, reason: String },
}

impl ActionError {
    /// A [`ActionError::Rejected`] for action `A`.
    pub fn rejected<A: Action>(reason: impl Into<String>) -> Self {
        ActionError::Rejected {
            action: A::name_for_type().to_owned(),
            reason: reason.into(),
        }
    }

    /// A [`ActionError::NotAvailable`] for action `A`.
    pub fn unavailable<A: Action>() -> Self {
        ActionError::NotAvailable(A::name_for_type().to_owned())
    }
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::Unknown(name) => write!(f, "unknown action \"{name}\""),
            ActionError::InvalidArgs { action, message } => {
                write!(f, "invalid arguments for {action}: {message}")
            }
            ActionError::NotAvailable(name) => {
                write!(f, "{name} is not available in the current view")
            }
            ActionError::Rejected { action, reason } => write!(f, "{action}: {reason}"),
        }
    }
}

impl std::error::Error for ActionError {}

impl ActionError {
    /// The refusal as one sentence for the person who clicked: never an internal action name.
    pub fn for_person(&self) -> String {
        let sentence = match self {
            ActionError::NotAvailable(_) => "That is not available in the current view".to_owned(),
            ActionError::Rejected { reason, .. } => reason.clone(),
            ActionError::InvalidArgs { message, .. } => message.clone(),
            ActionError::Unknown(_) => "That is not available".to_owned(),
        };
        let mut chars = sentence.chars();
        let first = chars.next().map(|c| c.to_uppercase().collect::<String>());
        let text = first
            .into_iter()
            .chain(chars.map(String::from))
            .collect::<String>();
        if text.ends_with('.') {
            text
        } else {
            format!("{text}.")
        }
    }
}

/// An action the app offers: what it is called, what it does, and how to make one.
pub struct ActionSpec {
    /// The GPUI action name, `namespace::Name`. Stable: automation and key maps refer to it.
    pub name: &'static str,
    /// A Title Case name for menus and the palette; it ends in "…" when it takes arguments.
    pub label: &'static str,
    pub description: &'static str,
    pub category: Category,
    pub scope: Scope,
    pub visibility: Visibility,
    pub args: &'static [Arg],
    /// Default keys, in GPUI keystroke syntax ("secondary-q", "down"), and the key context they
    /// apply in (`None`: everywhere). `secondary` is Cmd on macOS and Ctrl on Linux.
    pub keys: &'static [&'static str],
    pub key_context: Option<&'static str>,
    type_id: TypeId,
    build: fn(Value) -> Result<Box<dyn Action>, String>,
    probe: fn() -> Box<dyn Action>,
}

impl fmt::Debug for ActionSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionSpec")
            .field("name", &self.name)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl ActionSpec {
    /// Whether the action needs arguments (a palette asks for them first).
    pub fn takes_args(&self) -> bool {
        !self.args.is_empty()
    }

    /// Builds the action from its arguments: a JSON object, or `null` for none.
    pub fn build(&self, args: Value) -> Result<Box<dyn Action>, ActionError> {
        (self.build)(args).map_err(|message| ActionError::InvalidArgs {
            action: self.name.to_owned(),
            message,
        })
    }

    /// Whether `action` is this kind of action.
    pub fn matches(&self, action: &dyn Action) -> bool {
        action.as_any().type_id() == self.type_id
    }

    /// Whether the action is enabled now, whatever its arguments: its guard, if the workspace
    /// installed one (a list action with no list open is not).
    pub fn is_enabled(&self, cx: &App) -> bool {
        guard(self.type_id, cx).is_none_or(|g| (g.enabled)(cx))
    }

    /// Whether these arguments would do something now; `Rejected` says why not.
    pub fn validate(&self, action: &dyn Action, cx: &App) -> Result<(), ActionError> {
        match guard(self.type_id, cx) {
            Some(g) if !(g.enabled)(cx) => Err(ActionError::NotAvailable(self.name.to_owned())),
            Some(g) => (g.validate)(action, cx),
            None => Ok(()),
        }
    }

    /// Whether the action would be handled from the focused element up, and is enabled.
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
    pub fn is_available(&self, window: &Window, cx: &App) -> bool {
        let handled = match self.scope {
            Scope::App => true,
            _ => window.is_action_available(&*(self.probe)(), cx),
        };
        handled && self.is_enabled(cx)
    }

    /// Like [`ActionSpec::is_available`], from `focus` instead of the focused element. A
    /// palette that holds focus itself asks from the element focused before it opened.
    pub fn is_available_in(&self, focus: &FocusHandle, window: &Window, cx: &App) -> bool {
        let handled = match self.scope {
            Scope::App => true,
            _ => window.is_action_available_in(&*(self.probe)(), focus),
        };
        handled && self.is_enabled(cx)
    }

    /// The live key binding to show next to the action ("⌘F", [`keystroke_hint`]), if any: of
    /// those for its own keys, the one added last to the keymap (the highest precedence), so
    /// later or user bindings win over the defaults. Fallback and per-context extras (Close
    /// Window's Cmd-W, a shell tab's Ctrl+Shift+W) are not shown.
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "automation lists every action's shortcut")
    )]
    pub fn shortcut(&self, cx: &App) -> Option<String> {
        let action = (self.probe)();
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        self.own_hint(keymap.bindings_for_action(&*action), cx)
    }

    /// [`ActionSpec::shortcut`] as it works from `focus`, whose key contexts are `contexts`
    /// (outermost first, as [`Window::context_stack`] gives them while it has focus): the
    /// action's own binding that fires there. A key bound to something else in a deeper context
    /// takes it only while that other action would run: Cmd-R in a log tab is Refresh Object
    /// while the tab has nothing to reconnect. In a log tab, Copy's Cmd-C (a text field's) is not
    /// shown, since its context does not apply.
    pub fn shortcut_in(
        &self,
        contexts: &[KeyContext],
        focus: &FocusHandle,
        window: &Window,
        cx: &App,
    ) -> Option<String> {
        let action = (self.probe)();
        let own_context = self
            .key_context
            .map(|c| KeyBindingContextPredicate::parse(c).expect("static key context parses"));
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let own: Vec<KeyBinding> = keymap
            .bindings_for_action(&*action)
            .filter(|b| b.predicate().as_deref() == own_context.as_ref())
            .cloned()
            .collect();
        let is_own = |b: &KeyBinding| {
            b.action().partial_eq(&*action) && b.predicate().as_deref() == own_context.as_ref()
        };
        // The latest added first: it wins over the defaults. Of the bindings for its keys
        // there, in precedence order, the first that would take the key decides.
        let fires = own.iter().rev().find(|binding| {
            let (candidates, _) = keymap.bindings_for_input(binding.keystrokes(), contexts);
            candidates
                .iter()
                .find(|c| is_own(c) || would_run(c.action(), focus, window, cx))
                .is_some_and(&is_own)
        })?;
        self.own_hint(std::iter::once(fires), cx)
    }

    /// Whether one of the action's own keys fires it from `focus` ([`ActionSpec::shortcut_in`]).
    pub fn bound_in(
        &self,
        contexts: &[KeyContext],
        focus: &FocusHandle,
        window: &Window,
        cx: &App,
    ) -> bool {
        self.shortcut_in(contexts, focus, window, cx).is_some()
    }

    /// The last of `bindings` that is one of the action's own keys, as the OS writes it.
    fn own_hint<'a>(
        &self,
        bindings: impl DoubleEndedIterator<Item = &'a KeyBinding>,
        cx: &App,
    ) -> Option<String> {
        let os = cx.platform_info().os;
        let own: Vec<String> = self.keys.iter().map(|k| key_hint_for(k, os)).collect();
        bindings
            .map(|binding| {
                binding
                    .keystrokes()
                    .iter()
                    .map(|k| keystroke_hint(k.modifiers(), k.key(), os))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .rfind(|hint| own.contains(hint))
    }
}

/// Whether `action`, bound to a key, would take it from `focus`: a key bound to nothing (a shell
/// tab's own keys) always does; a registered action while it is available; another while
/// something there handles it.
fn would_run(action: &dyn Action, focus: &FocusHandle, window: &Window, cx: &App) -> bool {
    if action.as_any().is::<NoAction>() {
        return true;
    }
    match spec_of(action) {
        Some(spec) => spec.is_available_in(focus, window, cx),
        None => window.is_action_available_in(action, focus),
    }
}

/// A chord in GPUI syntax (`secondary-shift-g`) written the way this OS shows it: "⇧⌘G" on
/// macOS, "Ctrl+Shift+G" on Linux ([`keystroke_hint`]). For key hints in tooltips and notes.
pub fn key_hint(keys: &str) -> String {
    let os = if cfg!(target_os = "macos") {
        OperatingSystem::Mac
    } else {
        OperatingSystem::Linux
    };
    key_hint_for(keys, os)
}

/// [`key_hint`] for `os`.
fn key_hint_for(keys: &str, os: OperatingSystem) -> String {
    match Keystroke::parse_with_os(keys, os) {
        Ok(keystroke) => keystroke_hint(&keystroke.modifiers, &keystroke.key, os),
        Err(_) => keys.to_owned(),
    }
}

/// A keystroke as the OS's own menus write it. macOS: the modifier glyphs in Apple's order
/// (⌃⌥⇧⌘) and glyphs for named keys (⇞ ⇟ ↖ ↘ ␣ ⏎ ⎋ ⌫ ⌦ ⇥ and the arrows), with nothing between
/// them. Linux: modifier and key names joined by "+" (`Ctrl+Shift+PageUp`).
pub fn keystroke_hint(modifiers: &Modifiers, key: &str, os: OperatingSystem) -> String {
    let mac = os == OperatingSystem::Mac;
    let mut parts: Vec<&str> = Vec::new();
    for (on, glyph, name) in [
        (modifiers.control, "⌃", "Ctrl"),
        (modifiers.alt, "⌥", "Alt"),
        (modifiers.shift, "⇧", "Shift"),
        (modifiers.platform, "⌘", "Super"),
        (modifiers.function, "fn", "Fn"),
    ] {
        if on {
            parts.push(if mac { glyph } else { name });
        }
    }
    let named = match (key, mac) {
        ("pageup", true) => Some("⇞"),
        ("pagedown", true) => Some("⇟"),
        ("home", true) => Some("↖"),
        ("end", true) => Some("↘"),
        ("space", true) => Some("␣"),
        ("enter", true) => Some("⏎"),
        ("escape", true) => Some("⎋"),
        ("backspace", true) => Some("⌫"),
        ("delete", true) => Some("⌦"),
        ("tab", true) => Some("⇥"),
        ("up", true) => Some("↑"),
        ("down", true) => Some("↓"),
        ("left", true) => Some("←"),
        ("right", true) => Some("→"),
        ("pageup", false) => Some("PageUp"),
        ("pagedown", false) => Some("PageDown"),
        ("escape", false) => Some("Esc"),
        _ => None,
    };
    let key = match named {
        Some(name) => name.to_owned(),
        None => {
            // A letter or symbol in capitals (as on the keycap); a named key capitalized
            // (`Home`, `Enter`, `F5`).
            let mut chars = key.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    };
    parts.push(&key);
    parts.join(if mac { "" } else { "+" })
}

/// Every action, in menu order.
#[cfg_attr(
    not(any(test, feature = "automation")),
    expect(dead_code, reason = "used by the command palette (#29) and automation")
)]
pub fn actions() -> &'static [ActionSpec] {
    &REGISTRY
}

/// The action named `name` (`workspace::OpenView`).
pub fn find(name: &str) -> Option<&'static ActionSpec> {
    REGISTRY.iter().find(|spec| spec.name == name)
}

/// The label of action `A`, for a menu item.
pub fn label<A: Action>() -> &'static str {
    find(A::name_for_type()).map_or(A::name_for_type(), |spec| spec.label)
}

/// The spec of `action`'s type.
pub fn spec_of(action: &dyn Action) -> Option<&'static ActionSpec> {
    REGISTRY.iter().find(|spec| spec.matches(action))
}

/// The actions available from the focused element now, for a command palette.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the command palette (#29) and automation")
)]
pub fn available<'a>(
    window: &'a Window,
    cx: &'a App,
) -> impl Iterator<Item = &'static ActionSpec> + 'a {
    REGISTRY
        .iter()
        .filter(move |spec| spec.is_available(window, cx))
}

/// The actions available from `focus` (the element focused before a palette opened).
pub fn available_in<'a>(
    focus: &'a FocusHandle,
    window: &'a Window,
    cx: &'a App,
) -> impl Iterator<Item = &'static ActionSpec> + 'a {
    REGISTRY
        .iter()
        .filter(move |spec| spec.is_available_in(focus, window, cx))
}

/// Builds the action `name` from JSON `args` and dispatches it from the focused element, as
/// its key binding would (from the workspace's target when nothing is focused), after the
/// availability and guard checks. Names outside the registry
/// are built by GPUI, so library actions work too (without guards).
#[cfg_attr(
    not(any(test, feature = "automation")),
    expect(dead_code, reason = "used by the command palette (#29) and automation")
)]
pub fn dispatch(
    name: &str,
    args: Value,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), ActionError> {
    let action = match find(name) {
        Some(spec) => spec.build(args)?,
        None => {
            if !cx.all_action_names().contains(&name) {
                return Err(ActionError::Unknown(name.to_owned()));
            }
            let data = (!args.is_null()).then_some(args);
            cx.build_action(name, data)
                .map_err(|e| ActionError::InvalidArgs {
                    action: name.to_owned(),
                    message: e.to_string(),
                })?
        }
    };
    // With nothing focused, from the workspace's target, as a click would.
    let focus = window.focused(cx).or_else(|| installed_target(cx));
    match focus {
        Some(focus) => dispatch_in(&focus, &*action, window, cx),
        None => Err(ActionError::NotAvailable(name.to_owned())),
    }
}

/// Checks `action` and dispatches it from `focus`, synchronously: the path every click takes,
/// and the one a palette uses with the element focused before it opened.
pub fn dispatch_in(
    focus: &FocusHandle,
    action: &dyn Action,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), ActionError> {
    check_in(focus, action, window, cx)?;
    focus.dispatch_action(action, window, cx);
    Ok(())
}

/// The checks [`dispatch_in`] makes, without dispatching: whether `action` would be handled
/// from `focus`, and whether its guard lets it through. The palette asks before it closes, so a
/// refusal stays on screen beside what was typed.
pub fn check_in(
    focus: &FocusHandle,
    action: &dyn Action,
    window: &Window,
    cx: &App,
) -> Result<(), ActionError> {
    let spec = spec_of(action);
    let handled = match spec {
        Some(spec) if spec.scope == Scope::App => true,
        _ => window.is_action_available_in(action, focus),
    };
    if !handled {
        return Err(ActionError::NotAvailable(action.name().to_owned()));
    }
    match spec {
        Some(spec) => spec.validate(action, cx),
        None => Ok(()),
    }
}

/// Checks and dispatches `action` from the workspace's focus target (see [`install`]), or from
/// the focused element in a window without one.
pub fn perform(action: &dyn Action, window: &mut Window, cx: &mut App) -> Result<(), ActionError> {
    let target = installed_target(cx).or_else(|| window.focused(cx));
    match target {
        Some(focus) => dispatch_in(&focus, action, window, cx),
        None => Err(ActionError::NotAvailable(action.name().to_owned())),
    }
}

/// A click handler that performs `action`: what every control in `ui/` uses, so a click is
/// exactly its action. A refusal is logged and shown ([`report_refusal`]): the control was
/// drawn from older state.
pub fn on_click<E: ?Sized, A: Action>(action: A) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        if let Err(e) = perform(&action, window, cx) {
            report_refusal(&e, cx);
        }
    }
}

/// Logs `error` and tells the person why their click did nothing, in the installed workspace's
/// notice.
pub fn report_refusal(error: &ActionError, cx: &mut App) {
    tracing::warn!("{error}");
    let workspace = cx
        .try_global::<Installed>()
        .and_then(|installed| installed.owner.as_ref()?.upgrade())
        .and_then(|owner| owner.downcast::<super::workspace::Workspace>().ok());
    if let Some(workspace) = workspace {
        let why = error.for_person();
        workspace.update(cx, |ws, cx| ws.refuse(why, cx));
    }
}

/// [`on_click`] for a control that answers something other than the workspace (a dialog's
/// button): `action` is dispatched from `focus`, a node inside that thing.
pub fn on_click_in<E: ?Sized, A: Action>(
    focus: FocusHandle,
    action: A,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        if let Err(e) = dispatch_in(&focus, &action, window, cx) {
            report_refusal(&e, cx);
        }
    }
}

/// [`on_click`] that also hands a refusal to `refused`, for a control drawn from state that may
/// be older than the click (a menu left open), so the user learns why nothing happened.
pub fn on_click_or<E: ?Sized>(
    action: Box<dyn Action>,
    refused: impl Fn(&dyn Action, &ActionError, &mut App) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        if let Err(e) = perform(&*action, window, cx) {
            tracing::warn!("{e}");
            refused(&*action, &e, cx);
        }
    }
}

/// [`on_click`] for a control inside another clickable element (a button on a row): the click
/// stops there.
pub fn on_click_contained<E: ?Sized, A: Action>(
    action: A,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let perform = on_click(action);
    move |event, window, cx| {
        cx.stop_propagation();
        perform(event, window, cx);
    }
}

/// [`on_click`] for a press that something around the control also handles (a row's right-click,
/// which selects the row and then opens its context menu): the press goes on. Dispatching an
/// action marks the event handled, which would stop it here.
pub fn on_click_passing<E: ?Sized, A: Action>(
    action: A,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let perform = on_click(action);
    move |event, window, cx| {
        perform(event, window, cx);
        cx.propagate();
    }
}

/// Binds every action's default keys. Call once at startup, before opening windows.
pub fn init(cx: &mut App) {
    let parse =
        |c: &str| Rc::new(KeyBindingContextPredicate::parse(c).expect("static key context parses"));
    let mut bindings = Vec::new();
    for spec in REGISTRY.iter() {
        let context = spec.key_context.map(parse);
        for keys in spec.keys {
            match KeyBinding::load(keys, (spec.probe)(), context.clone(), false, None, cx) {
                Ok(binding) => bindings.push(binding),
                Err(e) => tracing::error!(action = spec.name, keys, "invalid key binding: {e}"),
            }
        }
        // Cmd-W closes the window when no dock tab is shown: Close Dock Tab, bound to it later
        // (so first in precedence), is bound only while a tab is shown, and the key falls
        // through to this. Not in the spec's keys, so menus and the palette show Shift-Cmd-W.
        if spec.type_id == TypeId::of::<CloseWindow>() {
            push(
                &mut bindings,
                CLOSE_WINDOW_FALLBACK,
                (spec.probe)(),
                &context,
                cx,
            );
        }
        // The list's arrows also work in a window holding only a list (as in view tests).
        if spec.scope == Scope::List && spec.key_context == Some(PLAIN) {
            for keys in spec.keys {
                if let Ok(binding) = KeyBinding::load(
                    keys,
                    (spec.probe)(),
                    Some(parse(LIST_ALONE)),
                    false,
                    None,
                    cx,
                ) {
                    bindings.push(binding);
                }
            }
        }
    }
    // Keys the shell in a shell tab gets instead of the app.
    let terminal = Some(parse(TERMINAL));
    // Linux terminals close their tab on Ctrl+Shift+W: so does a shell tab, over Close Window.
    for keys in TERMINAL_CLOSE_TAB {
        push(&mut bindings, keys, Box::new(CloseActiveTab), &terminal, cx);
    }
    for keys in TERMINAL_PASSTHROUGH {
        match KeyBinding::load(keys, Box::new(NoAction), terminal.clone(), false, None, cx) {
            Ok(binding) => bindings.push(binding),
            Err(e) => tracing::error!(keys, "invalid terminal key: {e}"),
        }
    }
    // In a confirmation whose OK ends sessions, Enter cancels, as Escape does: that dialog's own
    // keys are off, and these answer for it.
    for keys in ["enter", "escape"] {
        bindings.push(KeyBinding::new(
            keys,
            Cancel,
            Some(super::confirm::DEFAULT_CANCEL),
        ));
    }
    super::confirm::init(cx);
    cx.bind_keys(bindings);
}

/// Loads `keys` for `action` in `context` into `bindings`, logging a key that does not parse.
fn push(
    bindings: &mut Vec<KeyBinding>,
    keys: &str,
    action: Box<dyn Action>,
    context: &Option<Rc<KeyBindingContextPredicate>>,
    cx: &App,
) {
    let name = action.name();
    match KeyBinding::load(keys, action, context.clone(), false, None, cx) {
        Ok(binding) => bindings.push(binding),
        Err(e) => tracing::error!(action = name, keys, "invalid key binding: {e}"),
    }
}

/// A type-erased `enabled` check.
type Enabled = Rc<dyn Fn(&App) -> bool>;
/// A type-erased `validate` check.
type Validate = Rc<dyn Fn(&dyn Action, &App) -> Result<(), ActionError>>;

/// A guard's two checks.
struct Guard {
    enabled: Enabled,
    validate: Validate,
}

/// What the workspace installs: where clicks dispatch from, and the guards.
struct Installed {
    target: Option<FocusHandle>,
    guards: HashMap<TypeId, Guard>,
    /// The view the guards belong to, which shows a refused click's reason.
    owner: Option<AnyWeakEntity>,
}

impl Global for Installed {}

fn installed_target(cx: &App) -> Option<FocusHandle> {
    cx.try_global::<Installed>()?.target.clone()
}

fn guard(type_id: TypeId, cx: &App) -> Option<&Guard> {
    cx.try_global::<Installed>()?.guards.get(&type_id)
}

/// Guards over view `V` (held weakly): per action, `enabled` (whatever the arguments) and
/// `validate` (these arguments). Built by the workspace and handed to [`install`].
pub struct Guards<V: 'static> {
    view: WeakEntity<V>,
    guards: HashMap<TypeId, Guard>,
}

impl<V: 'static> Guards<V> {
    pub fn new(view: WeakEntity<V>) -> Self {
        Self {
            view,
            guards: HashMap::new(),
        }
    }

    /// Guards action `A`. With the view gone, `A` is disabled.
    pub fn add<A: Action>(
        mut self,
        enabled: fn(&V, &App) -> bool,
        validate: fn(&V, &A, &App) -> Result<(), ActionError>,
    ) -> Self {
        let view = self.view.clone();
        let enabled_view = view.clone();
        self.guards.insert(
            TypeId::of::<A>(),
            Guard {
                enabled: Rc::new(move |cx| {
                    enabled_view
                        .upgrade()
                        .is_some_and(|v| enabled(v.read(cx), cx))
                }),
                validate: Rc::new(move |action, cx| {
                    let Some(action) = action.as_any().downcast_ref::<A>() else {
                        return Ok(());
                    };
                    match view.upgrade() {
                        Some(v) => validate(v.read(cx), action, cx),
                        None => Err(ActionError::unavailable::<A>()),
                    }
                }),
            },
        );
        self
    }

    /// Guards action `A` with an `enabled` check only.
    pub fn when<A: Action>(self, enabled: fn(&V, &App) -> bool) -> Self {
        self.add::<A>(enabled, |_, _, _| Ok(()))
    }
}

/// Installs `guards` and the focus `target` clicks dispatch from (the workspace's), replacing
/// earlier ones: Flint has one main window.
pub fn install<V: 'static>(target: FocusHandle, guards: Guards<V>, cx: &mut App) {
    cx.set_global(Installed {
        target: Some(target),
        owner: Some(guards.view.into()),
        guards: guards.guards,
    });
}

/// A view's action handler: the same signature as a GPUI action listener.
pub type Handler<V, A> = fn(&mut V, &A, &mut Window, &mut Context<V>);

/// Where a view's handlers get registered. A view lists its handlers once, in a function
/// generic over this trait, and that list serves both its own root element ([`OnView`]) and the
/// workspace, which forwards the view's actions to it from anywhere in the window
/// ([`Forward`]).
pub trait Bind<V: 'static>: Sized {
    fn bind<A: Action>(self, handler: Handler<V, A>) -> Self;
}

/// Registers a view's handlers on its own root element.
pub struct OnView<'a, E, V: 'static> {
    element: E,
    cx: &'a Context<'a, V>,
}

impl<'a, E: InteractiveElement, V: 'static> OnView<'a, E, V> {
    pub fn new(element: E, cx: &'a Context<'a, V>) -> Self {
        Self { element, cx }
    }

    pub fn into_element(self) -> E {
        self.element
    }
}

impl<E: InteractiveElement, V: 'static> Bind<V> for OnView<'_, E, V> {
    fn bind<A: Action>(self, handler: Handler<V, A>) -> Self {
        let element = self.element.on_action(self.cx.listener(handler));
        Self {
            element,
            cx: self.cx,
        }
    }
}

/// Registers a child view's handlers on an ancestor's element, running them on that view. It
/// holds the view weakly, so the last drawn frame does not keep a closed view alive. With no
/// view (none is open) it registers nothing, so the actions are not available.
pub struct Forward<E, V: 'static> {
    element: E,
    view: Option<WeakEntity<V>>,
}

impl<E: InteractiveElement, V: 'static> Forward<E, V> {
    pub fn new(element: E, view: Option<&Entity<V>>) -> Self {
        Self {
            element,
            view: view.map(Entity::downgrade),
        }
    }

    pub fn into_element(self) -> E {
        self.element
    }
}

impl<E: InteractiveElement, V: 'static> Bind<V> for Forward<E, V> {
    fn bind<A: Action>(self, handler: Handler<V, A>) -> Self {
        let Some(view) = self.view else {
            return self;
        };
        let target = view.clone();
        let element =
            self.element
                .on_action(move |action: &A, window: &mut Window, cx: &mut App| {
                    // The view closed since this frame was drawn: nothing to do.
                    let _ = target.update(cx, |view, cx| handler(view, action, window, cx));
                });
        Self {
            element,
            view: Some(view),
        }
    }
}

/// [`Forward`] to the view `current` finds on `owner` when an action comes, rather than the
/// one drawn: an action that follows a change of view before the next frame (a key, or an
/// automation step, right after selecting another row) reaches the new view. Without a view
/// when drawn, the actions are not handled here.
pub struct ForwardCurrent<E, W: 'static, V: 'static> {
    element: E,
    owner: WeakEntity<W>,
    current: fn(&W) -> Option<&Entity<V>>,
    shown: bool,
}

impl<E: InteractiveElement, W: 'static, V: 'static> ForwardCurrent<E, W, V> {
    pub fn new(
        element: E,
        owner: &W,
        cx: &Context<W>,
        current: fn(&W) -> Option<&Entity<V>>,
    ) -> Self {
        let shown = current(owner).is_some();
        Self {
            element,
            owner: cx.weak_entity(),
            current,
            shown,
        }
    }

    pub fn into_element(self) -> E {
        self.element
    }
}

impl<E: InteractiveElement, W: 'static, V: 'static> Bind<V> for ForwardCurrent<E, W, V> {
    fn bind<A: Action>(self, handler: Handler<V, A>) -> Self {
        if !self.shown {
            return self;
        }
        let owner = self.owner.clone();
        let current = self.current;
        let element =
            self.element
                .on_action(move |action: &A, window: &mut Window, cx: &mut App| {
                    let Some(owner) = owner.upgrade() else {
                        return;
                    };
                    // The view closed since this frame was drawn: nothing to do.
                    let Some(view) = current(owner.read(cx)).cloned() else {
                        return;
                    };
                    view.update(cx, |view, cx| handler(view, action, window, cx));
                });
        Self { element, ..self }
    }
}

/// The workspace's key context.
pub const WORKSPACE: &str = "Workspace";
/// Plain keys (arrows, Esc) work in the window, but not while typing in a field.
const PLAIN: &str = "Workspace && !Input";
/// On the cluster overview, outside a text field.
const DASHBOARD_PLAIN: &str = "Dashboard && !Input";
/// The same, for the list: deeper than the workspace, so it wins there.
const LIST_ALONE: &str = "ResourceList && !Input";
/// The list's text filter.
const LIST_FILTER: &str = "ResourceList > Input";
/// The bottom dock's key context.
pub const DOCK: &str = "Dock";
/// A log tab's key context.
pub const LOG_VIEW: &str = "LogView";
/// A shell tab's key context.
pub const TERMINAL: &str = "Terminal";
/// Plain keys in a log tab, but not while typing in its search field.
const LOG_PLAIN: &str = "LogView && !Input";
/// Keys meant for the log tab's search field itself (Esc), which the field passes on.
const LOG_SEARCH: &str = "LogView > Input";
/// The command palette's key context.
pub const PALETTE: &str = "CommandPalette";
/// Keys the palette takes from its own search field (the arrows, Enter, Esc), over the field's.
const PALETTE_INPUT: &str = "CommandPalette > Input";
/// The namespace picker's key context.
pub const NAMESPACE_PICKER: &str = "NamespacePicker";
/// Keys the picker keeps for moving between its controls, also over its text field's.
const NAMESPACE_PICKER_KEYS: &str = "NamespacePicker || (NamespacePicker > Input)";
/// A focused text field, where gpui-component handles Cut, Copy, Paste and Select All.
const TEXT_FIELD: &str = "Input";
/// The cluster sidebar's key context, while it has focus.
pub const SIDEBAR: &str = "ClusterSidebar";
/// Plain keys in the cluster sidebar: deeper than the workspace's, so they win there.
const SIDEBAR_PLAIN: &str = "ClusterSidebar && !Input";
/// The navigation's key context, while it has focus.
pub const NAVIGATION: &str = "Navigation";
/// Plain keys in the navigation: deeper than the workspace's, so they win there.
const NAVIGATION_PLAIN: &str = "Navigation && !Input";

fn build_unit<A: Action + Default>(args: Value) -> Result<Box<dyn Action>, String> {
    match args {
        Value::Null => Ok(Box::new(A::default())),
        Value::Object(map) if map.is_empty() => Ok(Box::new(A::default())),
        _ => Err("takes no arguments".to_owned()),
    }
}

fn build_with<A: Action + DeserializeOwned>(args: Value) -> Result<Box<dyn Action>, String> {
    serde_json::from_value::<A>(args)
        .map(|a| Box::new(a) as Box<dyn Action>)
        .map_err(|e| e.to_string())
}

fn probe<A: Action + Default>() -> Box<dyn Action> {
    Box::new(A::default())
}

fn build_confirm(args: Value) -> Result<Box<dyn Action>, String> {
    build_unit::<Cancel>(args).map(|_| Box::new(Confirm { secondary: false }) as Box<dyn Action>)
}

fn probe_confirm() -> Box<dyn Action> {
    Box::new(Confirm { secondary: false })
}

/// A spec under construction; see `REGISTRY`.
struct Spec(ActionSpec);

impl Spec {
    fn unit<A: Action + Default>(label: &'static str, description: &'static str) -> Self {
        Spec(ActionSpec {
            name: A::name_for_type(),
            label,
            description,
            category: Category::Flint,
            scope: Scope::Workspace,
            visibility: Visibility::Palette,
            args: &[],
            keys: &[],
            key_context: None,
            type_id: TypeId::of::<A>(),
            build: build_unit::<A>,
            probe: probe::<A>,
        })
    }

    fn with_args<A: Action + Default + DeserializeOwned>(
        label: &'static str,
        description: &'static str,
        args: &'static [Arg],
    ) -> Self {
        let mut spec = Self::unit::<A>(label, description);
        spec.0.args = args;
        spec.0.build = build_with::<A>;
        spec
    }

    /// The dialog's Confirm, which has no `Default`.
    fn confirm() -> Self {
        Spec(ActionSpec {
            name: Confirm::name_for_type(),
            label: "Confirm",
            description: "Confirm the open dialog.",
            category: Category::Flint,
            scope: Scope::Dialog,
            visibility: Visibility::Palette,
            args: &[],
            keys: &[],
            key_context: None,
            type_id: TypeId::of::<Confirm>(),
            build: build_confirm,
            probe: probe_confirm,
        })
    }

    fn of(mut self, category: Category, scope: Scope) -> Self {
        self.0.category = category;
        self.0.scope = scope;
        self
    }

    fn keys(mut self, keys: &'static [&'static str], context: Option<&'static str>) -> Self {
        self.0.keys = keys;
        self.0.key_context = context;
        self
    }

    fn hidden(mut self) -> Self {
        self.0.visibility = Visibility::Hidden;
        self
    }
}

const fn arg(name: &'static str, description: &'static str, kind: ArgKind) -> Arg {
    Arg {
        name,
        description,
        required: true,
        kind,
    }
}

const CLUSTER: &[Arg] = &[arg("cluster", "kubeconfig context name", ArgKind::Cluster)];
const ALERT: &[Arg] = &[arg("tag", "the alert's tag", ArgKind::Text)];
const CLUSTER_ALERTS: &[Arg] = &[
    arg("cluster", "kubeconfig context name", ArgKind::Cluster),
    arg(
        "choice",
        "default, on or off",
        ArgKind::Choice(&["default", "on", "off"]),
    ),
];
const SETTINGS_SECTION: &[Arg] = &[arg(
    "section",
    "appearance, alerts, watching, thresholds, cluster_overrides, logs, paths, safety or network",
    ArgKind::Choice(&[
        "appearance",
        "alerts",
        "watching",
        "thresholds",
        "cluster_overrides",
        "logs",
        "paths",
        "safety",
        "network",
    ]),
)];
const THRESHOLD_METRICS: &[&str] = &[
    "cpu_percent",
    "memory_percent",
    "restarts_last_hour",
    "warning_events_last_hour",
    "pending_pod_seconds",
];
const EDIT_THRESHOLD: &[Arg] = &[
    Arg {
        name: "cluster",
        description: "kubeconfig context name; the global value when left out",
        required: false,
        kind: ArgKind::Cluster,
    },
    arg(
        "metric",
        "cpu_percent, memory_percent, restarts_last_hour, warning_events_last_hour or \
         pending_pod_seconds",
        ArgKind::Choice(THRESHOLD_METRICS),
    ),
    Arg {
        name: "value",
        description: "a whole number; left out or empty to follow the level below",
        required: false,
        kind: ArgKind::Text,
    },
];
const TYPE_THRESHOLD: &[Arg] = &[
    Arg {
        name: "cluster",
        description: "kubeconfig context name; the global value when left out",
        required: false,
        kind: ArgKind::Cluster,
    },
    arg(
        "metric",
        "cpu_percent, memory_percent, restarts_last_hour, warning_events_last_hour or \
         pending_pod_seconds",
        ArgKind::Choice(THRESHOLD_METRICS),
    ),
];
const EDIT_CLUSTER_ALERTS: &[Arg] = &[
    arg("cluster", "kubeconfig context name", ArgKind::Cluster),
    arg(
        "choice",
        "default, on or off",
        ArgKind::Choice(&["default", "on", "off"]),
    ),
];
const EDIT_CLUSTER_WATCH_MODE: &[Arg] = &[
    arg("cluster", "kubeconfig context name", ArgKind::Cluster),
    arg(
        "mode",
        "default, all or on_view",
        ArgKind::Choice(&["default", "all", "on_view"]),
    ),
];
const RESET_CLUSTER_OVERRIDES: &[Arg] =
    &[arg("cluster", "kubeconfig context name", ArgKind::Cluster)];
const ADD_CLUSTER_OVERRIDE: &[Arg] = &[Arg {
    name: "cluster",
    description: "kubeconfig context name; shows the list of clusters when left out",
    required: false,
    kind: ArgKind::Cluster,
}];
const EDIT_APPEARANCE: &[Arg] = &[arg(
    "appearance",
    "system, light or dark",
    ArgKind::Choice(&["system", "light", "dark"]),
)];
const EDIT_NOTIFICATIONS: &[Arg] = &[arg("enabled", "true or false", ArgKind::Bool)];
const EDIT_LOG_LINES: &[Arg] = &[arg("lines", "from 1000 to 1000000", ArgKind::Number)];
const EDIT_LOG_MIB: &[Arg] = &[arg("mib", "from 8 to 1024", ArgKind::Number)];
const EDIT_LOG_TOTAL: &[Arg] = &[arg("mib", "from 64 to 16384", ArgKind::Number)];
const EDIT_PATH: &[Arg] = &[arg(
    "path",
    "absolute folders separated by colons; empty uses the login shell's",
    ArgKind::Text,
)];
const EDIT_HELM: &[Arg] = &[arg(
    "path",
    "the helm binary, absolute or under ~; empty finds it on PATH",
    ArgKind::Text,
)];
const KUBECONFIG_PATH: &[Arg] = &[arg(
    "path",
    "a kubeconfig file, absolute or under ~",
    ArgKind::Text,
)];
const EDIT_PROXY_URL: &[Arg] = &[arg(
    "url",
    "http://, socks5:// or socks5h://host:port; empty uses HTTPS_PROXY from the environment",
    ArgKind::Text,
)];
const EDIT_NO_PROXY: &[Arg] = &[arg(
    "no_proxy",
    "hosts, domains, addresses or ranges reached without the proxy, separated by commas",
    ArgKind::Text,
)];
const EDIT_PROTECT_LOCAL: &[Arg] = &[arg("enabled", "true or false", ArgKind::Bool)];
const EDIT_WATCH_MODE: &[Arg] = &[arg(
    "mode",
    "all or on_view",
    ArgKind::Choice(&["all", "on_view"]),
)];
const PENDING_THRESHOLD: &[Arg] = &[
    Arg {
        name: "cluster",
        description: "kubeconfig context name; all clusters when left out",
        required: false,
        kind: ArgKind::Cluster,
    },
    Arg {
        name: "seconds",
        description: "seconds from 1 to 86400; clears the value when left out",
        required: false,
        kind: ArgKind::Number,
    },
];
const COMPARE_OBJECT: &[Arg] = &[
    arg("cluster", "kubeconfig context name", ArgKind::Cluster),
    arg("kind", "the kind, as plural or plural.group", ArgKind::Text),
    Arg {
        name: "namespace",
        description: "the object's namespace",
        required: false,
        kind: ArgKind::Namespace,
    },
    arg("name", "the object's name", ArgKind::Text),
];
const SET_NAME: &[Arg] = &[arg("name", "the cluster set's name", ArgKind::Text)];
const SET_PROBLEM: &[Arg] = &[
    arg(
        "category",
        "which problem count",
        ArgKind::Choice(&super::dashboard::CATEGORY_NAMES),
    ),
    Arg {
        name: "cluster",
        description: "the member; every member when left out",
        required: false,
        kind: ArgKind::Cluster,
    },
    Arg {
        name: "kind",
        description: "which kind of short workload",
        required: false,
        kind: ArgKind::Choice(&super::dashboard::SHORT_WORKLOAD_KINDS),
    },
];
const COMBINED_ROW: &[Arg] = &[
    arg("cluster", "kubeconfig context name", ArgKind::Cluster),
    arg("uid", "the object's uid", ArgKind::Text),
];
const COMBINED_COLUMN: &[Arg] = &[
    arg("key", "the column's key", ArgKind::Text),
    arg(
        "occurrence",
        "which column with that key, from 0",
        ArgKind::Number,
    ),
];
const CLUSTER_TAG: &[Arg] = &[
    arg("cluster", "kubeconfig context name", ArgKind::Cluster),
    arg(
        "tag",
        "the colour",
        ArgKind::Choice(&["blue", "purple", "teal", "green", "yellow", "red"]),
    ),
];
const DRAIN_OPTION: &[Arg] = &[arg(
    "option",
    "what the drain may also evict",
    ArgKind::Choice(&["delete_emptydir_data", "force"]),
)];
const SCOPE_NAMESPACE: &[Arg] = &[arg("namespace", "namespace name", ArgKind::Text)];
const SCOPE_KIND: &[Arg] = &[arg(
    "kind",
    "kind id: plural or plural.group",
    ArgKind::Text,
)];
const NAMESPACE: &[Arg] = &[arg("namespace", "namespace name", ArgKind::Namespace)];
const KIND: &[Arg] = &[arg(
    "kind",
    "plural or plural.group: pods, deployments.apps",
    ArgKind::Kind,
)];
const PROBLEM_CATEGORY: &[Arg] = &[arg(
    "category",
    "which problem tile",
    ArgKind::Choice(&super::dashboard::CATEGORY_NAMES),
)];
const SHORT_WORKLOAD_KIND: &[Arg] = &[arg(
    "kind",
    "which kind of short workload",
    ArgKind::Choice(&super::dashboard::SHORT_WORKLOAD_KINDS),
)];
const SECTION: &[Arg] = &[arg(
    "section",
    "section title, as shown: Workloads",
    ArgKind::Text,
)];
const PANE: &[Arg] = &[
    arg(
        "pane",
        "the pane",
        ArgKind::Choice(&["clusters", "navigation", "detail"]),
    ),
    arg("width", "width in points", ArgKind::Number),
];
const NAMESPACES: &[Arg] = &[arg(
    "namespaces",
    "namespace names; empty for all",
    ArgKind::List,
)];
const OBJECT: &[Arg] = &[
    arg(
        "kind",
        "plural or plural.group: pods, deployments.apps",
        ArgKind::Kind,
    ),
    Arg {
        name: "namespace",
        description: "namespace; leave out for cluster-scoped kinds",
        required: false,
        kind: ArgKind::Namespace,
    },
    arg("name", "object name", ArgKind::Text),
];
const ROW: &[Arg] = &[
    Arg {
        name: "namespace",
        description: "namespace; leave out for cluster-scoped kinds",
        required: false,
        kind: ArgKind::Namespace,
    },
    arg("name", "object name", ArgKind::Text),
];
const HELM_RELEASE: &[Arg] = &[
    arg("namespace", "the release's namespace", ArgKind::Namespace),
    arg("name", "release name", ArgKind::Text),
];
const COLUMN: &[Arg] = &[
    arg("column", "column name, as drawn: Restarts", ArgKind::Column),
    Arg {
        name: "key",
        description: "the column's key instead of its name (server:Restarts, flint:age), as a \
                      header click sends it",
        required: false,
        kind: ArgKind::Text,
    },
];
const KEY: &[Arg] = &[arg("key", "the data key", ArgKind::Text)];
const LOG_CONTEXT: &[Arg] = &[arg(
    "lines",
    "lines before and after, 0 to 100",
    ArgKind::Number,
)];
const PALETTE_ROW: &[Arg] = &[
    arg("row", "row, from the top, 0 and up", ArgKind::Number),
    Arg {
        name: "rows",
        description: "the version of the rows clicked; a stale one is dropped",
        required: false,
        kind: ArgKind::Number,
    },
];
const CONTAINER: &[Arg] = &[
    arg("container", "container name", ArgKind::Container),
    Arg {
        name: "previous",
        description: "the previous (terminated) instance",
        required: false,
        kind: ArgKind::Bool,
    },
];
const SELECTOR: &[Arg] = &[
    arg("namespace", "namespace name", ArgKind::Namespace),
    arg(
        "selector",
        "label selector, as kubectl -l takes it: app=web,tier!=db",
        ArgKind::Text,
    ),
];
const SHELL_CONTAINER: &[Arg] = &[arg("container", "container name", ArgKind::Container)];
const FORWARD_ID: &[Arg] = &[arg("id", "the port-forward's id", ArgKind::Number)];
const CELLS: &[Arg] = &[
    arg(
        "start_row",
        "first cell's row, from the top",
        ArgKind::Number,
    ),
    arg("start_col", "first cell's column", ArgKind::Number),
    arg("end_row", "last cell's row", ArgKind::Number),
    arg("end_col", "last cell's column", ArgKind::Number),
];
const HEIGHT: &[Arg] = &[arg("height", "height in points", ArgKind::Number)];
const TAB: &[Arg] = &[arg("tab", "tab id", ArgKind::Number)];
const LINE: &[Arg] = &[
    arg("line", "line id", ArgKind::Number),
    Arg {
        name: "extend",
        description: "extend the selection instead of starting one",
        required: false,
        kind: ArgKind::Bool,
    },
];
const DRAG_LINE: &[Arg] = &[arg("line", "line id", ArgKind::Number)];
const SAVE: &[Arg] = &[
    arg(
        "lines",
        "which lines",
        ArgKind::Choice(&["all", "matches", "selection"]),
    ),
    Arg {
        name: "timestamps",
        description: "include timestamps (default: as shown)",
        required: false,
        kind: ArgKind::Bool,
    },
];

static REGISTRY: LazyLock<Vec<ActionSpec>> = LazyLock::new(|| {
    use self::Scope as S;
    use Category as C;
    let specs = vec![
        // Flint.
        Spec::unit::<Quit>(
            "Quit Flint",
            "Quit the app, asking first while a shell runs or a log tab follows.",
        )
            .of(C::Flint, S::App)
            .keys(&["secondary-q"], None),
        // Shift-Cmd-W: Cmd-W closes the dock's tab first (Close Dock Tab), and the window only
        // when the dock shows none (#177).
        Spec::unit::<CloseWindow>(
            "Close Window",
            "Close the front window, asking first while a shell runs or a log tab follows.",
        )
        .of(C::Flint, S::App)
        .keys(&["secondary-shift-w"], Some(WORKSPACE)),
        Spec::unit::<AppearanceSystem>(
            "Use System Appearance",
            "Follow the system light and dark mode.",
        )
        .of(C::Flint, S::App),
        Spec::unit::<AppearanceLight>("Use Light Appearance", "Always use Flint Light.")
            .of(C::Flint, S::App),
        Spec::unit::<AppearanceDark>("Use Dark Appearance", "Always use Flint Dark.")
            .of(C::Flint, S::App),
        Spec::unit::<ToggleDesktopNotifications>(
            "Toggle Desktop Notifications",
            "Notify about new problems on clusters watching all kinds.",
        )
        .of(C::Flint, S::App),
        Spec::with_args::<OpenAlert>("Open Alert…", "Open the cluster that raised an alert.", ALERT)
            .of(C::Flint, S::App)
            .hidden(),
        Spec::with_args::<InvestigateAlert>(
            "Investigate Alert…",
            "Open an alert's cluster and copy a read-only prompt for an agent.",
            ALERT,
        )
        .of(C::Flint, S::App)
        .hidden(),
        Spec::with_args::<SetClusterAlerts>(
            "Set Cluster Alerts…",
            "Choose whether one cluster posts desktop notifications.",
            CLUSTER_ALERTS,
        )
        .of(C::Flint, S::App)
        .hidden(),
        Spec::with_args::<SetPendingThreshold>(
            "Set Pending Pod Threshold…",
            "Choose how long a pod may stay Pending before it counts as a problem.",
            PENDING_THRESHOLD,
        )
        .of(C::Flint, S::App)
        .hidden(),
        Spec::unit::<About>("About Flint", "Show Flint's version.").of(C::Flint, S::Workspace),
        // Settings (#739): Cmd-, on macOS and Ctrl-, on Linux, as in most apps.
        Spec::unit::<OpenSettings>(
            "Settings",
            "Open Flint's settings: appearance, notifications and the default watch mode.",
        )
        .of(C::Flint, S::Settings)
        .keys(&[super::settings_view::OPEN_SETTINGS_KEY], Some(WORKSPACE)),
        Spec::unit::<CloseSettings>(
            "Close Settings",
            "Leave Settings for the view shown before. Unsaved edits stay.",
        )
        .of(C::Flint, S::Settings)
        .keys(&["escape"], Some(super::settings_view::SETTINGS_KEYS)),
        Spec::unit::<SaveSettings>(
            "Save Settings",
            "Write the settings edited in Settings to settings.toml, and only those.",
        )
        .of(C::Flint, S::Settings),
        Spec::unit::<RevertSettings>(
            "Revert Settings",
            "Drop the unsaved edits in Settings and show settings.toml as it is.",
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<ShowSettingsSection>(
            "Show Settings Section…",
            "Show one section of Settings.",
            SETTINGS_SECTION,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditAppearance>(
            "Edit Appearance…",
            "Set the appearance in the Settings draft.",
            EDIT_APPEARANCE,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditDesktopNotifications>(
            "Edit Desktop Notifications…",
            "Turn desktop notifications on or off in the Settings draft.",
            EDIT_NOTIFICATIONS,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditWatchMode>(
            "Edit Default Watch Mode…",
            "Set the default watch mode in the Settings draft.",
            EDIT_WATCH_MODE,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditLogBufferLines>(
            "Edit Log Lines Per Tab…",
            "Set the lines each log tab keeps in the Settings draft.",
            EDIT_LOG_LINES,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditLogBufferMib>(
            "Edit Log Memory Per Tab…",
            "Set the MiB each log tab keeps in the Settings draft.",
            EDIT_LOG_MIB,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditLogTotalMib>(
            "Edit Total Log Memory…",
            "Set the MiB all log tabs keep together in the Settings draft.",
            EDIT_LOG_TOTAL,
        )
        .of(C::Flint, S::Settings),
        Spec::unit::<ChooseKubeconfigPaths>(
            "Choose Kubeconfig Files",
            "Choose kubeconfig files to add to the list in the Settings draft.",
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<AddKubeconfigPath>(
            "Add Kubeconfig Path…",
            "Add a kubeconfig file to the list in the Settings draft.",
            KUBECONFIG_PATH,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<RemoveKubeconfigPath>(
            "Remove Kubeconfig Path…",
            "Drop a kubeconfig file from the list in the Settings draft.",
            KUBECONFIG_PATH,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditPathOverride>(
            "Edit PATH…",
            "Set the PATH override in the Settings draft.",
            EDIT_PATH,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditHelmPath>(
            "Edit Helm Path…",
            "Set the helm binary in the Settings draft.",
            EDIT_HELM,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditProtectLocalClusters>(
            "Edit Local Cluster Protection…",
            "Turn protection of local clusters on or off in the Settings draft. Turning it off \
             is confirmed at Save.",
            EDIT_PROTECT_LOCAL,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditProxyUrl>(
            "Edit Proxy URL…",
            "Set the proxy URL in Settings' Network section; Apply Proxy puts it in use.",
            EDIT_PROXY_URL,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditNoProxy>(
            "Edit No Proxy…",
            "Set the servers reached without the proxy in Settings' Network section.",
            EDIT_NO_PROXY,
        )
        .of(C::Flint, S::Settings),
        Spec::unit::<ApplyProxy>(
            "Apply Proxy",
            "Save the proxy typed in Settings and reconnect every cluster on it, after asking.",
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditThreshold>(
            "Edit Threshold…",
            "Set a warning threshold, global or for one cluster, in the Settings draft.",
            EDIT_THRESHOLD,
        )
        .of(C::Flint, S::Settings)
        // The palette skips the optional value, so from there it could only clear one.
        .hidden(),
        Spec::with_args::<TypeThreshold>(
            "Type Threshold…",
            "Open the text field of a warning threshold in Settings.",
            TYPE_THRESHOLD,
        )
        .of(C::Flint, S::Settings)
        .hidden(),
        Spec::unit::<CancelThresholdEntry>(
            "Cancel Threshold Entry",
            "Drop what is typed in a threshold field in Settings.",
        )
        .of(C::Flint, S::Settings)
        .keys(&["escape"], Some(super::settings_view::SETTINGS_INPUT_KEYS))
        .hidden(),
        Spec::with_args::<EditClusterAlerts>(
            "Edit Cluster Alerts Override…",
            "Set whether one cluster posts desktop notifications, in the Settings draft.",
            EDIT_CLUSTER_ALERTS,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<EditClusterWatchMode>(
            "Edit Cluster Watch Mode Override…",
            "Set one cluster's watch mode in the Settings draft.",
            EDIT_CLUSTER_WATCH_MODE,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<ResetClusterOverrides>(
            "Reset Cluster Overrides…",
            "Clear one cluster's thresholds, alerts and watch mode overrides in the Settings draft.",
            RESET_CLUSTER_OVERRIDES,
        )
        .of(C::Flint, S::Settings),
        Spec::with_args::<AddClusterOverride>(
            "Add Cluster Override…",
            "Give a cluster a row in Settings' Cluster overrides.",
            ADD_CLUSTER_OVERRIDE,
        )
        .of(C::Flint, S::Settings),
        // The view's own Tab cycle, so its controls work without a mouse and Tab never reaches
        // the panes hidden under it.
        Spec::unit::<NextSettingsControl>(
            "Next Settings Control",
            "Move the cursor to the next control in Settings.",
        )
        .of(C::Flint, S::Settings)
        .keys(&["tab", "down"], Some(super::settings_view::SETTINGS_KEYS))
        .hidden(),
        Spec::unit::<PreviousSettingsControl>(
            "Previous Settings Control",
            "Move the cursor to the previous control in Settings.",
        )
        .of(C::Flint, S::Settings)
        .keys(&["shift-tab", "up"], Some(super::settings_view::SETTINGS_KEYS))
        .hidden(),
        Spec::unit::<ActivateSettingsControl>(
            "Use Settings Control",
            "Work the control under the cursor in Settings, as a click does.",
        )
        .of(C::Flint, S::Settings)
        .keys(&["enter", "space"], Some(super::settings_view::SETTINGS_KEYS))
        .hidden(),
        Spec::unit::<ToggleCommandPalette>(
            "Show Command Palette",
            "Search every action available now, and run one.",
        )
        .of(C::Flint, S::Workspace)
        .keys(&["secondary-shift-p"], Some(WORKSPACE)),
        // Cmd-P, as in VS Code and Zed. On Linux, Ctrl-P: a shell tab passes it to the shell,
        // and in the palette's field it moves the selection up.
        Spec::unit::<ToggleResourceSwitcher>(
            "Go to Resource",
            "Search the active cluster's kinds and the objects in memory, and open one.",
        )
        .of(C::View, S::Workspace)
        .keys(&["secondary-p"], Some(WORKSPACE)),
        Spec::unit::<Hide>("Hide Flint", "Hide Flint's windows.")
            .of(C::Flint, S::App)
            .keys(HIDE, None),
        Spec::unit::<HideOthers>("Hide Others", "Hide the windows of every other app.")
            .of(C::Flint, S::App)
            .keys(HIDE_OTHERS, None),
        Spec::unit::<ShowAll>("Show All", "Show the windows of every app again.")
            .of(C::Flint, S::App),
        Spec::unit::<MinimizeWindow>("Minimize", "Minimize the front window.")
            .of(C::Flint, S::Workspace)
            .keys(MINIMIZE, None),
        Spec::unit::<ZoomWindow>("Zoom", "Zoom the front window, or return it to its earlier size.")
            .of(C::Flint, S::Workspace),
        Spec::unit::<OpenHelp>("Flint Help", "Open Flint's project page in the browser.")
            .of(C::Flint, S::App),
        // The Edit menu, for the text fields (the list filter, the namespace field, dialogs):
        // gpui-component's own actions, which a focused field handles.
        Spec::unit::<input::Undo>("Undo", "Undo the last change in the focused field.")
            .of(C::Flint, S::TextField)
            .keys(&["secondary-z"], Some(TEXT_FIELD)),
        Spec::unit::<input::Redo>("Redo", "Redo the last undone change in the focused field.")
            .of(C::Flint, S::TextField)
            .keys(REDO, Some(TEXT_FIELD)),
        Spec::unit::<input::Cut>("Cut", "Cut the selected text of the focused field.")
            .of(C::Flint, S::TextField)
            .keys(&["secondary-x"], Some(TEXT_FIELD)),
        Spec::unit::<input::Copy>("Copy", "Copy the selected text of the focused field.")
            .of(C::Flint, S::TextField)
            .keys(&["secondary-c"], Some(TEXT_FIELD)),
        Spec::unit::<input::Paste>("Paste", "Paste into the focused field.")
            .of(C::Flint, S::TextField)
            .keys(&["secondary-v"], Some(TEXT_FIELD)),
        Spec::unit::<input::SelectAll>("Select All", "Select all the text of the focused field.")
            .of(C::Flint, S::TextField)
            .keys(&["secondary-a"], Some(TEXT_FIELD)),
        // The palette's own keys, while it is open; it does not list them.
        Spec::unit::<PaletteUp>("Select Previous Command", "Move the palette's selection up.")
            .of(C::Flint, S::Palette)
            .keys(&["up", "ctrl-p"], Some(PALETTE_INPUT))
            .hidden(),
        Spec::unit::<PaletteDown>("Select Next Command", "Move the palette's selection down.")
            .of(C::Flint, S::Palette)
            .keys(&["down", "ctrl-n"], Some(PALETTE_INPUT))
            .hidden(),
        Spec::unit::<PaletteConfirm>(
            "Run Selected Command",
            "Run the palette's selected entry, or ask for the argument it needs.",
        )
        .of(C::Flint, S::Palette)
        .keys(&["enter"], Some(PALETTE_INPUT))
        .hidden(),
        Spec::unit::<PaletteDismiss>(
            "Close Command Palette",
            "Close the palette and give focus back to where it was, or go back a step while it \
             asks for an argument.",
        )
        .of(C::Flint, S::Palette)
        .keys(&["escape"], Some(PALETTE_INPUT))
        .hidden(),
        Spec::unit::<PaletteBack>(
            "Go Back a Step",
            "In the palette's empty field, go back to the previous argument or the actions.",
        )
        .of(C::Flint, S::Palette)
        .keys(&["backspace"], Some(PALETTE_INPUT))
        .hidden(),
        Spec::with_args::<PalettePick>("Run Command in Row…", "Run the palette entry in a row.", PALETTE_ROW)
            .of(C::Flint, S::Palette)
            .hidden(),
        Spec::confirm(),
        Spec::unit::<Cancel>("Cancel", "Dismiss the open dialog.").of(C::Flint, S::Dialog),
        Spec::unit::<ConfirmAlternative>(
            "Choose Alternative",
            "Answer the open dialog with its third choice (Discard).",
        )
        .of(C::Flint, S::Dialog)
        .hidden(),
        // Clusters.
        Spec::with_args::<SelectCluster>(
            "Select Cluster…",
            "Make a cluster the active one and connect to it.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        // The sidebar's keys, while it has focus (UP-33).
        Spec::unit::<SelectPreviousCluster>(
            "Select Previous Cluster",
            "Move the cluster sidebar's selection up.",
        )
        .of(C::Cluster, S::Sidebar)
        .keys(&["up"], Some(SIDEBAR_PLAIN)),
        Spec::unit::<SelectNextCluster>(
            "Select Next Cluster",
            "Move the cluster sidebar's selection down.",
        )
        .of(C::Cluster, S::Sidebar)
        .keys(&["down"], Some(SIDEBAR_PLAIN)),
        Spec::unit::<ActivateSelectedCluster>(
            "Connect to Selected Cluster",
            "Make the cluster selected in the sidebar the active one, and connect to it.",
        )
        .of(C::Cluster, S::Sidebar)
        .keys(&["enter"], Some(SIDEBAR_PLAIN)),
        Spec::with_args::<ExposeToAi>(
            "Expose Cluster to AI…",
            "Let Flint's MCP server read a cluster, after a confirmation naming its server.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<ToggleKeepWatching>(
            "Keep Watching Cluster…",
            "Gather overview data and alerts for a cluster while it is not selected, or stop.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<DisconnectCluster>(
            "Disconnect Cluster…",
            "Stop a cluster's watches, log streams and background watching until it is selected.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<HideFromAi>(
            "Hide Cluster from AI…",
            "Stop exposing a cluster to Flint's MCP server, or remove a stale exposure.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<EditAiScope>(
            "Edit AI Scope…",
            "Choose which namespaces and kinds AI agents may read of an exposed cluster.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<ShowAgentActivity>(
            "Show Agent Activity…",
            "See every call AI agents made to Flint's MCP server on a cluster, as they make them.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<ShowHelmReleases>(
            "Show Helm Releases…",
            "See a cluster's Helm releases: details, history, masked values and manifest.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<SelectHelmRelease>(
            "Select Helm Release…",
            "Select a release in the Helm releases tab.",
            HELM_RELEASE,
        )
        .of(C::Cluster, S::Workspace)
        .hidden(),
        Spec::unit::<RefreshHelmReleases>(
            "Refresh Helm Releases",
            "Read the releases in the Helm releases tab again.",
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<ShowHelmDetails>("Show Helm Details", "Show the selected release's details.")
            .of(C::Cluster, S::Workspace),
        Spec::unit::<ShowHelmHistory>("Show Helm History", "Show the selected release's history.")
            .of(C::Cluster, S::Workspace),
        Spec::unit::<ShowHelmValues>(
            "Show Helm Values",
            "Show the selected release's values, with secrets masked.",
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<ShowHelmManifest>(
            "Show Helm Manifest",
            "Show the selected release's manifest, with secrets masked.",
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<ScopeAllNamespaces>(
            "Scope All Namespaces",
            "In the AI scope pickers, let agents read every namespace.",
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::unit::<ScopeAllKinds>(
            "Scope All Kinds",
            "In the AI scope pickers, let agents read every kind.",
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::unit::<ScopeSelectedNamespaces>(
            "Scope Selected Namespaces",
            "In the AI scope pickers, let agents read only the namespaces selected.",
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::unit::<ScopeSelectedKinds>(
            "Scope Selected Kinds",
            "In the AI scope pickers, let agents read only the kinds selected.",
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::with_args::<ToggleScopeNamespace>(
            "Toggle Scope Namespace…",
            "In the AI scope pickers, add a namespace to the scope or take it out.",
            SCOPE_NAMESPACE,
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::with_args::<ToggleScopeKind>(
            "Toggle Scope Kind…",
            "In the AI scope pickers, add a kind (pods, deployments.apps) to the scope or take it out.",
            SCOPE_KIND,
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::with_args::<UnlockCluster>(
            "Unlock Cluster…",
            "Let Flint change a protected cluster and open shells in it until Flint quits, after \
             a confirmation naming its server.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<RenameCluster>(
            "Rename Cluster…",
            "Choose the name Flint shows for a cluster. kubeconfig is not changed, and AI agents \
             and shells keep the context name.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<SetClusterTag>(
            "Set Cluster Colour Tag…",
            "Mark a cluster with a colour, for the server it points at now. Cosmetic: it changes \
             nothing about protection, trust or AI exposure.",
            CLUSTER_TAG,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<ClearClusterTag>(
            "Clear Cluster Colour Tag…",
            "Remove a cluster's colour tag at the server it points at now.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<LockCluster>(
            "Lock Cluster Again…",
            "Protect a cluster unlocked for this session again: read-only in Flint. Shells \
             already open keep running.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<TrustCluster>(
            "Trust Cluster…",
            "Keep a cluster unlocked across restarts, pinned to its server, after a \
             confirmation naming it.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<UntrustCluster>(
            "Stop Trusting Cluster…",
            "Protect a trusted cluster again, or remove a stale trust. Shells already open keep \
             running.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<RetryConnection>(
            "Retry Connection",
            "Connect to the active cluster again after a failure.",
        )
        .of(C::Cluster, S::Workspace)
        .keys(&["secondary-r"], Some(WORKSPACE)),
        Spec::unit::<CancelConnection>(
            "Cancel Connection",
            "Stop connecting to the active cluster and stop retrying it.",
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<WatchAllKinds>("Watch All Kinds", "Watch every kind on the active cluster.")
            .of(C::Cluster, S::Workspace),
        Spec::unit::<WatchOpenViewsOnly>(
            "Watch On View",
            "Watch the kinds of open views and the problem kinds, on the active cluster.",
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<UseDefaultWatchMode>(
            "Use Default Watch Mode",
            "Let the active cluster follow the default watch mode.",
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<DefaultWatchAllKinds>(
            "Watch All Kinds by Default",
            "Make watching every kind the default for all clusters.",
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<DefaultWatchOpenViewsOnly>(
            "Watch On View by Default",
            "Make watching the kinds of open views and the problem kinds the default for all clusters.",
        )
        .of(C::Cluster, S::Workspace),
        // Navigation and panes.
        Spec::with_args::<OpenView>(
            "Open Kind…",
            "Open the list of a kind on the active cluster.",
            KIND,
        )
        .of(C::View, S::Workspace),
        Spec::unit::<OpenClusterOverview>(
            "Open Cluster Overview",
            "Show what needs attention on the active cluster.",
        )
        .of(C::View, S::Navigation),
        Spec::with_args::<super::dashboard::OpenProblemList>(
            "Open Problem List…",
            "Open the objects behind a problem tile of the cluster overview.",
            PROBLEM_CATEGORY,
        )
        .of(C::View, S::Dashboard)
        .hidden(),
        Spec::with_args::<super::dashboard::OpenShortWorkloads>(
            "Open Short Workloads…",
            "Open one kind of the workloads the cluster overview counts as short.",
            SHORT_WORKLOAD_KIND,
        )
        .of(C::View, S::Dashboard)
        .hidden(),
        // Tab moves between panes, so the arrows move between the overview's links.
        Spec::unit::<super::dashboard::FocusNextLink>(
            "Focus Next Overview Link",
            "Move focus to the next link on the cluster overview.",
        )
        .of(C::View, S::Dashboard)
        .keys(&["down"], Some(DASHBOARD_PLAIN))
        .hidden(),
        Spec::unit::<super::dashboard::FocusPreviousLink>(
            "Focus Previous Overview Link",
            "Move focus to the previous link on the cluster overview.",
        )
        .of(C::View, S::Dashboard)
        .keys(&["up"], Some(DASHBOARD_PLAIN))
        .hidden(),
        Spec::with_args::<ToggleNavSection>(
            "Toggle Navigation Section…",
            "Open or close a section of the resource navigation.",
            SECTION,
        )
        .of(C::View, S::Navigation)
        .hidden(),
        // The navigation's keys, while it has focus (UP-33).
        Spec::unit::<SelectPreviousKind>(
            "Select Previous Kind",
            "Move the navigation's selection up.",
        )
        .of(C::View, S::Navigation)
        .keys(&["up"], Some(NAVIGATION_PLAIN)),
        Spec::unit::<SelectNextKind>("Select Next Kind", "Move the navigation's selection down.")
            .of(C::View, S::Navigation)
            .keys(&["down"], Some(NAVIGATION_PLAIN)),
        Spec::unit::<OpenSelectedKind>(
            "Open Selected Kind",
            "Open the kind selected in the navigation, or open or close the selected section.",
        )
        .of(C::View, S::Navigation)
        .keys(&["enter"], Some(NAVIGATION_PLAIN)),
        Spec::unit::<ExpandSection>("Expand Section", "Open the selected navigation section.")
            .of(C::View, S::Navigation)
            .keys(&["right"], Some(NAVIGATION_PLAIN)),
        Spec::unit::<CollapseSection>(
            "Collapse Section",
            "Close the selected navigation section, or the section of the selected kind.",
        )
        .of(C::View, S::Navigation)
        .keys(&["left"], Some(NAVIGATION_PLAIN)),
        Spec::with_args::<OpenObject>(
            "Open Object…",
            "Open a kind's list with one of its objects selected and its details shown.",
            OBJECT,
        )
        .of(C::View, S::Workspace)
        .hidden(),
        Spec::unit::<GoBack>("Go Back", "Go back to the kind and object shown before.")
            .of(C::View, S::Workspace)
            .keys(&["secondary-["], Some(WORKSPACE)),
        Spec::unit::<BackToList>(
            "Back to List",
            "In a narrow window, return from the detail to the list, keeping the selection.",
        )
        .of(C::View, S::Workspace)
        .keys(&["secondary-shift-b"], Some(WORKSPACE)),
        // Tab and Shift-Tab, outside text fields: a field keeps them for moving between
        // controls, and a shell tab sends them to the shell.
        Spec::unit::<FocusNextPane>(
            "Focus Next Pane",
            "Move focus to the next pane: clusters, navigation, list, detail, dock.",
        )
        .of(C::View, S::Workspace)
        .keys(&["tab"], Some(PLAIN)),
        Spec::unit::<FocusPreviousPane>(
            "Focus Previous Pane",
            "Move focus to the previous pane: dock, detail, list, navigation, clusters.",
        )
        .of(C::View, S::Workspace)
        .keys(&["shift-tab"], Some(PLAIN)),
        Spec::with_args::<SetPaneWidth>(
            "Set Pane Width…",
            "Resize the clusters, navigation or detail pane.",
            PANE,
        )
        .of(C::View, S::Workspace)
        .hidden(),
        // Namespaces.
        Spec::with_args::<ToggleNamespace>(
            "Toggle Namespace in Filter…",
            "Add a namespace to the filter, or remove it.",
            NAMESPACE,
        )
        .of(C::Namespace, S::Workspace),
        Spec::with_args::<AddNamespace>(
            "Add Namespace to Filter…",
            "Add a namespace to the filter by name, also one you may not list.",
            NAMESPACE,
        )
        .of(C::Namespace, S::Workspace),
        Spec::unit::<ShowAllNamespaces>("Show All Namespaces", "Clear the namespace filter.")
            .of(C::Namespace, S::Workspace),
        Spec::unit::<OpenNamespaceFilter>(
            "Open Namespace Filter",
            "Open the namespace filter's picker.",
        )
        .of(C::Namespace, S::Workspace),
        Spec::unit::<NamespacePickerNext>(
            "Next Namespace Control",
            "Move focus to the namespace picker's next control.",
        )
        .of(C::Namespace, S::Workspace)
        .keys(&["tab", "down"], Some(NAMESPACE_PICKER_KEYS))
        .hidden(),
        Spec::unit::<NamespacePickerPrevious>(
            "Previous Namespace Control",
            "Move focus to the namespace picker's previous control.",
        )
        .of(C::Namespace, S::Workspace)
        .keys(&["shift-tab", "up"], Some(NAMESPACE_PICKER_KEYS))
        .hidden(),
        Spec::with_args::<SetNamespaces>(
            "Set Namespace Filter…",
            "Show only these namespaces (remembered per cluster).",
            NAMESPACES,
        )
        .of(C::Namespace, S::Workspace)
        .hidden(),
        // Resource list.
        Spec::unit::<FocusFilter>("Focus Filter", "Move focus to the list's text filter.")
            .of(C::List, S::List)
            .keys(&["secondary-f"], Some(WORKSPACE)),
        Spec::unit::<FocusRows>(
            "Focus Rows",
            "Move focus from the filter back to the rows, keeping the filter text.",
        )
        .of(C::List, S::List)
        .keys(&["escape", "enter"], Some(LIST_FILTER))
        .hidden(),
        Spec::unit::<SelectFromFilter>(
            "Select From Filter",
            "Move focus from the filter to the rows, selecting the first if none is selected.",
        )
        .of(C::List, S::List)
        .keys(&["down"], Some(LIST_FILTER))
        .hidden(),
        Spec::unit::<SelectPrev>("Select Previous Row", "Move the selection up.")
            .of(C::List, S::List)
            .keys(&["up"], Some(PLAIN)),
        Spec::unit::<SelectNext>("Select Next Row", "Move the selection down.")
            .of(C::List, S::List)
            .keys(&["down"], Some(PLAIN)),
        Spec::unit::<SelectFirst>("Select First Row", "Select the first row.")
            .of(C::List, S::List)
            .keys(&["home"], Some(PLAIN)),
        Spec::unit::<SelectLast>("Select Last Row", "Select the last row.")
            .of(C::List, S::List)
            .keys(&["end"], Some(PLAIN)),
        Spec::unit::<ToggleWide>(
            "Toggle Wide Columns",
            "Show or hide the extra columns of kubectl get -o wide.",
        )
        .of(C::List, S::List),
        Spec::unit::<ToggleProblemsOnly>(
            "Toggle Problems Only",
            "Show only the rows with problems, in a list of pods, workloads or nodes.",
        )
        .of(C::List, S::List),
        Spec::unit::<CloseDetail>(
            "Clear Selection",
            "Clear the selection, which closes the detail pane.",
        )
        .of(C::List, S::List)
        .keys(&["escape"], Some(LIST_ALONE))
        .hidden(),
        Spec::unit::<OpenSelectedRow>(
            "Open Selected Row",
            "Show the selected row's details, as clicking it does.",
        )
        .of(C::List, S::List)
        .keys(&["enter"], Some(LIST_ALONE))
        .hidden(),
        Spec::with_args::<SelectRow>(
            "Select Object…",
            "Select an object in the open list and show its details.",
            ROW,
        )
        .of(C::List, S::List)
        .hidden(),
        Spec::with_args::<SortBy>(
            "Sort by Column…",
            "Sort by a column: ascending, descending, then unsorted.",
            COLUMN,
        )
        .of(C::List, S::List),
        // Cluster sets.
        Spec::unit::<ToggleClusterPicking>(
            "Choose Several Clusters",
            "Start or stop choosing several clusters in the sidebar.",
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::with_args::<TogglePickedCluster>(
            "Toggle Cluster in Choice…",
            "Add a cluster to the sidebar's choice, or take it out.",
            CLUSTER,
        )
        .of(C::Cluster, S::ClusterSets)
        .hidden(),
        Spec::unit::<OpenPickedClusters>(
            "Open Chosen Clusters",
            "Run the chosen clusters as a set and open their combined list.",
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::with_args::<SaveClusterSet>(
            "Save Cluster Set…",
            "Save the chosen clusters under a name.",
            SET_NAME,
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::with_args::<OpenClusterSet>(
            "Open Cluster Set…",
            "Run a saved set and open its combined list.",
            SET_NAME,
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::with_args::<RenameClusterSet>(
            "Rename Cluster Set…",
            "Give a saved set a new name.",
            SET_NAME,
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::with_args::<DeleteClusterSet>(
            "Delete Cluster Set…",
            "Delete a saved set, after confirming. Its clusters are not changed.",
            SET_NAME,
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::unit::<CloseClusterSet>(
            "Close Cluster Set",
            "Stop the running set and close its combined list.",
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::unit::<OpenClusterSetDashboard>(
            "Open Cluster Set Dashboard",
            "Show a card per cluster of the running set, worst first.",
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::unit::<ShowClusterSetList>(
            "Show Cluster Set List",
            "Go back from the set's dashboard to its combined list.",
        )
        .of(C::Cluster, S::ClusterSets),
        Spec::unit::<RestartAcrossSet>(
            "Restart Selected Workload Across Set",
            "Restart the selected workload on every member of the running set, one at a time, \
             after a per-member plan and one confirmation.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<ScaleAcrossSet>(
            "Scale Selected Workload Across Set",
            "Ask for a replica count, then scale the selected workload on every member of the \
             running set, one at a time, after a per-member plan and one confirmation.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<ApplyAcrossSet>(
            "Apply YAML to Selected Object Across Set",
            "Edit a manifest of the selected object, then apply it on every member of the \
             running set, one at a time, after a per-member dry run and one confirmation.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<DeleteAcrossSet>(
            "Delete Selected Object Across Set",
            "Delete the selected object on every member of the running set, one at a time, \
             after a per-member dry run and one confirmation.",
        )
        .of(C::Detail, S::Workspace),
        Spec::with_args::<OpenSetMember>(
            "Open Set Member…",
            "Open the combined list filtered to one member.",
            CLUSTER,
        )
        .of(C::Cluster, S::ClusterSets)
        .hidden(),
        Spec::with_args::<OpenSetProblem>(
            "Open Set Problem…",
            "Open the combined list filtered to what a dashboard count counted.",
            SET_PROBLEM,
        )
        .of(C::Cluster, S::ClusterSets)
        .hidden(),
        // The combined list.
        Spec::with_args::<SelectCombinedRow>(
            "Select Combined Row…",
            "Select a row of the combined list by its cluster and uid.",
            COMBINED_ROW,
        )
        .of(C::List, S::CombinedList)
        .hidden(),
        Spec::with_args::<SortCombined>(
            "Sort Combined List…",
            "Sort the combined list by a column.",
            COMBINED_COLUMN,
        )
        .of(C::List, S::CombinedList)
        .hidden(),
        Spec::with_args::<ToggleClusterFilter>(
            "Filter Combined List by Cluster…",
            "Add a cluster to the combined list's filter, or take it out.",
            CLUSTER,
        )
        .of(C::List, S::CombinedList)
        .hidden(),
        Spec::with_args::<ToggleClusterGroup>(
            "Collapse Cluster Group…",
            "Collapse or expand a cluster's group in the combined list.",
            CLUSTER,
        )
        .of(C::List, S::CombinedList)
        .hidden(),
        Spec::unit::<ToggleGroupByCluster>(
            "Group Combined List by Cluster",
            "Turn grouping the combined list by cluster on or off.",
        )
        .of(C::List, S::CombinedList),
        Spec::unit::<ToggleCombinedWide>(
            "Toggle Wide Combined Columns",
            "Show or hide the combined list's extra columns.",
        )
        .of(C::List, S::CombinedList),
        Spec::unit::<ToggleCombinedProblems>(
            "Toggle Combined Problems Only",
            "Show only the combined list's rows with problems.",
        )
        .of(C::List, S::CombinedList),
        Spec::unit::<ShowAllCombinedRows>(
            "Show All Combined Rows",
            "Show every row again after a dashboard narrowed the combined list.",
        )
        .of(C::List, S::CombinedList),
        // Detail pane.
        Spec::unit::<Close>("Close Detail", "Close the detail pane.")
            .of(C::Detail, S::Detail)
            .keys(&["escape"], Some(PLAIN)),
        Spec::unit::<ShowOverview>("Show Overview", "Show the object's overview.")
            .of(C::Detail, S::Detail)
            .keys(&["secondary-1"], Some(WORKSPACE)),
        Spec::unit::<ShowYaml>("Show YAML", "Show the object as YAML.")
            .of(C::Detail, S::Detail)
            .keys(&["secondary-2"], Some(WORKSPACE)),
        Spec::unit::<Refresh>("Refresh Object", "Fetch the shown object again.")
            .of(C::Detail, S::Detail)
            .keys(&["secondary-r"], Some(WORKSPACE)),
        Spec::unit::<ToggleManagedFields>(
            "Toggle Managed Fields",
            "Show or hide metadata.managedFields in the YAML.",
        )
        .of(C::Detail, S::Detail),
        Spec::unit::<EditYaml>("Edit YAML", "Edit the object's YAML.").of(C::Detail, S::Detail),
        Spec::unit::<ReviewYamlEdit>(
            "Review YAML Edit",
            "Run a server dry run of the edit and show its diff.",
        )
        .of(C::Detail, S::Detail),
        Spec::unit::<ApplyYamlEdit>(
            "Apply YAML Edit",
            "Apply the reviewed edit, after a confirmation.",
        )
        .of(C::Detail, S::Detail),
        Spec::unit::<ReloadYamlEdit>(
            "Reload YAML Edit",
            "Reload an object that changed, keep the edit, and review it again.",
        )
        .of(C::Detail, S::Detail),
        Spec::unit::<BackToYamlEdit>("Back to YAML Edit", "Go back from the diff to the editor.")
            .of(C::Detail, S::Detail),
        Spec::unit::<CancelYamlEdit>("Cancel YAML Edit", "Discard the edit.")
            .of(C::Detail, S::Detail),
        Spec::unit::<ToggleLabels>("Toggle Labels", "Expand or collapse the labels.")
            .of(C::Detail, S::Detail),
        Spec::unit::<ToggleAnnotations>(
            "Toggle Annotations",
            "Expand or collapse the annotations.",
        )
        .of(C::Detail, S::Detail),
        Spec::with_args::<ToggleSecretValue>(
            "Toggle Secret Value…",
            "Reveal or hide one value of a Secret.",
            KEY,
        )
        .of(C::Detail, S::Detail)
        .hidden(),
        Spec::unit::<OpenLogs>(
            "Open Logs",
            "Open the selected pod's logs, or every pod of the selected workload merged, in the dock.",
        )
            .of(C::Logs, S::Detail)
            .keys(&["secondary-l"], Some(WORKSPACE)),
        Spec::unit::<OpenPreviousLogs>(
            "Open Previous Logs",
            "Open the logs of the previous instance of the selected pod's container.",
        )
        .of(C::Logs, S::Detail)
        .keys(&["secondary-shift-l"], Some(WORKSPACE)),
        Spec::with_args::<OpenContainerLogs>(
            "Open Container Logs…",
            "Open the logs of one container of the shown pod, or of its previous instance.",
            CONTAINER,
        )
        .of(C::Logs, S::Detail),
        Spec::unit::<OpenShell>(
            "Open Shell",
            "Open a shell (bash, else sh) in the selected pod, or the selected workload's first ready pod, in the dock.",
        )
        .of(C::Terminal, S::Detail)
        .keys(&["secondary-shift-e"], Some(WORKSPACE)),
        Spec::with_args::<OpenContainerShell>(
            "Open Container Shell…",
            "Open a shell (bash, else sh) in one container of the shown pod, in the dock.",
            SHELL_CONTAINER,
        )
        .of(C::Terminal, S::Detail),
        Spec::unit::<ForwardPort>(
            "Forward Port",
            "Forward a local port to the selected pod or service. It listens on 127.0.0.1 only.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<ShowPortForwards>(
            "Show Port Forwards",
            "Show the active cluster's port forwards in the dock.",
        )
        .of(C::View, S::Workspace),
        Spec::unit::<StopAllPortForwards>(
            "Stop All Port Forwards",
            "Stop every port forward in this window.",
        )
        .of(C::View, S::Workspace),
        Spec::unit::<StartPortForward>(
            "Start Port Forward",
            "Start the forward the port forward panel's form describes.",
        )
        .of(C::View, S::Workspace)
        .hidden(),
        Spec::with_args::<StopPortForward>(
            "Stop Port Forward…",
            "Stop one port forward, or dismiss its row once it has ended.",
            FORWARD_ID,
        )
        .of(C::View, S::Workspace)
        .hidden(),
        Spec::with_args::<OpenForwardedPort>(
            "Open Forwarded Port…",
            "Open a running port forward's local address in the browser.",
            FORWARD_ID,
        )
        .of(C::View, S::Workspace)
        .hidden(),
        Spec::unit::<DeleteObject>(
            "Delete Selected Object",
            "Delete the selected object, after a review and a confirmation naming it.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<ScaleObject>(
            "Scale Selected Workload",
            "Ask for a replica count for the selected workload, then review and confirm it.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<RestartObject>(
            "Restart Selected Workload",
            "Restart the pods of the selected Deployment, DaemonSet or StatefulSet, after a \
             confirmation.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<CordonNode>(
            "Cordon Selected Node",
            "Stop new pods landing on the selected node, after a confirmation.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<UncordonNode>(
            "Uncordon Selected Node",
            "Let new pods land on the selected node again, after a confirmation.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<DrainNode>(
            "Drain Selected Node",
            "Choose what a drain of the selected node may evict, then review and confirm it.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<ShowDrainReport>(
            "Show Drain Report",
            "Open what the latest drain did, also when it stopped early.",
        )
        .of(C::Detail, S::Workspace),
        Spec::unit::<OpenCompare>(
            "Compare Across Clusters",
            "Compare the selected object across the connected clusters. Read-only.",
        )
        .of(C::Detail, S::Workspace),
        Spec::with_args::<CompareObject>(
            "Compare Object Across Clusters…",
            "Compare one exact object across the connected clusters. Read-only.",
            COMPARE_OBJECT,
        )
        .of(C::Detail, S::Workspace)
        .hidden(),
        Spec::unit::<RefreshCompare>("Refresh Comparison", "Read the compared object again.")
            .of(C::Detail, S::Compare)
            .keys(&["secondary-r"], Some(COMPARE_VIEW)),
        Spec::unit::<ToggleCompareSame>("Toggle Matching Fields", "Show the fields that match.")
            .of(C::Detail, S::Compare),
        Spec::unit::<ToggleCompareVolatile>(
            "Toggle Cluster-Assigned Fields",
            "Include the fields each cluster assigns for itself.",
        )
        .of(C::Detail, S::Compare),
        Spec::with_args::<ToggleFleetTarget>(
            "Toggle Fleet Target…",
            "In a fleet action's target list, take a cluster out of the run or put it back.",
            CLUSTER,
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::unit::<EditFleetManifestAgain>(
            "Edit Fleet Manifest Again",
            "In a fleet apply's target list, go back to the manifest to edit and review it again.",
        )
        .of(C::Cluster, S::Dialog)
        .hidden(),
        Spec::with_args::<ToggleDrainOption>(
            "Toggle Drain Option…",
            "In the drain dialog, turn an option on or off.",
            DRAIN_OPTION,
        )
        .of(C::Detail, S::Dialog)
        .hidden(),
        Spec::with_args::<OpenSelectorLogs>(
            "Open Logs by Label Selector…",
            "Open the merged logs of every pod matching a label selector on the active cluster, in the dock.",
            SELECTOR,
        )
        .of(C::Logs, S::Workspace),
        // The dock.
        Spec::unit::<ToggleDock>("Toggle Dock", "Show or hide the bottom dock.")
            .of(C::View, S::Dock)
            .keys(&["secondary-j"], Some(WORKSPACE)),
        Spec::unit::<CloseActiveTab>(
            "Close Dock Tab",
            "Close the dock's active tab, ending its session. Its key, Cmd-W, closes the window \
             when the dock shows no tab.",
        )
        .of(C::View, S::Dock)
        // Wherever focus is in the window, not only in the dock (#177).
        .keys(&["secondary-w"], Some(WORKSPACE)),
        Spec::unit::<NextTab>("Next Dock Tab", "Show the dock's next tab.")
            .of(C::View, S::Dock)
            .keys(&["ctrl-tab"], Some(DOCK)),
        Spec::unit::<PrevTab>("Previous Dock Tab", "Show the dock's previous tab.")
            .of(C::View, S::Dock)
            .keys(&["ctrl-shift-tab"], Some(DOCK)),
        Spec::unit::<ShowTabList>(
            "Show Dock Tabs",
            "List every dock tab, with its cluster and status, and show the one picked.",
        )
        .of(C::View, S::Dock)
        .keys(&["secondary-shift-j"], Some(WORKSPACE)),
        Spec::unit::<ReconnectLogs>(
            "Reconnect Logs",
            "Open the active log tab's stream again once it ended or failed, in a new tab beside \
             it (the ended tab keeps its lines), on the API server its context points at now.",
        )
        .of(C::Logs, S::Dock)
        .keys(&["secondary-r"], Some(DOCK)),
        Spec::unit::<RestartShell>(
            "Restart Shell",
            "Start a new shell once the active shell tab's exited or failed, in a new tab beside \
             it (the ended tab keeps its output), on the API server its context points at now.",
        )
        .of(C::Terminal, S::Dock)
        .keys(RESTART_SHELL, Some(DOCK)),
        Spec::with_args::<ActivateTab>("Show Dock Tab…", "Show a dock tab by its id.", TAB)
            .of(C::View, S::Dock)
            .hidden(),
        Spec::with_args::<CloseTab>(
            "Close Dock Tab by Id…",
            "Close a dock tab by its id, ending its session.",
            TAB,
        )
        .of(C::View, S::Dock)
        .hidden(),
        Spec::with_args::<SetDockHeight>("Set Dock Height…", "Resize the bottom dock.", HEIGHT)
            .of(C::View, S::Workspace)
            .hidden(),
        // A log tab.
        Spec::unit::<TogglePause>(
            "Toggle Pause",
            "Pause following (new lines are held back), or resume it.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["space"], Some(LOG_PLAIN)),
        Spec::unit::<JumpToEnd>(
            "Jump to Newest Line",
            "Scroll to the newest line and follow again, resuming a pause.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["end", "secondary-down"], Some(LOG_PLAIN)),
        Spec::unit::<JumpToStart>("Jump to Oldest Line", "Scroll to the oldest line shown.")
            .of(C::Logs, S::Logs)
            .keys(&["home", "secondary-up"], Some(LOG_PLAIN)),
        Spec::unit::<ToggleTimestamps>(
            "Toggle Timestamps",
            "Show or hide each line's timestamp, without reloading.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["t"], Some(LOG_PLAIN)),
        Spec::unit::<ToggleSources>(
            "Toggle Pod Prefix",
            "Show or hide each workload line's pod and container, without reloading.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["p"], Some(LOG_PLAIN)),
        Spec::unit::<ToggleWrap>("Toggle Line Wrap", "Wrap long lines, or cut them at the edge.")
            .of(C::Logs, S::Logs)
            .keys(&["w"], Some(LOG_PLAIN)),
        Spec::unit::<ToggleMatchingOnly>(
            "Toggle Matching Lines Only",
            "Show only the lines matching the search, with their context, or every line.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["m"], Some(LOG_PLAIN)),
        Spec::unit::<ToggleErrorsOnly>(
            "Toggle Errors Only",
            "Show only warnings and errors, or every line.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["e"], Some(LOG_PLAIN)),
        Spec::unit::<ToggleLogRegex>(
            "Toggle Regex Search",
            "Read the log search as a regular expression, or as plain text.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["secondary-alt-r"], Some(LOG_VIEW)),
        Spec::with_args::<SetLogContext>(
            "Set Context Lines…",
            "How many lines to show around each line a filtered log tab shows.",
            LOG_CONTEXT,
        )
        .of(C::Logs, S::Logs),
        Spec::unit::<FocusSearch>("Search Logs", "Move focus to the log search field.")
            .of(C::Logs, S::Logs)
            .keys(&["secondary-f"], Some(LOG_VIEW)),
        Spec::unit::<NextMatch>("Next Match", "Go to the next line matching the search.")
            .of(C::Logs, S::Logs)
            .keys(&["secondary-g"], Some(LOG_VIEW)),
        Spec::unit::<PrevMatch>(
            "Previous Match",
            "Go to the previous line matching the search.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["secondary-shift-g"], Some(LOG_VIEW)),
        Spec::unit::<SelectPrevLine>(
            "Select Previous Line",
            "Select the line above, or the newest line.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["up"], Some(LOG_PLAIN)),
        Spec::unit::<SelectNextLine>(
            "Select Next Line",
            "Select the line below, or the newest line.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["down"], Some(LOG_PLAIN)),
        Spec::unit::<ExtendSelectionUp>(
            "Extend Selection Up",
            "Extend the line selection one line up.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["shift-up"], Some(LOG_PLAIN)),
        Spec::unit::<ExtendSelectionDown>(
            "Extend Selection Down",
            "Extend the line selection one line down.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["shift-down"], Some(LOG_PLAIN)),
        Spec::unit::<SelectAllLines>("Select All Lines", "Select every line shown.")
            .of(C::Logs, S::Logs)
            .keys(&["secondary-a"], Some(LOG_PLAIN)),
        Spec::unit::<ClearSelection>(
            "Clear Line Selection",
            "Clear the line selection and resume following.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["escape"], Some(LOG_PLAIN)),
        Spec::unit::<LeaveSearch>(
            "Leave Log Search",
            "Clear the log search; when it is empty, move focus back to the lines.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["escape"], Some(LOG_SEARCH))
        .hidden(),
        Spec::unit::<CopyLines>(
            "Copy Selected Lines",
            "Copy the selected lines as shown, without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["secondary-c"], Some(LOG_PLAIN)),
        Spec::unit::<SaveLogs>(
            "Save Logs",
            "Save every line shown to a file, as shown and without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["secondary-s"], Some(LOG_VIEW)),
        Spec::unit::<SaveMatchingLines>(
            "Save Matching Lines",
            "Save the lines matching the search to a file, as shown and without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["secondary-shift-s"], Some(LOG_VIEW)),
        Spec::unit::<SaveSelectedLines>(
            "Save Selected Lines",
            "Save the selected lines to a file, as shown and without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["secondary-alt-s"], Some(LOG_VIEW)),
        Spec::with_args::<SaveLines>(
            "Save Log Lines…",
            "Save all lines, the search matches or the selection to a file, as shown and without redaction.",
            SAVE,
        )
        .of(C::Logs, S::Logs),
        Spec::with_args::<SelectLine>(
            "Select Log Line…",
            "Select a log line by id, or extend the selection to it.",
            LINE,
        )
        .of(C::Logs, S::Logs)
        .hidden(),
        Spec::with_args::<DragSelection>(
            "Drag Log Selection…",
            "Extend a selection being dragged to a line.",
            DRAG_LINE,
        )
        .of(C::Logs, S::Logs)
        .hidden(),
        // A shell tab.
        Spec::unit::<CopyText>(
            "Copy Terminal Selection",
            "Copy the text selected in the shell tab.",
        )
        .of(C::Terminal, S::Terminal)
        .keys(TERMINAL_COPY, Some(TERMINAL)),
        Spec::unit::<PasteText>(
            "Paste in Terminal",
            "Paste the clipboard's text into the shell, bracketed when the program asked for it.",
        )
        .of(C::Terminal, S::Terminal)
        .keys(TERMINAL_PASTE, Some(TERMINAL)),
        Spec::unit::<ScrollPageUp>(
            "Scroll Terminal Up a Page",
            "Scroll the shell tab up one screen into its history.",
        )
        .of(C::Terminal, S::Terminal)
        .keys(&["shift-pageup"], Some(TERMINAL)),
        Spec::unit::<ScrollPageDown>(
            "Scroll Terminal Down a Page",
            "Scroll the shell tab down one screen.",
        )
        .of(C::Terminal, S::Terminal)
        .keys(&["shift-pagedown"], Some(TERMINAL)),
        Spec::unit::<ScrollToTop>(
            "Scroll Terminal to Top",
            "Scroll the shell tab to the oldest line it keeps.",
        )
        .of(C::Terminal, S::Terminal)
        .keys(&["shift-home"], Some(TERMINAL)),
        Spec::unit::<ScrollToBottom>(
            "Scroll Terminal to Bottom",
            "Scroll the shell tab back to the bottom, where new output appears.",
        )
        .of(C::Terminal, S::Terminal)
        .keys(&["shift-end"], Some(TERMINAL)),
        Spec::with_args::<SelectText>(
            "Select Terminal Text…",
            "Select the shell tab's text from one screen cell to another.",
            CELLS,
        )
        .of(C::Terminal, S::Terminal)
        .hidden(),
    ];
    specs.into_iter().map(|s| s.0).collect()
});

/// Restart Shell: Cmd-R on macOS; Ctrl-Shift-R elsewhere, since a shell tab hands Ctrl-R to
/// the shell (reverse search).
pub(in crate::ui) const RESTART_SHELL: &[&str] = restart_shell_keys(cfg!(target_os = "macos"));

/// [`RESTART_SHELL`] on macOS (`mac`) or elsewhere.
const fn restart_shell_keys(mac: bool) -> &'static [&'static str] {
    if mac {
        &["secondary-r"]
    } else {
        &["secondary-shift-r"]
    }
}

/// Hide Flint: Cmd-H on macOS. Linux desktops have no such command, and Ctrl-H is a shell's.
const HIDE: &[&str] = if cfg!(target_os = "macos") {
    &["secondary-h"]
} else {
    &[]
};
/// Hide Others: Option-Cmd-H on macOS; nothing elsewhere.
const HIDE_OTHERS: &[&str] = if cfg!(target_os = "macos") {
    &["secondary-alt-h"]
} else {
    &[]
};
/// Minimize: Cmd-M on macOS. Nothing elsewhere: Ctrl-M is Enter in a shell.
const MINIMIZE: &[&str] = if cfg!(target_os = "macos") {
    &["secondary-m"]
} else {
    &[]
};
/// Redo in a text field: Shift-Cmd-Z on macOS, Ctrl-Y elsewhere.
const REDO: &[&str] = if cfg!(target_os = "macos") {
    &["secondary-shift-z"]
} else {
    &["secondary-y"]
};

/// Copy in a shell tab: Cmd-C on macOS; Ctrl-Shift-C elsewhere, where Ctrl-C interrupts the
/// program.
pub(in crate::ui) const TERMINAL_COPY: &[&str] = if cfg!(target_os = "macos") {
    &["secondary-c"]
} else {
    &["secondary-shift-c"]
};
/// Paste in a shell tab: Cmd-V on macOS; Ctrl-Shift-V elsewhere, where Ctrl-V quotes the next
/// key in the shell.
pub(in crate::ui) const TERMINAL_PASTE: &[&str] = if cfg!(target_os = "macos") {
    &["secondary-v"]
} else {
    &["secondary-shift-v"]
};

/// Close Window's Cmd-W, under Close Dock Tab's: see [`init`].
const CLOSE_WINDOW_FALLBACK: &str = "secondary-w";

/// Close Dock Tab in a shell tab: Ctrl+Shift+W on Linux, as its terminals close a tab (over
/// Close Window's binding there); nothing extra on macOS, where Cmd-W already does.
const fn terminal_close_tab_keys(mac: bool) -> &'static [&'static str] {
    if mac { &[] } else { &["secondary-shift-w"] }
}
pub(in crate::ui) const TERMINAL_CLOSE_TAB: &[&str] =
    terminal_close_tab_keys(cfg!(target_os = "macos"));

/// Keys a shell tab sends to its program even where the app or gpui-component bind them: Tab
/// (focus traversal), Esc, Return and the arrows (lists, dialogs), and Ctrl with a letter or
/// symbol (the app's shortcuts on Linux, where they use Ctrl, and gpui-component's Ctrl-C
/// there). They are bound to nothing in the terminal's key context, which outranks every
/// context above it, so they reach its key handler (`terminal_keys`). Cmd shortcuts on macOS
/// and Ctrl-Shift ones elsewhere stay the app's.
pub const TERMINAL_PASSTHROUGH: &[&str] = &[
    "tab",
    "shift-tab",
    "escape",
    "enter",
    "shift-enter",
    "backspace",
    "delete",
    "space",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "ctrl-a",
    "ctrl-b",
    "ctrl-c",
    "ctrl-d",
    "ctrl-e",
    "ctrl-f",
    "ctrl-g",
    "ctrl-h",
    "ctrl-i",
    "ctrl-j",
    "ctrl-k",
    "ctrl-l",
    "ctrl-m",
    "ctrl-n",
    "ctrl-o",
    "ctrl-p",
    "ctrl-q",
    "ctrl-r",
    "ctrl-s",
    "ctrl-t",
    "ctrl-u",
    "ctrl-v",
    "ctrl-w",
    "ctrl-x",
    "ctrl-y",
    "ctrl-z",
    "ctrl-space",
    "ctrl-[",
    "ctrl-]",
    "ctrl-\\",
    "ctrl-/",
    "ctrl-up",
    "ctrl-down",
    "ctrl-left",
    "ctrl-right",
    "alt-up",
    "alt-down",
    "alt-left",
    "alt-right",
];

#[cfg(test)]
pub mod tests {
    use std::collections::HashSet;

    use gpui_kit::Action;
    use serde_json::{Value, json};

    use super::ArgKind;
    use super::{
        ActionError, Bind, Category, Handler, OpenView, Scope, SelectRow, Visibility, actions,
        find, key_hint, key_hint_for,
    };

    #[test]
    fn a_refusal_reads_as_a_sentence_without_the_action_name() {
        let gone = ActionError::rejected::<SelectRow>("no row web in the list as shown");
        assert_eq!(gone.for_person(), "No row web in the list as shown.");
        let unavailable = ActionError::unavailable::<SelectRow>();
        let text = unavailable.for_person();
        assert!(!text.contains("resource_list"), "{text}");
        assert!(text.ends_with('.'), "{text}");
    }

    /// Collects the names of the actions a view binds.
    pub struct Names(Vec<&'static str>);

    impl<V: 'static> Bind<V> for Names {
        fn bind<A: Action>(mut self, _: Handler<V, A>) -> Self {
            self.0.push(A::name_for_type());
            self
        }
    }

    /// Window-level actions the root handles, not the view that owns their scope: they are
    /// `Scope::Workspace` so the native menu disables them without a window.
    const HANDLED_BY_ROOT: [&str; 2] = ["flint::MinimizeWindow", "flint::ZoomWindow"];

    /// Asserts that `bind_actions` binds exactly the registry's actions of `scope`, apart from
    /// those the root handles.
    pub fn assert_binds_scope(scope: Scope, bind_actions: fn(Names) -> Names) {
        let mut bound = bind_actions(Names(Vec::new())).0;
        let mut wanted: Vec<&str> = actions()
            .iter()
            .filter(|s| s.scope == scope && !HANDLED_BY_ROOT.contains(&s.name))
            .map(|s| s.name)
            .collect();
        bound.sort_unstable();
        wanted.sort_unstable();
        assert_eq!(
            bound, wanted,
            "the {scope:?} actions bound differ from the registry's"
        );
    }

    #[test]
    fn names_are_unique_and_labels_consistent() {
        let small = ["to", "by", "in", "from", "of", "on", "the", "a"];
        let mut seen = HashSet::new();
        for spec in actions() {
            assert!(seen.insert(spec.name), "duplicate action {}", spec.name);
            assert!(spec.name.contains("::"), "{} has no namespace", spec.name);
            assert!(!spec.description.is_empty(), "{}", spec.name);
            // Title Case, with "…" exactly on the actions that take arguments.
            assert_eq!(
                spec.label.ends_with('…'),
                spec.takes_args(),
                "{}",
                spec.label
            );
            for word in spec.label.split(' ') {
                let first = word.chars().next().unwrap();
                assert!(
                    first.is_uppercase() || small.contains(&word),
                    "{} is not Title Case",
                    spec.label
                );
            }
            assert!(!spec.description.contains("flint mcp"), "{}", spec.name);
        }
        assert_eq!(find("workspace::OpenView").unwrap().label, "Open Kind…");
        assert_eq!(
            find("workspace::OpenView").unwrap().category,
            Category::View
        );
        assert!(find("workspace::Nope").is_none());
    }

    #[test]
    fn arguments_say_what_they_take() {
        let pane = find("workspace::SetPaneWidth").unwrap();
        assert_eq!(
            pane.args[0].kind,
            ArgKind::Choice(&["clusters", "navigation", "detail"])
        );
        assert_eq!(pane.args[1].kind, ArgKind::Number);
        assert_eq!(
            find("workspace::OpenView").unwrap().args[0].kind,
            ArgKind::Kind
        );
        // Bool is for future toggles that take a value.
        assert_ne!(ArgKind::Bool, ArgKind::Text);
    }

    // The shell tab's Copy and Paste tooltips show their bindings' hints (`terminal_view`):
    // Cmd-C and Cmd-V on macOS, Ctrl-Shift-C and Ctrl-Shift-V elsewhere.
    #[test]
    fn terminal_copy_and_paste_hints_follow_their_bindings() {
        let (copy, paste) = if cfg!(target_os = "macos") {
            ("⌘C", "⌘V")
        } else {
            ("Ctrl+Shift+C", "Ctrl+Shift+V")
        };
        assert_eq!(key_hint(super::TERMINAL_COPY[0]), copy);
        assert_eq!(key_hint(super::TERMINAL_PASTE[0]), paste);
    }

    #[test]
    fn key_hints_use_the_platform_modifier() {
        let hint = key_hint("secondary-shift-g");
        if cfg!(target_os = "macos") {
            assert_eq!(hint, "⇧⌘G");
        } else {
            assert_eq!(hint, "Ctrl+Shift+G");
        }
    }

    // Key hints follow each OS's own menus: Apple's modifier order and key glyphs on macOS,
    // spelled-out names joined by "+" on Linux.
    #[test]
    fn key_hints_read_like_the_os_menus() {
        use gpui_kit::OperatingSystem::{Linux, Mac};
        let cases = [
            ("secondary-shift-e", "⇧⌘E", "Ctrl+Shift+E"),
            ("ctrl-alt-shift-cmd-k", "⌃⌥⇧⌘K", "Ctrl+Alt+Shift+Super+K"),
            ("shift-pageup", "⇧⇞", "Shift+PageUp"),
            ("shift-pagedown", "⇧⇟", "Shift+PageDown"),
            ("shift-home", "⇧↖", "Shift+Home"),
            ("shift-end", "⇧↘", "Shift+End"),
            ("space", "␣", "Space"),
            ("enter", "⏎", "Enter"),
            ("escape", "⎋", "Esc"),
            ("secondary-down", "⌘↓", "Ctrl+Down"),
            ("ctrl-shift-tab", "⌃⇧⇥", "Ctrl+Shift+Tab"),
            ("t", "T", "T"),
            ("f5", "F5", "F5"),
        ];
        for (keys, mac, linux) in cases {
            assert_eq!(key_hint_for(keys, Mac), mac, "{keys} on macOS");
            assert_eq!(key_hint_for(keys, Linux), linux, "{keys} on Linux");
        }
    }

    #[test]
    fn the_palette_hides_automation_only_actions() {
        let hidden: Vec<&str> = actions()
            .iter()
            .filter(|s| s.visibility == Visibility::Hidden)
            .map(|s| s.name)
            .collect();
        for name in [
            "workspace::SetPaneWidth",
            "resource_list::SelectRow",
            "workspace::ToggleNavSection",
            "workspace::SetNamespaces",
            "detail::ToggleSecretValue",
            "resource_list::CloseDetail",
            "settings::EditThreshold",
        ] {
            assert!(hidden.contains(&name), "{name} is in the palette");
        }
        assert!(!hidden.contains(&"detail::Close"));
        // Reachable only from the palette (#29): no key, no menu item, no control.
        for name in [
            "resource_list::SortBy",
            "workspace::SelectCluster",
            "workspace::OpenView",
            "logs::OpenSelectorLogs",
        ] {
            assert!(!hidden.contains(&name), "{name} is hidden from the palette");
        }
        // The palette's own keys are not entries in it.
        for spec in actions().iter().filter(|s| s.scope == Scope::Palette) {
            assert_eq!(spec.visibility, Visibility::Hidden, "{}", spec.name);
        }
    }

    // The standard macOS menu items are actions in category Flint (#29), the Edit menu's for
    // text fields only, so they never take a key from a shell tab.
    #[test]
    fn standard_menu_items_are_registered() {
        for (name, label) in [
            ("flint::About", "About Flint"),
            ("flint::Hide", "Hide Flint"),
            ("flint::HideOthers", "Hide Others"),
            ("flint::ShowAll", "Show All"),
            ("flint::MinimizeWindow", "Minimize"),
            ("flint::ZoomWindow", "Zoom"),
            ("flint::OpenHelp", "Flint Help"),
            ("input::Undo", "Undo"),
            ("input::Redo", "Redo"),
            ("input::Cut", "Cut"),
            ("input::Copy", "Copy"),
            ("input::Paste", "Paste"),
            ("input::SelectAll", "Select All"),
        ] {
            let spec = find(name).unwrap_or_else(|| panic!("{name} is not registered"));
            assert_eq!(spec.label, label);
            assert_eq!(spec.category, Category::Flint, "{name}");
            assert_eq!(spec.visibility, Visibility::Palette, "{name}");
        }
        let mac = cfg!(target_os = "macos");
        let hide: &[&str] = if mac { &["secondary-h"] } else { &[] };
        let hide_others: &[&str] = if mac { &["secondary-alt-h"] } else { &[] };
        assert_eq!(find("flint::Hide").unwrap().keys, hide);
        assert_eq!(find("flint::HideOthers").unwrap().keys, hide_others);
        // Ctrl-M is Enter in a shell, so only macOS has a key for Minimize.
        let minimize: &[&str] = if mac { &["secondary-m"] } else { &[] };
        assert_eq!(find("flint::MinimizeWindow").unwrap().keys, minimize);
        assert!(find("flint::ZoomWindow").unwrap().keys.is_empty());
        assert!(find("flint::OpenHelp").unwrap().keys.is_empty());
        // These two need a window, so that the native menu disables them without one.
        for name in ["flint::MinimizeWindow", "flint::ZoomWindow"] {
            assert_eq!(find(name).unwrap().scope, Scope::Workspace, "{name}");
        }
        assert_eq!(find("flint::OpenHelp").unwrap().scope, Scope::App);
        let redo = if mac {
            "secondary-shift-z"
        } else {
            "secondary-y"
        };
        for (name, key) in [
            ("input::Undo", "secondary-z"),
            ("input::Redo", redo),
            ("input::Cut", "secondary-x"),
            ("input::Copy", "secondary-c"),
            ("input::Paste", "secondary-v"),
            ("input::SelectAll", "secondary-a"),
        ] {
            let spec = find(name).unwrap();
            assert_eq!(spec.keys, [key]);
            assert_eq!(spec.scope, Scope::TextField);
            assert_eq!(spec.key_context, Some(super::TEXT_FIELD));
            assert_ne!(spec.key_context, Some(super::TERMINAL));
        }
    }

    #[test]
    fn builds_actions_from_json_arguments() {
        let open = find("workspace::OpenView").unwrap();
        let action = open.build(json!({"kind": "pods"})).unwrap();
        assert!(open.matches(&*action));
        assert_eq!(
            action.as_any().downcast_ref::<OpenView>(),
            Some(&OpenView {
                kind: "pods".into(),
                ..Default::default()
            })
        );
        let select = find("resource_list::SelectRow").unwrap();
        let row = select.build(json!({"name": "web"})).unwrap();
        assert_eq!(
            row.as_any().downcast_ref::<SelectRow>(),
            Some(&SelectRow {
                namespace: None,
                name: "web".into()
            })
        );
        let confirm = find("ui::Confirm").unwrap();
        assert!(confirm.matches(&*confirm.build(Value::Null).unwrap()));
    }

    #[test]
    fn rejects_bad_arguments() {
        let open = find("workspace::OpenView").unwrap();
        assert!(matches!(
            open.build(json!({})),
            Err(ActionError::InvalidArgs { .. })
        ));
        assert!(matches!(
            open.build(json!({"kind": "pods", "extra": 1})),
            Err(ActionError::InvalidArgs { .. })
        ));
        let quit = find("flint::Quit").unwrap();
        assert!(quit.build(Value::Null).is_ok());
        assert!(quit.build(json!({})).is_ok());
        assert!(quit.build(json!({"now": true})).is_err());
    }

    #[test]
    fn keys_belong_to_actions_without_arguments() {
        for spec in actions() {
            if spec.takes_args() {
                assert!(
                    spec.keys.is_empty(),
                    "{} has keys but needs args",
                    spec.name
                );
            } else {
                assert!(spec.build(Value::Null).is_ok(), "{}", spec.name);
            }
            // Cmd exists only on macOS; on Linux it is the Super key, which the desktop owns.
            for key in spec.keys {
                assert!(
                    !key.to_lowercase().split('-').any(|m| m == "cmd"),
                    "{} binds {key}: use secondary- (Cmd on macOS, Ctrl on Linux)",
                    spec.name
                );
            }
            for key in spec.keys {
                assert!(
                    plain_key_allowed(key, spec.key_context),
                    "{} binds the plain key {key} outside the text-field guard",
                    spec.name
                );
            }
        }
    }

    /// Plain keys never fire while typing in a field, except Esc meant for the log search field
    /// or a Settings threshold field itself (which the field passes on), and the keys the palette's field and the list's
    /// filter hand on (no field there takes Enter, Esc or Down for itself). Keys with a modifier
    /// are not plain.
    fn plain_key_allowed(key: &str, context: Option<&str>) -> bool {
        key.contains('-')
            || context.is_some_and(|c| c.contains("!Input"))
            || (key == "escape" && context == Some(super::LOG_SEARCH))
            || (key == "escape"
                && context == Some(super::super::settings_view::SETTINGS_INPUT_KEYS))
            || context == Some(super::PALETTE_INPUT)
            || (matches!(key, "escape" | "enter" | "down") && context == Some(super::LIST_FILTER))
            || (matches!(key, "tab" | "up" | "down")
                && context == Some(super::NAMESPACE_PICKER_KEYS))
    }

    #[test]
    fn only_esc_may_reach_the_log_search_field() {
        let search = Some(super::LOG_SEARCH);
        assert!(plain_key_allowed("escape", search));
        assert!(!plain_key_allowed("t", search));
        assert!(!plain_key_allowed("enter", search));
        assert!(plain_key_allowed("secondary-f", search));
        assert!(plain_key_allowed("t", Some("LogView && !Input")));
        assert!(!plain_key_allowed("escape", Some("Workspace")));
        // The palette's search field gives its arrows, Enter and Esc to the palette.
        assert!(plain_key_allowed("enter", Some(super::PALETTE_INPUT)));
        assert!(!plain_key_allowed("enter", Some(super::PALETTE)));
        // The list's filter hands Esc, Enter and Down back to the rows, and keeps the rest.
        assert!(plain_key_allowed("down", Some(super::LIST_FILTER)));
        assert!(!plain_key_allowed("t", Some(super::LIST_FILTER)));
    }

    // covers UP-41: on Linux, `secondary-` app shortcuts become Ctrl ones. Every one that is
    // Ctrl with a plain letter (what a shell uses: Ctrl-W, Ctrl-R, Ctrl-L, ...) must be in the
    // terminal's passthrough, so it reaches the shell inside a shell tab. Checked for Linux on
    // every host, since the keys are written once for both.
    #[test]
    fn linux_ctrl_letter_shortcuts_reach_the_shell() {
        use gpui_kit::{Keystroke, OperatingSystem};
        let linux = |k: &str| Keystroke::parse_with_os(k, OperatingSystem::Linux).unwrap();
        let passthrough: Vec<Keystroke> = super::TERMINAL_PASSTHROUGH
            .iter()
            .map(|k| linux(k))
            .collect();
        for spec in actions() {
            if spec.key_context == Some(super::TERMINAL) {
                continue;
            }
            for key in spec.keys {
                let k = linux(key);
                let m = k.modifiers;
                let ctrl_letter = m.control
                    && !m.shift
                    && !m.alt
                    && !m.platform
                    && k.key.len() == 1
                    && k.key.chars().all(|c| c.is_ascii_alphabetic());
                if ctrl_letter {
                    assert!(
                        passthrough.contains(&k),
                        "{} binds {key} (Ctrl-{} on Linux), which a shell tab must pass through",
                        spec.name,
                        k.key
                    );
                }
            }
        }
    }

    // covers UP-41: the keys a shell needs are not the app's inside a shell tab, and the tab's
    // own shortcuts are not among them.
    #[test]
    fn shell_keys_and_terminal_shortcuts_do_not_overlap() {
        for spec in actions() {
            if spec.key_context != Some(super::TERMINAL) {
                continue;
            }
            for key in spec.keys {
                assert!(
                    !super::TERMINAL_PASSTHROUGH.contains(key),
                    "{} binds {key}, which the shell gets",
                    spec.name
                );
            }
        }
        for key in [
            "tab", "escape", "enter", "up", "ctrl-c", "ctrl-d", "ctrl-w", "ctrl-l",
        ] {
            assert!(super::TERMINAL_PASSTHROUGH.contains(&key), "{key}");
        }
        // Restart Shell's key reaches the dock from a shell tab on both systems: on Linux,
        // where Cmd is Ctrl, Ctrl-R would go to the shell.
        for (mac, os) in [
            (true, gpui_kit::OperatingSystem::Mac),
            (false, gpui_kit::OperatingSystem::Linux),
        ] {
            for key in super::restart_shell_keys(mac) {
                let keystroke = gpui_kit::Keystroke::parse_with_os(key, os).unwrap();
                let passed = super::TERMINAL_PASSTHROUGH
                    .iter()
                    .any(|p| gpui_kit::Keystroke::parse_with_os(p, os).unwrap() == keystroke);
                assert!(!passed, "{key} on {os:?} goes to the shell");
            }
        }
        // Copy and paste use Cmd on macOS; elsewhere Ctrl-C and Ctrl-V are the shell's.
        let mac = cfg!(target_os = "macos");
        assert_eq!(
            super::TERMINAL_COPY,
            if mac {
                &["secondary-c"]
            } else {
                &["secondary-shift-c"]
            }
        );
        assert_eq!(
            super::TERMINAL_PASTE,
            if mac {
                &["secondary-v"]
            } else {
                &["secondary-shift-v"]
            }
        );
    }

    // covers UP-42 (#177): Cmd-W closes the dock's tab before the window. Close Window's
    // fallback Cmd-W is loaded before Close Dock Tab's, so it is lower in precedence, and it is
    // not one of Close Window's own keys, which the menus and the palette show.
    #[test]
    fn cmd_w_reaches_the_dock_tab_before_the_window() {
        let position = |name: &str| actions().iter().position(|s| s.name == name).unwrap();
        assert!(position("flint::CloseWindow") < position("dock::CloseActiveTab"));
        let close_window = find("flint::CloseWindow").unwrap();
        assert_eq!(close_window.keys, ["secondary-shift-w"]);
        assert!(!close_window.keys.contains(&super::CLOSE_WINDOW_FALLBACK));
        assert_eq!(find("dock::CloseActiveTab").unwrap().keys, ["secondary-w"]);
    }

    // covers UP-42 (#177): on Linux, Ctrl+Shift+W in a shell tab closes the tab, as Linux
    // terminals do, instead of the window (Close Window's key elsewhere); the shell never gets
    // it. On macOS nothing extra is bound: Cmd-W already closes the tab. Checked for both systems
    // on every host, like the other per-OS keys.
    #[test]
    fn linux_ctrl_shift_w_in_a_shell_closes_the_tab() {
        use gpui_kit::{Keystroke, OperatingSystem};
        assert!(super::terminal_close_tab_keys(true).is_empty());
        let linux = super::terminal_close_tab_keys(false);
        assert_eq!(linux, ["secondary-shift-w"]);
        assert_eq!(linux, find("flint::CloseWindow").unwrap().keys);
        let parse = |k: &str| Keystroke::parse_with_os(k, OperatingSystem::Linux).unwrap();
        let ctrl_shift_w = parse(linux[0]);
        assert!(ctrl_shift_w.modifiers.control && ctrl_shift_w.modifiers.shift);
        assert!(
            !super::TERMINAL_PASSTHROUGH
                .iter()
                .any(|p| parse(p) == ctrl_shift_w),
            "the shell would get it"
        );
        assert_eq!(
            super::TERMINAL_CLOSE_TAB,
            super::terminal_close_tab_keys(cfg!(target_os = "macos"))
        );
    }

    #[test]
    fn errors_read_well() {
        assert_eq!(
            ActionError::Unknown("x::Y".into()).to_string(),
            "unknown action \"x::Y\""
        );
        assert_eq!(
            ActionError::NotAvailable("detail::Close".into()).to_string(),
            "detail::Close is not available in the current view"
        );
        assert_eq!(
            ActionError::rejected::<OpenView>("no such kind").to_string(),
            "workspace::OpenView: no such kind"
        );
    }
}
