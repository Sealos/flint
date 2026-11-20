//! The context menu of a pod or workload row (UP-47): Logs, Previous Logs and Shell.
//!
//! On a pod with several containers each entry is a container picker, the default container
//! marked; the keys (⌘L, ⇧⌘L, ⇧⌘E) open the default container at once (see "From rows" in
//! `docs/ARCHITECTURE.md`).
//!
//! Right-clicking a row selects it, as a click does, so the detail pane shows it; the menu's
//! entries are then the detail pane's registered actions ([`OpenLogs`], [`OpenPreviousLogs`],
//! [`OpenShell`], [`OpenContainerShell`]), performed through [`registry::on_click`], so the menu,
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

use super::detail::{Opening, Subject, container_label, reason, refused};
use super::object::PodContainer;
use super::registry::{
    self, OpenContainerLogs, OpenContainerShell, OpenLogs, OpenPreviousLogs, OpenShell,
};
use super::workspace::Workspace;

/// Builds a row's context menu (gpui-component's `context_menu` builder).
pub type RowMenu = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// The longest reason an action gives that a menu line shows; the rest is cut.
const MAX_REASON: usize = 80;

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
    pub shell_pod: Option<(String, Vec<String>, String)>,
    /// A pod's containers (init ones too), once fetched: what Logs and Previous Logs pick
    /// from.
    pub log_containers: Vec<PodContainer>,
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
        shell_pod: detail.shell_pod().map(|p| (p.pod, p.containers, p.default)),
        log_containers: match subject {
            Subject::Pod => detail.log_containers().to_vec(),
            _ => Vec::new(),
        },
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
        menu = menu.label(why.clone()).separator();
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
    let label = match (&a.shell_pod, a.subject) {
        (Some((pod, _, _)), Subject::Workload(_)) => format!("Shell in {pod}"),
        _ => "Shell".to_owned(),
    };
    match a.shell_pod {
        // The container picker.
        Some((_, containers, default)) if a.shell.is_ok() && containers.len() > 1 => {
            menu.submenu(label, window, cx, move |sub, _, _| {
                containers.iter().fold(sub.label("Container"), |sub, name| {
                    sub.item(
                        PopupMenuItem::new(container_label(name, false, *name == default))
                            .on_click(registry::on_click_or(
                                Box::new(OpenContainerShell {
                                    container: name.clone(),
                                }),
                                refuse_in(&workspace),
                            )),
                    )
                })
            })
        }
        _ => entry(menu, label.into(), Box::new(OpenShell), &a.shell, says_why),
    }
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
            sub.item(
                PopupMenuItem::new(container_label(&c.name, c.init, c.default)).on_click(
                    registry::on_click_or(
                        Box::new(OpenContainerLogs {
                            container: c.name.clone(),
                            previous,
                        }),
                        refuse_in(&workspace),
                    ),
                ),
            )
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
        Err(why) if says_why => menu.label(why.clone()),
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
            Opening::Shell | Opening::LogsAndShell => a.shell,
        };
        can.err()
    });
    refused(opening, why.as_deref().unwrap_or("it cannot run now"))
}

#[cfg(test)]
pub(in crate::ui) mod tests {
    use std::cell::Cell;

    thread_local! {
        /// How many times an open menu was built again, on this test's thread.
        pub static REBUILDS: Cell<usize> = const { Cell::new(0) };
    }
}
