//! The File, Cluster and Resource menus (#781): their items, and when each is enabled.

use flint_core::Appearance;
use gpui_kit::{Action, Menu, MenuItem};

use super::all_menus;
use super::app_menus::{MenuInputs, cluster_menu, file_menu, resource_menu};
use super::registry::{
    self, CloseActiveTab, CloseWindow, DeleteAcrossSet, DeleteObject, DrainNode,
    OpenClusterOverview, OpenLogs, OpenShell, RestartObject, RetryConnection, SaveLogs,
    SaveMatchingLines, SaveSelectedLines, ScaleObject, ToggleKeepWatching, WatchAllKinds,
};

fn inputs(cluster: Option<&str>, clusters: &[&str]) -> MenuInputs {
    MenuInputs {
        appearance: Appearance::System,
        cluster: cluster.map(str::to_owned),
        clusters: clusters.iter().map(|c| (*c).to_owned()).collect(),
    }
}

/// A menu's items as (name, enabled); a submenu's items follow it as `Submenu > item`.
fn rows(menu: &Menu) -> Vec<(String, bool)> {
    let mut rows = Vec::new();
    for item in &menu.items {
        match item {
            MenuItem::Action { name, disabled, .. } => rows.push((name.to_string(), !disabled)),
            MenuItem::Submenu(sub) => {
                rows.push((sub.name.to_string(), true));
                for (name, on) in rows_of(sub) {
                    rows.push((format!("{} > {name}", sub.name), on));
                }
            }
            _ => {}
        }
    }
    rows
}

fn rows_of(menu: &Menu) -> Vec<(String, bool)> {
    rows(menu)
}

fn names(menu: &Menu) -> Vec<String> {
    rows(menu).into_iter().map(|(name, _)| name).collect()
}

/// Enables only the actions whose registry label is in `allowed`.
fn enabled_if(allowed: &'static [&'static str]) -> impl FnMut(&dyn Action) -> bool {
    move |action| registry::spec_of(action).is_some_and(|spec| allowed.contains(&spec.label))
}

fn state(rows: &[(String, bool)], name: &str) -> bool {
    rows.iter().find(|(n, _)| n == name).expect(name).1
}

#[test]
fn file_menu_holds_save_and_close_items() {
    let menu = file_menu(&mut |_| true);
    assert_eq!(
        names(&menu),
        [
            registry::label::<SaveLogs>(),
            registry::label::<SaveMatchingLines>(),
            registry::label::<SaveSelectedLines>(),
            registry::label::<CloseActiveTab>(),
            registry::label::<CloseWindow>(),
        ]
    );
}

#[test]
fn file_menu_follows_the_enabled_state() {
    let mut only_close = enabled_if(&["Close Window"]);
    let rows = rows(&file_menu(&mut only_close));
    assert!(state(&rows, registry::label::<CloseWindow>()));
    assert!(!state(&rows, registry::label::<SaveLogs>()));
    assert!(!state(&rows, registry::label::<CloseActiveTab>()));
}

#[test]
fn cluster_menu_holds_the_cluster_items() {
    let menu = cluster_menu(&inputs(Some("prod"), &["dev", "prod"]), &mut |_| true);
    let names = names(&menu);
    for expected in [
        registry::label::<OpenClusterOverview>().to_owned(),
        registry::label::<RetryConnection>().to_owned(),
        format!("Watch Mode > {}", registry::label::<WatchAllKinds>()),
        "Colour Tag > Blue".to_owned(),
        "Colour Tag > Red".to_owned(),
        "Switch To > dev".to_owned(),
        "Switch To > prod".to_owned(),
    ] {
        assert!(names.contains(&expected), "{expected}: {names:?}");
    }
}

#[test]
fn cluster_items_act_on_the_active_cluster() {
    let menu = cluster_menu(&inputs(Some("prod"), &["dev", "prod"]), &mut |_| true);
    let keep = menu.items.iter().find_map(|item| match item {
        MenuItem::Action { action, .. } => action.as_any().downcast_ref::<ToggleKeepWatching>(),
        _ => None,
    });
    assert_eq!(keep.unwrap().cluster, "prod");
    let switch = menu
        .items
        .iter()
        .find_map(|item| match item {
            MenuItem::Submenu(m) if m.name == "Switch To" => Some(m),
            _ => None,
        })
        .unwrap();
    let checked: Vec<_> = switch
        .items
        .iter()
        .filter_map(|item| match item {
            MenuItem::Action { name, checked, .. } if *checked => Some(name.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(checked, ["prod"]);
}

#[test]
fn cluster_items_have_an_empty_name_without_a_cluster() {
    let menu = cluster_menu(&inputs(None, &[]), &mut |_| true);
    let keep = menu.items.iter().find_map(|item| match item {
        MenuItem::Action { action, .. } => action.as_any().downcast_ref::<ToggleKeepWatching>(),
        _ => None,
    });
    assert_eq!(keep.unwrap().cluster, "");
}

#[test]
fn resource_menu_holds_the_resource_items() {
    let names = names(&resource_menu(&mut |_| true));
    for expected in [
        registry::label::<OpenLogs>(),
        registry::label::<OpenShell>(),
        registry::label::<ScaleObject>(),
        registry::label::<RestartObject>(),
        registry::label::<DeleteObject>(),
        registry::label::<DrainNode>(),
    ] {
        assert!(names.iter().any(|n| n == expected), "{expected}: {names:?}");
    }
    let set = format!(
        "Across Cluster Set > {}",
        registry::label::<DeleteAcrossSet>()
    );
    assert!(names.contains(&set), "{set}");
}

#[test]
fn resource_items_are_disabled_when_the_action_is_not_enabled() {
    let mut logs_only = enabled_if(&["Open Logs"]);
    let rows = rows(&resource_menu(&mut logs_only));
    assert!(state(&rows, registry::label::<OpenLogs>()));
    assert!(!state(&rows, registry::label::<DeleteObject>()));
    assert!(!state(&rows, registry::label::<OpenShell>()));
}

#[test]
fn every_item_is_a_registered_action() {
    fn check(menu: &Menu) {
        for item in &menu.items {
            match item {
                MenuItem::Action { name, action, .. } => {
                    assert!(registry::spec_of(&**action).is_some(), "{name}")
                }
                MenuItem::Submenu(sub) => check(sub),
                _ => {}
            }
        }
    }
    let all = all_menus(&inputs(Some("a"), &["a"]), &mut |_| true);
    let names: Vec<_> = all.iter().map(|m| m.name.to_string()).collect();
    assert_eq!(
        names,
        [
            "Flint", "File", "Edit", "View", "Cluster", "Resource", "Window", "Help"
        ]
    );
    all.iter().for_each(check);
}
