//! The File, Cluster and Resource menus (#781), and keeping the whole menu bar in step with the
//! window.
//!
//! Every item is a registered action, the one the command palette and the row menus run, and it
//! goes through the same guards: an item is disabled when the palette would not list the action
//! (no cluster active, nothing selected, the kind has no such action). The menus are built again
//! when what they show changes: the active cluster, the clusters listed, the appearance, or an
//! action's enabled state. They add no write behaviour; Scale, Restart, Delete and the node
//! actions still open their own review and confirmation.

// module-tests: app_menus_tests.rs

use flint_core::Appearance;
use flint_core::settings::ClusterTag;
use gpui_kit::*;

use super::registry::{
    self, ActivateSelectedCluster, ApplyAcrossSet, CancelConnection, ClearClusterTag,
    CloseActiveTab, CloseClusterSet, CloseWindow, CordonNode, DeleteAcrossSet, DeleteObject,
    DisconnectCluster, DrainNode, EditYaml, ForwardPort, LockCluster, OpenClusterOverview,
    OpenClusterSetDashboard, OpenLogs, OpenNamespaceFilter, OpenPickedClusters, OpenPreviousLogs,
    OpenShell, Refresh, RenameCluster, RestartAcrossSet, RestartObject, RetryConnection, SaveLogs,
    SaveMatchingLines, SaveSelectedLines, ScaleAcrossSet, ScaleObject, SelectCluster,
    SelectNextCluster, SelectPreviousCluster, SetClusterTag, ShowAllNamespaces, ShowDrainReport,
    ShowHelmReleases, ShowOverview, ShowPortForwards, ShowYaml, ToggleClusterPicking,
    ToggleKeepWatching, UncordonNode, UnlockCluster, UseDefaultWatchMode, WatchAllKinds,
    WatchOpenViewsOnly,
};

/// What the menus are built from, besides the actions' enabled state.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MenuInputs {
    pub appearance: Appearance,
    /// The active cluster's context name, which the Cluster menu's items act on.
    pub cluster: Option<String>,
    /// Every cluster listed, as its context name and the name shown for it, for the Switch To
    /// submenu.
    pub clusters: Vec<(String, String)>,
}

/// What the menu bar shows now: its inputs and the enabled state of each item, in menu order.
struct Shown {
    inputs: MenuInputs,
    enabled: Vec<bool>,
}

impl Global for Shown {}

actions!(
    menu,
    [
        /// What a menu item that cannot do anything now holds in place of its action. Nothing
        /// handles it, so the native menu, which asks only whether an action has a handler,
        /// shows the item disabled.
        Unavailable
    ]
);

/// Whether an action would do something now.
pub(super) type Enabled<'a> = &'a mut dyn FnMut(&dyn Action) -> bool;

/// A menu item for `action`, named by the registry and disabled unless `enabled` says so.
fn item<A: Action>(action: A, enabled: Enabled) -> MenuItem {
    named(registry::label::<A>(), action, enabled, false)
}

/// Like [`item`], with the name the menu shows and a check mark if `checked`.
fn named(
    name: impl Into<SharedString>,
    action: impl Action,
    enabled: Enabled,
    checked: bool,
) -> MenuItem {
    let ok = enabled(&action);
    // The native menu asks only whether the action has a handler, not the registry's guards, and
    // enables the item then whatever `disabled` says.
    let action: Box<dyn Action> = if ok {
        Box::new(action)
    } else {
        Box::new(Unavailable)
    };
    MenuItem::Action {
        name: name.into(),
        action,
        os_action: None,
        checked,
        disabled: !ok,
    }
}

/// File: saving what a log tab shows, and closing the dock tab or the window.
pub(super) fn file_menu(enabled: Enabled) -> Menu {
    Menu::new("File").items([
        item(SaveLogs, enabled),
        item(SaveMatchingLines, enabled),
        item(SaveSelectedLines, enabled),
        MenuItem::separator(),
        item(CloseActiveTab, enabled),
        item(CloseWindow, enabled),
    ])
}

/// Cluster: the active cluster's connection, watching, protection, name and tag; moving between
/// clusters, sets of them, and its namespaces.
pub(super) fn cluster_menu(inputs: &MenuInputs, enabled: Enabled) -> Menu {
    let cluster = inputs.cluster.clone().unwrap_or_default();
    let mut tags: Vec<MenuItem> = Vec::new();
    for tag in ClusterTag::ALL {
        let action = SetClusterTag {
            cluster: cluster.clone(),
            tag,
            endpoint: None,
        };
        tags.push(named(tag.label(), action, enabled, false));
    }
    tags.push(MenuItem::separator());
    tags.push(item(
        ClearClusterTag {
            cluster: cluster.clone(),
            endpoint: None,
        },
        enabled,
    ));
    let mut switch: Vec<MenuItem> = Vec::new();
    for (context, shown) in &inputs.clusters {
        let action = SelectCluster {
            cluster: context.clone(),
        };
        let active = inputs.cluster.as_deref() == Some(context.as_str());
        switch.push(named(shown.clone(), action, enabled, active));
    }
    Menu::new("Cluster").items([
        item(OpenClusterOverview, enabled),
        item(
            ShowHelmReleases {
                cluster: cluster.clone(),
            },
            enabled,
        ),
        item(ShowPortForwards, enabled),
        MenuItem::separator(),
        item(RetryConnection, enabled),
        item(CancelConnection, enabled),
        item(
            DisconnectCluster {
                cluster: cluster.clone(),
            },
            enabled,
        ),
        MenuItem::separator(),
        item(
            ToggleKeepWatching {
                cluster: cluster.clone(),
            },
            enabled,
        ),
        MenuItem::submenu(Menu::new("Watch Mode").items([
            item(WatchAllKinds, enabled),
            item(WatchOpenViewsOnly, enabled),
            item(UseDefaultWatchMode, enabled),
        ])),
        MenuItem::separator(),
        item(
            UnlockCluster {
                cluster: cluster.clone(),
            },
            enabled,
        ),
        item(
            LockCluster {
                cluster: cluster.clone(),
            },
            enabled,
        ),
        item(
            RenameCluster {
                cluster: cluster.clone(),
            },
            enabled,
        ),
        MenuItem::submenu(Menu::new("Colour Tag").items(tags)),
        MenuItem::separator(),
        MenuItem::submenu(Menu::new("Switch To").items(switch)),
        item(SelectPreviousCluster, enabled),
        item(SelectNextCluster, enabled),
        item(ActivateSelectedCluster, enabled),
        MenuItem::separator(),
        item(ToggleClusterPicking, enabled),
        item(OpenPickedClusters, enabled),
        item(OpenClusterSetDashboard, enabled),
        item(CloseClusterSet, enabled),
        MenuItem::separator(),
        item(OpenNamespaceFilter, enabled),
        item(ShowAllNamespaces, enabled),
    ])
}

/// Resource: what the selected object offers, as its row menu does: logs, shell, port forward,
/// the views of it, and the actions that change it (each opens its own review and
/// confirmation), then the same across the running set.
pub(super) fn resource_menu(enabled: Enabled) -> Menu {
    Menu::new("Resource").items([
        item(OpenLogs, enabled),
        item(OpenPreviousLogs, enabled),
        item(OpenShell, enabled),
        item(ForwardPort, enabled),
        MenuItem::separator(),
        item(Refresh, enabled),
        item(ShowOverview, enabled),
        item(ShowYaml, enabled),
        item(EditYaml, enabled),
        MenuItem::separator(),
        item(ScaleObject, enabled),
        item(RestartObject, enabled),
        item(DeleteObject, enabled),
        MenuItem::separator(),
        item(CordonNode, enabled),
        item(UncordonNode, enabled),
        item(DrainNode, enabled),
        item(ShowDrainReport, enabled),
        MenuItem::separator(),
        MenuItem::submenu(Menu::new("Across Cluster Set").items([
            item(ScaleAcrossSet, enabled),
            item(RestartAcrossSet, enabled),
            item(ApplyAcrossSet, enabled),
            item(DeleteAcrossSet, enabled),
        ])),
    ])
}

/// Changes what the menus are built from, and builds them again if that changed the bar.
pub(super) fn update(cx: &mut App, change: impl FnOnce(&mut MenuInputs)) {
    let mut inputs = cx
        .try_global::<Shown>()
        .map(|shown| shown.inputs.clone())
        .unwrap_or(MenuInputs {
            appearance: Appearance::default(),
            cluster: None,
            clusters: Vec::new(),
        });
    change(&mut inputs);
    install(inputs, cx);
}

/// The enabled state of every item built by `build`, in menu order, with the menus.
pub(super) fn build_with(inputs: &MenuInputs, cx: &App) -> (Vec<Menu>, Vec<bool>) {
    let mut states = Vec::new();
    let menus = {
        let mut enabled = |action: &dyn Action| {
            let ok = registry::spec_of(action).is_none_or(|spec| spec.validate(action, cx).is_ok());
            states.push(ok);
            ok
        };
        super::all_menus(inputs, &mut enabled)
    };
    (menus, states)
}

/// Whether the menu bar must be built again: nothing was shown yet, or its inputs or an item's
/// enabled state differ from what the bar shows. The inputs and the states determine the whole
/// model, so comparing them is comparing the menus.
pub(super) fn needs_rebuild(
    shown: Option<(&MenuInputs, &[bool])>,
    inputs: &MenuInputs,
    enabled: &[bool],
) -> bool {
    shown.is_none_or(|(old_inputs, old_enabled)| old_inputs != inputs || old_enabled != enabled)
}

/// Sets the menu bar when it differs from the last one set. The macOS backend keeps a copy of
/// every action it is given, so an unchanged bar is never set again (#831).
fn install(inputs: MenuInputs, cx: &mut App) {
    let (menus, enabled) = build_with(&inputs, cx);
    let shown = cx
        .try_global::<Shown>()
        .map(|shown| (&shown.inputs, shown.enabled.as_slice()));
    if !needs_rebuild(shown, &inputs, &enabled) {
        return;
    }
    cx.set_menus(menus);
    cx.set_global(Shown { inputs, enabled });
}
