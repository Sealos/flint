//! The action registry: every interactive control in the app as a named GPUI action, with a
//! label, a description, its arguments and its default keys.
//!
//! Clicks, key bindings, the menu bar and anything that drives the app by name (the command
//! palette, test automation) end in the same action handlers, so one list says everything a user
//! can do. Views own the handlers: each view registers the actions of its [`Scope`] on its root
//! element, and the workspace forwards list, detail and navigation actions to the view that is
//! open, so an action works wherever focus is.
//!
//! - [`actions()`] lists every action, in menu order.
//! - [`find`] looks one up by name (`workspace::OpenView`).
//! - [`ActionSpec::build`] makes the action from JSON arguments.
//! - [`ActionSpec::is_available`] says whether it would do anything in the window right now.
//! - [`dispatch`] builds an action by name and sends it to the focused element, exactly as its
//!   key binding would.
//! - [`init`] binds the default keys; call it once at startup.

// The lookup half (names, labels, `dispatch`) is for the command palette (#29) and the test-only
// automation channel; a default build has neither yet.
#![allow(dead_code)]

use std::any::TypeId;
use std::fmt;
use std::rc::Rc;
use std::sync::LazyLock;

use gpui_kit::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

// Unit actions: no arguments.
actions!(
    flint,
    [
        /// Quit Flint.
        Quit,
        /// Close the front window.
        CloseWindow
    ]
);
actions!(
    workspace,
    [
        /// Connect (and discover) the active cluster again after a failure.
        RetryConnection,
        /// Clear the namespace filter.
        ShowAllNamespaces
    ]
);
actions!(
    resource_list,
    [
        SelectPrev,
        SelectNext,
        SelectFirst,
        SelectLast,
        /// Clear the selection, which closes the detail pane.
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
        ToggleManagedFields,
        ToggleLabels,
        ToggleAnnotations
    ]
);

/// Make a cluster (kubeconfig context) the active one, as clicking it in the sidebar does.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SelectCluster {
    pub cluster: String,
}

/// Ask to expose a cluster to AI agents. Like the sidebar button, this opens the confirmation.
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

/// Open the list of a kind on the active cluster, as clicking it in the navigation does. `kind`
/// is matched like `FLINT_VIEW`: `pods`, `deployments`, `deployments.apps`.
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

/// Which watch-mode setting [`SetWatchMode`] changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchScope {
    /// The active cluster's own setting.
    #[default]
    Cluster,
    /// The default for every cluster.
    Default,
}

/// A watch mode, as the menu offers it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchModeChoice {
    /// The cluster follows the default (only for [`WatchScope::Cluster`]).
    #[default]
    UseDefault,
    All,
    OnView,
}

/// Pick the watch mode of the active cluster, or the default for all clusters.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetWatchMode {
    pub scope: WatchScope,
    pub mode: WatchModeChoice,
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

/// Set a pane's width in points, as dragging its edge does (clamped to the pane's limits).
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

/// A tab of the detail pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabName {
    #[default]
    Overview,
    Yaml,
}

/// Switch the detail pane's tab.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = detail, no_json)]
#[serde(deny_unknown_fields)]
pub struct ShowTab {
    pub tab: TabName,
}

/// Reveal or hide one value of a Secret, by key.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = detail, no_json)]
#[serde(deny_unknown_fields)]
pub struct ToggleSecretValue {
    pub key: String,
}

/// Which part of the app handles an action, and so when it is available.
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
}

/// One argument of an action, for help text and pickers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arg {
    pub name: &'static str,
    pub description: &'static str,
    pub required: bool,
}

/// Why an action could not be built or dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionError {
    /// No action has this name.
    Unknown(String),
    /// The arguments do not fit the action.
    InvalidArgs { action: String, message: String },
    /// Nothing in the window handles the action now (for example no list is open).
    NotAvailable(String),
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
        }
    }
}

impl std::error::Error for ActionError {}

/// An action the app offers: what it is called, what it does, and how to make one.
pub struct ActionSpec {
    /// The GPUI action name, `namespace::Name`. Stable: automation and key maps refer to it.
    pub name: &'static str,
    /// A short title for menus and the command palette ("Open Resource View").
    pub label: &'static str,
    pub description: &'static str,
    pub scope: Scope,
    pub args: &'static [Arg],
    /// Default key bindings, in GPUI keystroke syntax ("cmd-q", "up").
    pub keys: &'static [&'static str],
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

    /// Whether something in `window` handles this action now, from the focused element up.
    pub fn is_available(&self, window: &Window, cx: &App) -> bool {
        self.scope == Scope::App || window.is_action_available(&*(self.probe)(), cx)
    }

    /// Whether `action` is this kind of action.
    pub fn matches(&self, action: &dyn Action) -> bool {
        action.as_any().type_id() == self.type_id
    }
}

/// Every action, in the order a menu lists them.
pub fn actions() -> &'static [ActionSpec] {
    &REGISTRY
}

/// The action named `name` (`workspace::OpenView`).
pub fn find(name: &str) -> Option<&'static ActionSpec> {
    REGISTRY.iter().find(|spec| spec.name == name)
}

/// The actions that would do something in `window` now, for a command palette.
pub fn available<'a>(
    window: &'a Window,
    cx: &'a App,
) -> impl Iterator<Item = &'static ActionSpec> + 'a {
    REGISTRY
        .iter()
        .filter(move |spec| spec.is_available(window, cx))
}

/// Builds the action `name` from `args` and dispatches it to the focused element, like its key
/// binding. Names outside the registry are built by GPUI, so actions of library components (a
/// dialog's confirm) work too. Fails, without dispatching, when nothing would handle it.
pub fn dispatch(
    name: &str,
    args: Value,
    window: &mut Window,
    cx: &mut App,
) -> Result<(), ActionError> {
    let action = match find(name) {
        Some(spec) => {
            let action = spec.build(args)?;
            if !spec.is_available(window, cx) {
                return Err(ActionError::NotAvailable(name.to_owned()));
            }
            action
        }
        None => {
            if !cx.all_action_names().contains(&name) {
                return Err(ActionError::Unknown(name.to_owned()));
            }
            let data = (!args.is_null()).then_some(args);
            let action = cx
                .build_action(name, data)
                .map_err(|e| ActionError::InvalidArgs {
                    action: name.to_owned(),
                    message: e.to_string(),
                })?;
            if !window.is_action_available(&*action, cx) {
                return Err(ActionError::NotAvailable(name.to_owned()));
            }
            action
        }
    };
    window.dispatch_action(action, cx);
    Ok(())
}

/// Binds every action's default keys. Call once at startup, before opening windows.
pub fn init(cx: &mut App) {
    let mut bindings = Vec::new();
    for spec in REGISTRY.iter() {
        let context = key_context(spec.scope).map(|c| {
            Rc::new(KeyBindingContextPredicate::parse(c).expect("static key context parses"))
        });
        for keys in spec.keys {
            let action = (spec.probe)();
            match KeyBinding::load(keys, action, context.clone(), false, None, cx) {
                Ok(binding) => bindings.push(binding),
                Err(e) => tracing::error!(action = spec.name, keys, "invalid key binding: {e}"),
            }
        }
    }
    cx.bind_keys(bindings);
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

/// Registers a child view's handlers on an ancestor's element, running them on that view. With
/// no view (none is open), it registers nothing, so the actions are not available.
pub struct Forward<E, V: 'static> {
    element: E,
    view: Option<Entity<V>>,
}

impl<E: InteractiveElement, V: 'static> Forward<E, V> {
    pub fn new(element: E, view: Option<Entity<V>>) -> Self {
        Self { element, view }
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
                    target.update(cx, |view, cx| handler(view, action, window, cx))
                });
        Self {
            element,
            view: Some(view),
        }
    }
}

/// The key context an action's keys apply in. Only the list has one; other actions' keys are
/// global.
fn key_context(scope: Scope) -> Option<&'static str> {
    match scope {
        Scope::List => Some(super::list::CONTEXT),
        _ => None,
    }
}

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

fn unit<A: Action + Default>(
    label: &'static str,
    description: &'static str,
    scope: Scope,
    keys: &'static [&'static str],
) -> ActionSpec {
    ActionSpec {
        name: A::name_for_type(),
        label,
        description,
        scope,
        args: &[],
        keys,
        type_id: TypeId::of::<A>(),
        build: build_unit::<A>,
        probe: probe::<A>,
    }
}

fn with_args<A: Action + Default + DeserializeOwned>(
    label: &'static str,
    description: &'static str,
    scope: Scope,
    args: &'static [Arg],
) -> ActionSpec {
    ActionSpec {
        name: A::name_for_type(),
        label,
        description,
        scope,
        args,
        keys: &[],
        type_id: TypeId::of::<A>(),
        build: build_with::<A>,
        probe: probe::<A>,
    }
}

const CLUSTER: Arg = Arg {
    name: "cluster",
    description: "kubeconfig context name",
    required: true,
};

static REGISTRY: LazyLock<Vec<ActionSpec>> = LazyLock::new(|| {
    vec![
        unit::<Quit>("Quit Flint", "Quit the app.", Scope::App, &["cmd-q"]),
        unit::<CloseWindow>(
            "Close Window",
            "Close the front window.",
            Scope::App,
            &["cmd-w"],
        ),
        // Clusters.
        with_args::<SelectCluster>(
            "Select Cluster",
            "Make a cluster the active one and connect to it.",
            Scope::Workspace,
            &[CLUSTER],
        ),
        with_args::<ExposeToAi>(
            "Expose Cluster to AI…",
            "Let flint mcp read this cluster, after a confirmation naming its server.",
            Scope::Workspace,
            &[CLUSTER],
        ),
        with_args::<HideFromAi>(
            "Hide Cluster from AI",
            "Stop exposing a cluster to flint mcp, or remove a stale exposure.",
            Scope::Workspace,
            &[CLUSTER],
        ),
        unit::<RetryConnection>(
            "Retry Connection",
            "Connect to the active cluster again after a failure.",
            Scope::Workspace,
            &[],
        ),
        // Navigation.
        with_args::<OpenView>(
            "Open Resource View",
            "Open the list of a kind on the active cluster.",
            Scope::Workspace,
            &[Arg {
                name: "kind",
                description: "plural or plural.group: pods, deployments.apps",
                required: true,
            }],
        ),
        with_args::<ToggleNavSection>(
            "Toggle Navigation Section",
            "Open or close a section of the resource navigation.",
            Scope::Navigation,
            &[Arg {
                name: "section",
                description: "section title, as shown: Workloads",
                required: true,
            }],
        ),
        // Header.
        with_args::<SetNamespaces>(
            "Filter by Namespaces",
            "Show only these namespaces (remembered per cluster).",
            Scope::Workspace,
            &[Arg {
                name: "namespaces",
                description: "namespace names; empty for all",
                required: true,
            }],
        ),
        with_args::<ToggleNamespace>(
            "Toggle Namespace",
            "Add a namespace to the filter, or remove it.",
            Scope::Workspace,
            &[Arg {
                name: "namespace",
                description: "namespace name",
                required: true,
            }],
        ),
        unit::<ShowAllNamespaces>(
            "Show All Namespaces",
            "Clear the namespace filter.",
            Scope::Workspace,
            &[],
        ),
        with_args::<SetWatchMode>(
            "Set Watch Mode",
            "Choose which kinds Flint watches, for this cluster or by default.",
            Scope::Workspace,
            &[
                Arg {
                    name: "scope",
                    description: "cluster or default",
                    required: true,
                },
                Arg {
                    name: "mode",
                    description: "use_default, all or on_view",
                    required: true,
                },
            ],
        ),
        with_args::<SetPaneWidth>(
            "Set Pane Width",
            "Resize the clusters, navigation or detail pane.",
            Scope::Workspace,
            &[
                Arg {
                    name: "pane",
                    description: "clusters, navigation or detail",
                    required: true,
                },
                Arg {
                    name: "width",
                    description: "width in points",
                    required: true,
                },
            ],
        ),
        // Resource list.
        with_args::<SelectRow>(
            "Select Object",
            "Select an object in the open list and show its details.",
            Scope::List,
            &[
                Arg {
                    name: "namespace",
                    description: "namespace; leave out for cluster-scoped kinds",
                    required: false,
                },
                Arg {
                    name: "name",
                    description: "object name",
                    required: true,
                },
            ],
        ),
        unit::<SelectPrev>(
            "Select Previous Row",
            "Move the selection up.",
            Scope::List,
            &["up"],
        ),
        unit::<SelectNext>(
            "Select Next Row",
            "Move the selection down.",
            Scope::List,
            &["down"],
        ),
        unit::<SelectFirst>(
            "Select First Row",
            "Select the first row.",
            Scope::List,
            &["home"],
        ),
        unit::<SelectLast>(
            "Select Last Row",
            "Select the last row.",
            Scope::List,
            &["end"],
        ),
        unit::<CloseDetail>(
            "Clear Selection",
            "Clear the selection and close the detail pane.",
            Scope::List,
            &["escape"],
        ),
        with_args::<SortBy>(
            "Sort by Column",
            "Sort by a column: ascending, descending, then unsorted.",
            Scope::List,
            &[Arg {
                name: "column",
                description: "column name, as the server names it: Restarts",
                required: true,
            }],
        ),
        unit::<ToggleWide>(
            "Toggle Wide Columns",
            "Show or hide the extra columns of kubectl get -o wide.",
            Scope::List,
            &[],
        ),
        unit::<FocusFilter>(
            "Filter List",
            "Move focus to the list's text filter.",
            Scope::List,
            &[],
        ),
        // Detail pane.
        with_args::<ShowTab>(
            "Show Detail Tab",
            "Switch the detail pane between Overview and YAML.",
            Scope::Detail,
            &[Arg {
                name: "tab",
                description: "overview or yaml",
                required: true,
            }],
        ),
        unit::<Refresh>(
            "Refresh Object",
            "Fetch the shown object again.",
            Scope::Detail,
            &[],
        ),
        unit::<Close>("Close Detail", "Close the detail pane.", Scope::Detail, &[]),
        unit::<ToggleManagedFields>(
            "Toggle managedFields",
            "Show or hide metadata.managedFields in the YAML.",
            Scope::Detail,
            &[],
        ),
        unit::<ToggleLabels>(
            "Toggle Labels",
            "Expand or collapse the labels.",
            Scope::Detail,
            &[],
        ),
        unit::<ToggleAnnotations>(
            "Toggle Annotations",
            "Expand or collapse the annotations.",
            Scope::Detail,
            &[],
        ),
        with_args::<ToggleSecretValue>(
            "Reveal Secret Value",
            "Reveal or hide one value of a Secret.",
            Scope::Detail,
            &[Arg {
                name: "key",
                description: "the data key",
                required: true,
            }],
        ),
    ]
});

#[cfg(test)]
pub mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use gpui_kit::Action;
    use serde_json::Value;

    use super::{
        ActionError, Bind, Handler, OpenView, Scope, SelectRow, SetWatchMode, WatchModeChoice,
        WatchScope, actions, find,
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
    fn names_are_unique_and_namespaced() {
        let mut seen = HashSet::new();
        for spec in actions() {
            assert!(seen.insert(spec.name), "duplicate action {}", spec.name);
            assert!(spec.name.contains("::"), "{} has no namespace", spec.name);
            assert!(!spec.label.is_empty() && !spec.description.is_empty());
        }
        assert_eq!(
            find("workspace::OpenView").unwrap().label,
            "Open Resource View"
        );
        assert!(find("workspace::Nope").is_none());
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

        let mode = find("workspace::SetWatchMode").unwrap();
        let action = mode
            .build(json!({"scope": "default", "mode": "on_view"}))
            .unwrap();
        assert_eq!(
            action.as_any().downcast_ref::<SetWatchMode>(),
            Some(&SetWatchMode {
                scope: WatchScope::Default,
                mode: WatchModeChoice::OnView
            })
        );
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
    fn unit_actions_take_no_args_and_keys_belong_to_unit_actions() {
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
        }
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
    }
}
