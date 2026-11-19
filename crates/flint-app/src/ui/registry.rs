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
use gpui_kit::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

actions!(
    flint,
    [
        /// Quit Flint.
        Quit,
        /// Close the front window.
        CloseWindow,
        /// Follow macOS light and dark mode (the `appearance` setting).
        AppearanceSystem,
        /// Always use Flint Light.
        AppearanceLight,
        /// Always use Flint Dark.
        AppearanceDark
    ]
);
actions!(
    workspace,
    [
        /// Connect (and discover) the active cluster again after a failure.
        RetryConnection,
        /// Clear the namespace filter.
        ShowAllNamespaces,
        /// Watch every kind on the active cluster.
        WatchAllKinds,
        /// Watch only the kinds open in a view, on the active cluster.
        WatchOpenViewsOnly,
        /// Let the active cluster follow the default watch mode.
        UseDefaultWatchMode,
        /// Make "all kinds" the default watch mode.
        DefaultWatchAllKinds,
        /// Make "open views only" the default watch mode.
        DefaultWatchOpenViewsOnly
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
        /// Move focus to the list's text filter.
        FocusFilter
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
        ToggleAnnotations
    ]
);

actions!(
    detail,
    [
        /// Open the shown pod's logs (its default container) in the dock.
        OpenLogs,
        /// Open the logs of the previous instance of the shown pod's default container.
        OpenPreviousLogs
    ]
);
actions!(
    detail,
    [
        /// Open a shell in the shown pod's default container, in the dock.
        OpenShell
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
        /// Show the dock's next tab.
        NextTab,
        /// Show the dock's previous tab.
        PrevTab
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
        SaveSelectedLines
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

/// Hide a cluster from AI agents, or remove a stale exposure.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct HideFromAi {
    pub cluster: String,
}

/// Open the list of a kind on the active cluster. `kind` is matched like `FLINT_VIEW`: `pods`,
/// `deployments`, `deployments.apps`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenView {
    pub kind: String,
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

/// Select the row of an object in the open list, as clicking it does.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = resource_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectRow {
    #[serde(default)]
    pub namespace: Option<String>,
    pub name: String,
}

/// Sort by a column, as clicking its header does: ascending, descending, then unsorted.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = resource_list, no_json)]
#[serde(deny_unknown_fields)]
pub struct SortBy {
    pub column: String,
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

/// Which part of the app handles an action, and so where it is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Always available (application menu).
    App,
    /// The main window.
    Workspace,
    /// The resource navigation of the active cluster.
    Navigation,
    /// The open resource list.
    List,
    /// The detail pane.
    Detail,
    /// An open dialog (the Expose to AI confirmation).
    Dialog,
    /// The bottom dock and its tabs.
    Dock,
    /// The log tab shown in the dock.
    Logs,
    /// The shell tab shown in the dock.
    Terminal,
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
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
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

/// An action the app offers: what it is called, what it does, and how to make one.
pub struct ActionSpec {
    /// The GPUI action name, `namespace::Name`. Stable: automation and key maps refer to it.
    pub name: &'static str,
    /// A Title Case name for menus and the palette; it ends in "…" when it takes arguments.
    pub label: &'static str,
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
    pub description: &'static str,
    pub category: Category,
    pub scope: Scope,
    pub visibility: Visibility,
    pub args: &'static [Arg],
    /// Default keys, in GPUI keystroke syntax ("cmd-q", "down"), and the key context they
    /// apply in (`None`: everywhere).
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
    pub fn takes_args(&self) -> bool {
        !self.args.is_empty()
    }

    /// Builds the action from its arguments: a JSON object, or `null` for none.
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
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
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
    pub fn is_available_in(&self, focus: &FocusHandle, window: &Window, cx: &App) -> bool {
        let handled = match self.scope {
            Scope::App => true,
            _ => window.is_action_available_in(&*(self.probe)(), focus),
        };
        handled && self.is_enabled(cx)
    }

    /// The live key binding to show next to the action ("⌘F"), if any: the one added last to
    /// the keymap (the highest precedence), so later or user bindings win over the defaults.
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "used by the command palette (#29) and automation")
    )]
    pub fn shortcut(&self, cx: &App) -> Option<String> {
        let action = (self.probe)();
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let binding = keymap.bindings_for_action(&*action).next_back()?;
        let os = cx.platform_info().os;
        Some(
            binding
                .keystrokes()
                .iter()
                .map(|k| k.display(os).to_string())
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by the command palette (#29) and automation")
)]
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
    let spec = spec_of(action);
    let handled = match spec {
        Some(spec) if spec.scope == Scope::App => true,
        _ => window.is_action_available_in(action, focus),
    };
    if !handled {
        return Err(ActionError::NotAvailable(action.name().to_owned()));
    }
    if let Some(spec) = spec {
        spec.validate(action, cx)?;
    }
    focus.dispatch_action(action, window, cx);
    Ok(())
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
/// exactly its action. A refusal is logged: the control was drawn from older state.
pub fn on_click<E: ?Sized, A: Action>(action: A) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        if let Err(e) = perform(&action, window, cx) {
            tracing::warn!("{e}");
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
    for keys in TERMINAL_PASSTHROUGH {
        match KeyBinding::load(keys, Box::new(NoAction), terminal.clone(), false, None, cx) {
            Ok(binding) => bindings.push(binding),
            Err(e) => tracing::error!(keys, "invalid terminal key: {e}"),
        }
    }
    cx.bind_keys(bindings);
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

/// The workspace's key context.
pub const WORKSPACE: &str = "Workspace";
/// Plain keys (arrows, Esc) work in the window, but not while typing in a field.
const PLAIN: &str = "Workspace && !Input";
/// The same, for the list: deeper than the workspace, so it wins there.
const LIST_ALONE: &str = "ResourceList && !Input";
/// The bottom dock's key context: Cmd-W closes a tab there instead of the window.
pub const DOCK: &str = "Dock";
/// A log tab's key context.
pub const LOG_VIEW: &str = "LogView";
/// A shell tab's key context.
pub const TERMINAL: &str = "Terminal";
/// Plain keys in a log tab, but not while typing in its search field.
const LOG_PLAIN: &str = "LogView && !Input";

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
const NAMESPACE: &[Arg] = &[arg("namespace", "namespace name", ArgKind::Namespace)];
const KIND: &[Arg] = &[arg(
    "kind",
    "plural or plural.group: pods, deployments.apps",
    ArgKind::Kind,
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
const ROW: &[Arg] = &[
    Arg {
        name: "namespace",
        description: "namespace; leave out for cluster-scoped kinds",
        required: false,
        kind: ArgKind::Namespace,
    },
    arg("name", "object name", ArgKind::Text),
];
const COLUMN: &[Arg] = &[arg(
    "column",
    "column name, as drawn: Restarts",
    ArgKind::Column,
)];
const KEY: &[Arg] = &[arg("key", "the data key", ArgKind::Text)];
const CONTAINER: &[Arg] = &[
    arg("container", "container name", ArgKind::Text),
    Arg {
        name: "previous",
        description: "the previous (terminated) instance",
        required: false,
        kind: ArgKind::Bool,
    },
];
const SHELL_CONTAINER: &[Arg] = &[arg("container", "container name", ArgKind::Text)];
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
        Spec::unit::<Quit>("Quit Flint", "Quit the app.")
            .of(C::Flint, S::App)
            .keys(&["cmd-q"], None),
        Spec::unit::<CloseWindow>("Close Window", "Close the front window.")
            .of(C::Flint, S::App)
            .keys(&["cmd-w"], None),
        Spec::unit::<AppearanceSystem>(
            "Use System Appearance",
            "Follow macOS light and dark mode.",
        )
        .of(C::Flint, S::App),
        Spec::unit::<AppearanceLight>("Use Light Appearance", "Always use Flint Light.")
            .of(C::Flint, S::App),
        Spec::unit::<AppearanceDark>("Use Dark Appearance", "Always use Flint Dark.")
            .of(C::Flint, S::App),
        Spec::confirm(),
        Spec::unit::<Cancel>("Cancel", "Dismiss the open dialog.").of(C::Flint, S::Dialog),
        // Clusters.
        Spec::with_args::<SelectCluster>(
            "Select Cluster…",
            "Make a cluster the active one and connect to it.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<ExposeToAi>(
            "Expose Cluster to AI…",
            "Let Flint's MCP server read a cluster, after a confirmation naming its server.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::with_args::<HideFromAi>(
            "Hide Cluster from AI…",
            "Stop exposing a cluster to Flint's MCP server, or remove a stale exposure.",
            CLUSTER,
        )
        .of(C::Cluster, S::Workspace),
        Spec::unit::<RetryConnection>(
            "Retry Connection",
            "Connect to the active cluster again after a failure.",
        )
        .of(C::Cluster, S::Workspace)
        .keys(&["cmd-r"], Some(WORKSPACE)),
        Spec::unit::<WatchAllKinds>("Watch All Kinds", "Watch every kind on the active cluster.")
            .of(C::Cluster, S::Workspace),
        Spec::unit::<WatchOpenViewsOnly>(
            "Watch Open Views Only",
            "Watch only the kinds open in a view, on the active cluster.",
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
            "Watch Open Views Only by Default",
            "Make watching only open views the default for all clusters.",
        )
        .of(C::Cluster, S::Workspace),
        // Navigation and panes.
        Spec::with_args::<OpenView>(
            "Open Kind…",
            "Open the list of a kind on the active cluster.",
            KIND,
        )
        .of(C::View, S::Workspace),
        Spec::with_args::<ToggleNavSection>(
            "Toggle Navigation Section…",
            "Open or close a section of the resource navigation.",
            SECTION,
        )
        .of(C::View, S::Navigation)
        .hidden(),
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
            .keys(&["cmd-f"], Some(WORKSPACE)),
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
        Spec::unit::<CloseDetail>(
            "Clear Selection",
            "Clear the selection, which closes the detail pane.",
        )
        .of(C::List, S::List)
        .keys(&["escape"], Some(LIST_ALONE))
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
        .of(C::List, S::List)
        .hidden(),
        // Detail pane.
        Spec::unit::<Close>("Close Detail", "Close the detail pane.")
            .of(C::Detail, S::Detail)
            .keys(&["escape"], Some(PLAIN)),
        Spec::unit::<ShowOverview>("Show Overview", "Show the object's overview.")
            .of(C::Detail, S::Detail)
            .keys(&["cmd-1"], Some(WORKSPACE)),
        Spec::unit::<ShowYaml>("Show YAML", "Show the object as YAML.")
            .of(C::Detail, S::Detail)
            .keys(&["cmd-2"], Some(WORKSPACE)),
        Spec::unit::<Refresh>("Refresh Object", "Fetch the shown object again.")
            .of(C::Detail, S::Detail)
            .keys(&["cmd-r"], Some(WORKSPACE)),
        Spec::unit::<ToggleManagedFields>(
            "Toggle Managed Fields",
            "Show or hide metadata.managedFields in the YAML.",
        )
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
        Spec::unit::<OpenLogs>("Open Logs", "Open the shown pod's logs in the dock.")
            .of(C::Logs, S::Detail)
            .keys(&["cmd-l"], Some(WORKSPACE)),
        Spec::unit::<OpenPreviousLogs>(
            "Open Previous Logs",
            "Open the logs of the previous instance of the shown pod's container.",
        )
        .of(C::Logs, S::Detail)
        .keys(&["cmd-shift-l"], Some(WORKSPACE)),
        Spec::with_args::<OpenContainerLogs>(
            "Open Container Logs…",
            "Open the logs of one container of the shown pod, or of its previous instance.",
            CONTAINER,
        )
        .of(C::Logs, S::Detail),
        Spec::unit::<OpenShell>(
            "Open Shell",
            "Open a shell (bash, else sh) in the shown pod's default container, in the dock.",
        )
        .of(C::Terminal, S::Detail)
        .keys(&["secondary-shift-e"], Some(WORKSPACE)),
        Spec::with_args::<OpenContainerShell>(
            "Open Container Shell…",
            "Open a shell (bash, else sh) in one container of the shown pod, in the dock.",
            SHELL_CONTAINER,
        )
        .of(C::Terminal, S::Detail),
        // The dock.
        Spec::unit::<ToggleDock>("Toggle Dock", "Show or hide the bottom dock.")
            .of(C::View, S::Dock)
            .keys(&["cmd-j"], Some(WORKSPACE)),
        Spec::unit::<CloseActiveTab>(
            "Close Dock Tab",
            "Close the dock's active tab, ending its session.",
        )
        .of(C::View, S::Dock)
        .keys(&["cmd-w"], Some(DOCK)),
        Spec::unit::<NextTab>("Next Dock Tab", "Show the dock's next tab.")
            .of(C::View, S::Dock)
            .keys(&["ctrl-tab"], Some(DOCK)),
        Spec::unit::<PrevTab>("Previous Dock Tab", "Show the dock's previous tab.")
            .of(C::View, S::Dock)
            .keys(&["ctrl-shift-tab"], Some(DOCK)),
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
        .keys(&["end", "cmd-down"], Some(LOG_PLAIN)),
        Spec::unit::<JumpToStart>("Jump to Oldest Line", "Scroll to the oldest line shown.")
            .of(C::Logs, S::Logs)
            .keys(&["home", "cmd-up"], Some(LOG_PLAIN)),
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
        Spec::unit::<FocusSearch>("Search Logs", "Move focus to the log search field.")
            .of(C::Logs, S::Logs)
            .keys(&["cmd-f"], Some(LOG_VIEW)),
        Spec::unit::<NextMatch>("Next Match", "Go to the next line matching the search.")
            .of(C::Logs, S::Logs)
            .keys(&["cmd-g"], Some(LOG_VIEW)),
        Spec::unit::<PrevMatch>(
            "Previous Match",
            "Go to the previous line matching the search.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["cmd-shift-g"], Some(LOG_VIEW)),
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
            .keys(&["cmd-a"], Some(LOG_PLAIN)),
        Spec::unit::<ClearSelection>(
            "Clear Line Selection",
            "Clear the line selection and resume following.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["escape"], Some(LOG_PLAIN)),
        Spec::unit::<CopyLines>(
            "Copy Selected Lines",
            "Copy the selected lines as shown, without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["cmd-c"], Some(LOG_PLAIN)),
        Spec::unit::<SaveLogs>(
            "Save Logs",
            "Save every line shown to a file, as shown and without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["cmd-s"], Some(LOG_VIEW)),
        Spec::unit::<SaveMatchingLines>(
            "Save Matching Lines",
            "Save the lines matching the search to a file, as shown and without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["cmd-shift-s"], Some(LOG_VIEW)),
        Spec::unit::<SaveSelectedLines>(
            "Save Selected Lines",
            "Save the selected lines to a file, as shown and without redaction.",
        )
        .of(C::Logs, S::Logs)
        .keys(&["cmd-alt-s"], Some(LOG_VIEW)),
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
        ActionError, Bind, Category, Handler, OpenView, Scope, SelectRow, Visibility, actions, find,
    };

    /// Collects the names of the actions a view binds.
    pub struct Names(Vec<&'static str>);

    impl<V: 'static> Bind<V> for Names {
        fn bind<A: Action>(mut self, _: Handler<V, A>) -> Self {
            self.0.push(A::name_for_type());
            self
        }
    }

    /// Asserts that `bind_actions` binds exactly the registry's actions of `scope`.
    pub fn assert_binds_scope(scope: Scope, bind_actions: fn(Names) -> Names) {
        let mut bound = bind_actions(Names(Vec::new())).0;
        let mut wanted: Vec<&str> = actions()
            .iter()
            .filter(|s| s.scope == scope)
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
            "resource_list::SortBy",
            "workspace::ToggleNavSection",
            "workspace::SetNamespaces",
            "detail::ToggleSecretValue",
            "resource_list::CloseDetail",
        ] {
            assert!(hidden.contains(&name), "{name} is in the palette");
        }
        assert!(!hidden.contains(&"detail::Close"));
    }

    #[test]
    fn builds_actions_from_json_arguments() {
        let open = find("workspace::OpenView").unwrap();
        let action = open.build(json!({"kind": "pods"})).unwrap();
        assert!(open.matches(&*action));
        assert_eq!(
            action.as_any().downcast_ref::<OpenView>(),
            Some(&OpenView {
                kind: "pods".into()
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
            // Plain keys never fire while typing in a field.
            if spec.keys.iter().any(|k| !k.contains('-')) {
                assert!(
                    spec.key_context.is_some_and(|c| c.contains("!Input")),
                    "{} binds a plain key outside the text-field guard",
                    spec.name
                );
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
