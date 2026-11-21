//! The context menu of a pod or workload row (UP-47): Logs, Previous Logs and Shell.
//!
//! On a pod with several containers each entry is a container picker, the default container
//! marked; the keys (⌘L, ⇧⌘L, ⇧⌘E) open the default container at once (see "From rows" in
//! `docs/ARCHITECTURE.md`).
//!
//! Right-clicking a row selects it, as a click does, so the detail pane shows it; the menu's
//! entries are then the detail pane's registered actions ([`OpenLogs`], [`OpenPreviousLogs`],
//! [`OpenShell`], [`OpenContainerShell`](super::registry::OpenContainerShell)), performed through [`registry::on_click`], so the menu,
//! the keys and the palette take one path. What each entry can do comes from [`row_actions`],
//! which uses the same checks as the actions' guards; an entry that cannot run is disabled,
//! with the reason under it. An entry chosen before the row's object has loaded waits for it
//! (see `detail_actions`).
//!
//! The menu is built a frame after the right-click, usually before the object (or a
//! workload's pods, which opening the menu lists) has arrived. While it is open it is built
//! again whenever what it shows changes.
//!
//! The list draws the menu; the workspace hands it the builder ([`row_menu`]) for the kinds that
//! have these actions, so the list stays generic.

// module-tests: row_actions_tests.rs

use std::cell::RefCell;
use std::rc::Rc;

use flint_core::KindId;
use flint_core::redact::first_line;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;

use super::detail::{Opening, ShellPod, Subject, log_item, menu_name, reason, refused, shell_item};
use super::object::PodContainer;
use super::registry::{
    self, OpenContainerLogs, OpenLogs, OpenPreviousLogs, OpenShell, UnlockCluster,
};
#[cfg(any(test, feature = "automation"))]
use super::view_state::{MenuEntryState, SubmenuEntryState};
use super::workspace::Workspace;

/// Builds a row's context menu (gpui-component's `context_menu` builder).
pub type RowMenu = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// The longest reason an action gives that a menu line shows; the rest is cut.
const MAX_REASON: usize = 80;

/// The most characters a line of the menu shows: what fits inside its padding at the menu's text
/// size, for names and reasons. Its items neither wrap nor shrink, so a longer line would be
/// drawn past its edge.
const MENU_LINE: usize = 64;

/// Whether rows of `kind` get the menu: pods and workloads.
pub fn has_row_actions(kind: &KindId) -> bool {
    Subject::of(kind) != Subject::Other
}

/// Whether an entry can run now: `Err` says why not.
pub type Availability = Result<(), String>;

/// What Logs, Previous Logs and Shell can do for the selected row now, and why not: what the
/// menu shows, and what the window's state reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowActions {
    /// Why nothing can open: the cluster is not connected, with core's summary of why
    /// (`ClusterError::summary`, already one short line).
    pub blocked: Option<String>,
    pub subject: Subject,
    pub logs: Availability,
    /// For pods only.
    pub previous: Option<Availability>,
    pub shell: Availability,
    /// The pod a shell opens in, its app containers, and the default one, as far as known.
    pub shell_pod: Option<ShellPod>,
    /// The Shell entry is a container picker: the pod has several containers and one of them
    /// takes a shell now, even if the default one does not.
    pub shell_picker: bool,
    /// A pod's containers (init ones too), once fetched: what Logs and Previous Logs pick
    /// from.
    pub log_containers: Vec<PodContainer>,
    /// The row's cluster, when it is protected (ADR-0015): Shell is disabled, saying so, and
    /// an Unlock entry follows it, so the unlock is offered in place.
    pub unlock: Option<String>,
}

/// What the selected row's actions can do now, if a pod or workload row is selected. The
/// actions' guards (`workspace_actions`) use the same checks.
pub fn row_actions(ws: &Workspace, cx: &App) -> Option<RowActions> {
    let detail = ws.detail_view()?;
    let detail = detail.read(cx);
    let subject = detail.subject();
    if subject == Subject::Other {
        return None;
    }
    let blocked = ws.sessions_blocked(&detail.object().cluster, cx);
    // An action's own reason can quote a server's error (a pod list refused): one short line.
    let gate = |checked: Result<(), registry::ActionError>| match &blocked {
        Some(why) => Err(why.clone()),
        None => checked.map_err(|e| first_line(&reason(&e), MAX_REASON)),
    };
    Some(RowActions {
        logs: gate(detail.log_plan::<OpenLogs>(None, false).map(drop)),
        previous: (subject == Subject::Pod)
            .then(|| gate(detail.log_plan::<OpenPreviousLogs>(None, true).map(drop))),
        shell: gate(detail.shell_check::<OpenShell>(None)),
        shell_pod: detail.shell_pod(),
        shell_picker: detail.shell_picker().is_some(),
        log_containers: match subject {
            Subject::Pod => detail.log_containers().to_vec(),
            _ => Vec::new(),
        },
        unlock: (blocked.is_none() && ws.is_protected(&detail.object().cluster, cx))
            .then(|| detail.object().cluster.clone()),
        blocked,
        subject,
    })
}

/// The menu for the rows of the workspace's list, acting on the row selected.
pub fn row_menu(workspace: WeakEntity<Workspace>) -> RowMenu {
    Rc::new(move |menu, window, cx| {
        let Some(ws) = workspace.upgrade() else {
            return menu;
        };
        let Some(detail) = ws.read(cx).detail_view() else {
            return menu;
        };
        // The pod a workload's shell opens in, for the Shell entry.
        detail.update(cx, |detail, cx| detail.ensure_pods_listed(cx));
        let shown = Rc::new(RefCell::new(row_actions(ws.read(cx), cx)));
        // Built again whenever what it shows changes while it is open (the object or the pods
        // arrived, the cluster disconnected, another row got selected). gpui-component keeps a
        // dismissed menu until the row's next right-click, so the observers are dropped when
        // the menu is dismissed, not when it goes.
        let rebuild = {
            let workspace = workspace.clone();
            let shown = shown.clone();
            move |menu: &mut PopupMenu, window: &mut Window, cx: &mut Context<PopupMenu>| {
                let Some(ws) = workspace.upgrade() else {
                    return;
                };
                let now = row_actions(ws.read(cx), cx);
                if *shown.borrow() == now {
                    return;
                }
                *shown.borrow_mut() = now.clone();
                #[cfg(test)]
                tests::REBUILDS.with(|n| n.set(n.get() + 1));
                let workspace = workspace.clone();
                menu.rebuild(window, cx, move |menu, window, cx| {
                    fill(menu, now.clone(), &workspace, window, cx)
                });
            }
        };
        let on_detail = rebuild.clone();
        let observers = vec![
            cx.observe_in(&detail, window, move |menu, _, window, cx| {
                on_detail(menu, window, cx)
            }),
            cx.observe_in(&ws, window, move |menu, _, window, cx| {
                rebuild(menu, window, cx)
            }),
        ];
        let observers = RefCell::new(observers);
        // Holds the menu weakly and ends with it.
        cx.subscribe_self(move |_, _: &DismissEvent, _| observers.borrow_mut().clear())
            .detach();
        let now = shown.borrow().clone();
        fill(menu, now, &workspace, window, cx)
    })
}

fn fill(
    menu: PopupMenu,
    actions: Option<RowActions>,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(a) = actions else {
        return menu;
    };
    let mut menu = menu;
    if let Some(why) = &a.blocked {
        menu = menu.label(menu_line(why)).separator();
    }
    // The cluster's reason is said once, above.
    let says_why = a.blocked.is_none();
    let entry =
        |menu, label, action, can, says_why| entry(menu, label, action, can, says_why, workspace);
    let workspace = workspace.clone();
    // A pod with several containers: Logs and Previous Logs pick one, as Shell does.
    let several = a.log_containers.len() > 1;
    menu = if several && a.logs.is_ok() {
        let containers = a.log_containers.clone();
        log_picker(menu, "Logs", containers, false, &workspace, window, cx)
    } else {
        entry(menu, "Logs".into(), Box::new(OpenLogs), &a.logs, says_why)
    };
    if let Some(previous) = &a.previous {
        // Only the containers that ran before have a previous instance.
        let restarted: Vec<PodContainer> = a
            .log_containers
            .iter()
            .filter(|c| c.has_previous)
            .cloned()
            .collect();
        menu = if several && !restarted.is_empty() && a.blocked.is_none() {
            log_picker(
                menu,
                "Previous Logs",
                restarted,
                true,
                &workspace,
                window,
                cx,
            )
        } else {
            entry(
                menu,
                "Previous Logs".into(),
                Box::new(OpenPreviousLogs),
                previous,
                says_why,
            )
        };
    }
    let label = shell_entry_label(&a);
    let menu = match a.shell_pod {
        // The container picker.
        Some(shell) if a.shell_picker && a.blocked.is_none() => {
            menu.submenu(label, window, cx, move |sub, _, _| {
                shell
                    .containers
                    .iter()
                    .fold(sub.label("Container"), |sub, name| {
                        let running = !shell.not_running.contains(name);
                        let (item, action) = shell_item(name, *name == shell.default, running);
                        sub.item(item.on_click(registry::on_click_or(
                            Box::new(action),
                            refuse_in(&workspace),
                        )))
                    })
            })
        }
        _ => entry(menu, label.into(), Box::new(OpenShell), &a.shell, says_why),
    };
    match a.unlock {
        Some(cluster) => menu.item(
            PopupMenuItem::new(unlock_label(&cluster))
                .on_click(registry::on_click(UnlockCluster { cluster })),
        ),
        None => menu,
    }
}

/// `text` as one line of the menu, cut in the middle when long: a reason keeps the name it
/// starts with and what it ends up saying.
fn menu_line(text: &str) -> String {
    flint_core::display::middle_truncate(text, MENU_LINE)
}

/// The Shell entry: a workload's names the pod it opens in.
fn shell_entry_label(a: &RowActions) -> String {
    match (&a.shell_pod, a.subject) {
        (Some(shell), Subject::Workload(_)) => format!("Shell in {}", menu_name(&shell.pod)),
        _ => "Shell".to_owned(),
    }
}

/// The Unlock entry of a protected cluster's row menu.
fn unlock_label(cluster: &str) -> String {
    format!("Unlock {}…", flint_core::display::short_name(cluster))
}

/// `label` as a submenu that picks one of `containers` for Logs (or Previous Logs, with
/// `previous`).
fn log_picker(
    menu: PopupMenu,
    label: &'static str,
    containers: Vec<PodContainer>,
    previous: bool,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let workspace = workspace.clone();
    menu.submenu(label, window, cx, move |sub, _, _| {
        containers.iter().fold(sub.label("Container"), |sub, c| {
            let (item, action) = log_item(c, previous);
            sub.item(item.on_click(registry::on_click_or(
                Box::new(action),
                refuse_in(&workspace),
            )))
        })
    })
}

/// An entry for `action`, disabled with the reason under it when it cannot run.
fn entry(
    menu: PopupMenu,
    label: SharedString,
    action: Box<dyn Action>,
    can: &Availability,
    says_why: bool,
    workspace: &WeakEntity<Workspace>,
) -> PopupMenu {
    let menu = menu.item(
        PopupMenuItem::new(label)
            .action(action.boxed_clone())
            .disabled(can.is_err())
            .on_click(registry::on_click_or(action, refuse_in(workspace))),
    );
    match can {
        Err(why) if says_why => menu.label(menu_line(why)),
        _ => menu,
    }
}

/// What a menu entry does with a refusal: a menu drawn from older state may offer an action
/// that no longer runs, and it says why, as the keys do.
fn refuse_in(
    workspace: &WeakEntity<Workspace>,
) -> impl Fn(&dyn Action, &registry::ActionError, &mut App) + 'static {
    let workspace = workspace.clone();
    move |action, e, cx| {
        let _ = workspace.update(cx, |ws, cx| {
            let why = refusal(ws, action, e, cx);
            ws.refuse(why, cx)
        });
    }
}

/// Why `action`, chosen in a row's menu, was refused, as one sentence ("Can't open a shell:
/// …."): an action that is not available says why from what the row's actions are now, never
/// by its internal name.
pub(in crate::ui) fn refusal(
    ws: &Workspace,
    action: &dyn Action,
    error: &registry::ActionError,
    cx: &App,
) -> String {
    let any = action.as_any();
    let opening = if any.is::<OpenLogs>() {
        Opening::Logs
    } else if any.is::<OpenPreviousLogs>() {
        Opening::PreviousLogs
    } else if let Some(a) = any.downcast_ref::<OpenContainerLogs>() {
        if a.previous {
            Opening::PreviousLogs
        } else {
            Opening::Logs
        }
    } else {
        Opening::Shell
    };
    let registry::ActionError::NotAvailable(_) = error else {
        return refused(opening, &reason(error));
    };
    let why = row_actions(ws, cx).and_then(|a| {
        let can = match opening {
            Opening::Logs => a.logs,
            Opening::PreviousLogs => a.previous?,
            Opening::Shell | Opening::LogsAndShell | Opening::RestartShell => a.shell,
            Opening::ReconnectLogs => a.logs,
        };
        can.err()
    });
    refused(opening, why.as_deref().unwrap_or("it cannot run now"))
}

/// What the menu `fill` builds from `a` shows, as data: the note above its entries (why nothing
/// can open) and every entry, a picker with its containers. Whether an entry is enabled and why
/// not come from the same checks as the menu's (a disabled entry's reason is given even where
/// the menu says it once, in the note).
#[cfg(any(test, feature = "automation"))]
pub fn menu_state(a: &RowActions) -> (Option<String>, Vec<MenuEntryState>) {
    use super::detail::{container_label, shell_label};

    let entry = |label: String, can: &Availability| MenuEntryState {
        label,
        enabled: can.is_ok(),
        reason: can.as_ref().err().map(|why| menu_line(why)),
        checked: None,
        submenu: Vec::new(),
    };
    let picker = |label: String, submenu: Vec<SubmenuEntryState>| MenuEntryState {
        label,
        enabled: true,
        reason: None,
        checked: None,
        submenu,
    };
    let logs_of = |containers: &mut dyn Iterator<Item = &PodContainer>| {
        containers
            .map(|c| SubmenuEntryState {
                label: container_label(&c.name, c.init, c.default),
                enabled: true,
            })
            .collect()
    };
    let several = a.log_containers.len() > 1;
    let mut entries = Vec::new();
    entries.push(if several && a.logs.is_ok() {
        picker("Logs".into(), logs_of(&mut a.log_containers.iter()))
    } else {
        entry("Logs".into(), &a.logs)
    });
    if let Some(previous) = &a.previous {
        let mut restarted = a
            .log_containers
            .iter()
            .filter(|c| c.has_previous)
            .peekable();
        entries.push(
            if several && restarted.peek().is_some() && a.blocked.is_none() {
                picker("Previous Logs".into(), logs_of(&mut restarted))
            } else {
                entry("Previous Logs".into(), previous)
            },
        );
    }
    let label = shell_entry_label(a);
    entries.push(match &a.shell_pod {
        Some(shell) if a.shell_picker && a.blocked.is_none() => picker(
            label,
            shell
                .containers
                .iter()
                .map(|name| {
                    let running = !shell.not_running.contains(name);
                    SubmenuEntryState {
                        label: shell_label(name, *name == shell.default, running),
                        enabled: running,
                    }
                })
                .collect(),
        ),
        _ => entry(label, &a.shell),
    });
    if let Some(cluster) = &a.unlock {
        entries.push(entry(unlock_label(cluster), &Ok(())));
    }
    (a.blocked.as_deref().map(menu_line), entries)
}

#[cfg(test)]
pub(in crate::ui) mod tests {
    use std::cell::Cell;

    thread_local! {
        /// How many times an open menu was built again, on this test's thread.
        pub static REBUILDS: Cell<usize> = const { Cell::new(0) };
    }

    use flint_core::WorkloadKind;
    use serde_json::json;

    use super::{MENU_LINE, RowActions, menu_state};
    use crate::ui::detail::{ShellPod, Subject};
    use crate::ui::object::PodContainer;

    fn container(name: &str, default: bool, has_previous: bool) -> PodContainer {
        PodContainer {
            name: name.into(),
            init: false,
            not_running: false,
            has_previous,
            default,
        }
    }

    fn pod_actions() -> RowActions {
        RowActions {
            blocked: None,
            subject: Subject::Pod,
            logs: Ok(()),
            previous: Some(Err("No container has restarted.".into())),
            shell: Ok(()),
            shell_pod: None,
            shell_picker: false,
            log_containers: vec![container("app", true, false)],
            unlock: None,
        }
    }

    fn state(a: &RowActions) -> serde_json::Value {
        let (note, entries) = menu_state(a);
        json!({"note": note, "entries": entries})
    }

    // The state names a row menu's entries as `fill` draws them: plain entries with why they are
    // disabled, pickers with their containers, and the cluster's reason once above.
    #[test]
    fn a_row_menus_entries_are_named_as_drawn() {
        assert_eq!(
            state(&pod_actions()),
            json!({"note": null, "entries": [
                {"label": "Logs", "enabled": true, "reason": null, "checked": null, "submenu": []},
                {"label": "Previous Logs", "enabled": false,
                 "reason": "No container has restarted.", "checked": null, "submenu": []},
                {"label": "Shell", "enabled": true, "reason": null, "checked": null, "submenu": []},
            ]})
        );

        let several = RowActions {
            previous: Some(Ok(())),
            log_containers: vec![
                container("app", true, true),
                container("sidecar", false, false),
            ],
            shell_pod: Some(ShellPod {
                pod: "web-1".into(),
                containers: vec!["app".into(), "sidecar".into()],
                default: "app".into(),
                not_running: vec!["sidecar".into()],
            }),
            shell_picker: true,
            ..pod_actions()
        };
        assert_eq!(
            state(&several)["entries"],
            json!([
                {"label": "Logs", "enabled": true, "reason": null, "checked": null, "submenu": [
                    {"label": "app (default)", "enabled": true},
                    {"label": "sidecar", "enabled": true},
                ]},
                {"label": "Previous Logs", "enabled": true, "reason": null, "checked": null, "submenu": [
                    {"label": "app (default)", "enabled": true},
                ]},
                {"label": "Shell", "enabled": true, "reason": null, "checked": null, "submenu": [
                    {"label": "app (default)", "enabled": true},
                    {"label": "sidecar (not running)", "enabled": false},
                ]},
            ])
        );

        let why = "alpha is not connected.".to_owned();
        let blocked = RowActions {
            blocked: Some(why.clone()),
            logs: Err(why.clone()),
            previous: None,
            shell: Err(why.clone()),
            subject: Subject::Workload(WorkloadKind::Deployment),
            shell_pod: Some(ShellPod {
                pod: "web-1".into(),
                containers: vec!["app".into()],
                default: "app".into(),
                not_running: Vec::new(),
            }),
            ..several
        };
        assert_eq!(
            state(&blocked),
            json!({"note": why, "entries": [
                {"label": "Logs", "enabled": false, "reason": why, "checked": null, "submenu": []},
                {"label": "Shell in web-1", "enabled": false, "reason": why, "checked": null, "submenu": []},
            ]})
        );

        // covers UP-54: on a protected cluster Shell says why, and Unlock follows it.
        let why = "alpha is protected (read-only in Flint) until you unlock it".to_owned();
        let protected = RowActions {
            shell: Err(why.clone()),
            unlock: Some("alpha".into()),
            ..pod_actions()
        };
        assert_eq!(
            state(&protected)["entries"].as_array().unwrap()[2..],
            [
                json!({"label": "Shell", "enabled": false, "reason": why, "checked": null, "submenu": []}),
                json!({"label": "Unlock alpha…", "enabled": true, "reason": null, "checked": null, "submenu": []}),
            ]
        );
    }

    // Menu items neither wrap nor shrink, so a line longer than the menu is drawn past its edge
    // (#338). Long names are cut in the middle, keeping what tells them apart and what is said
    // about them.
    #[test]
    fn every_line_of_a_row_menu_fits_it() {
        let long = "sidecar-proxy-with-a-very-long-container-name-for-the-mesh-0001";
        let cluster =
            "arn:aws:eks:eu-north-1:123456789012:cluster/production-platform-services-primary";
        let pod = "web-platform-services-canary-release-".repeat(6);
        let actions = RowActions {
            previous: Some(Err(format!(
                "container {long} has not restarted, so it has no previous logs"
            ))),
            shell: Err(format!("{cluster} is protected until you unlock it")),
            log_containers: vec![container(long, true, false), container("app", false, false)],
            shell_pod: Some(ShellPod {
                pod: pod.clone(),
                containers: vec![long.into()],
                default: long.into(),
                not_running: vec![long.into()],
            }),
            subject: Subject::Workload(WorkloadKind::Deployment),
            ..pod_actions()
        };
        let picker = RowActions {
            shell: Ok(()),
            shell_picker: true,
            subject: Subject::Pod,
            ..actions.clone()
        };
        let blocked = RowActions {
            blocked: Some(format!("{cluster} is not connected: the server refused it")),
            ..actions.clone()
        };

        for a in [&actions, &picker, &blocked] {
            let (note, entries) = menu_state(a);
            let mut lines: Vec<String> = note.into_iter().collect();
            for e in &entries {
                lines.push(e.label.clone());
                lines.extend(e.reason.clone());
                lines.extend(e.submenu.iter().map(|s| s.label.clone()));
            }
            for line in lines {
                assert!(line.chars().count() <= MENU_LINE, "too long: {line}");
            }
        }

        let (_, entries) = menu_state(&picker);
        let default = &entries[0].submenu[0].label;
        assert!(default.starts_with("sidecar-proxy"), "{default}");
        assert!(default.ends_with("-0001 (default)"), "{default}");
        let (_, entries) = menu_state(&actions);
        let why = entries[1].reason.as_deref().unwrap();
        assert!(why.ends_with("no previous logs"), "{why}");
    }
}
