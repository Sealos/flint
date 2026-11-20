//! The context menu of a pod or workload row (UP-47): Logs, Previous Logs and Shell.
//!
//! Right-clicking a row selects it, as a click does, so the detail pane shows it; the menu's
//! entries are then the detail pane's registered actions ([`OpenLogs`], [`OpenPreviousLogs`],
//! [`OpenShell`], [`OpenContainerShell`]), performed through [`registry::on_click`], so the menu,
//! the keys and the palette take one path and the same guards. An entry chosen before the row's
//! object has loaded waits for it (see `detail_actions`). The menu is built a frame after the
//! right-click, usually before the object arrives; it is built again once it has, so it shows
//! the container picker and the entries that turned out not to apply.
//!
//! The list draws the menu; the workspace hands it the builder ([`row_menu`]) for the kinds that
//! have these actions, so the list stays generic.

// module-tests: row_actions_tests.rs

use std::cell::Cell;
use std::rc::Rc;

use flint_core::KindId;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;

use super::detail::Subject;
use super::registry::{self, OpenContainerShell, OpenLogs, OpenPreviousLogs, OpenShell};
use super::workspace::Workspace;

/// Builds a row's context menu (gpui-component's `context_menu` builder).
pub type RowMenu = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// Whether rows of `kind` get the menu: pods and workloads.
pub fn has_row_actions(kind: &KindId) -> bool {
    Subject::of(kind) != Subject::Other
}

/// The menu for the rows of the workspace's list, acting on the row selected.
pub fn row_menu(workspace: WeakEntity<Workspace>) -> RowMenu {
    Rc::new(move |menu, window, cx| {
        let Some(ws) = workspace.upgrade() else {
            return menu;
        };
        let loading = ws
            .read(cx)
            .detail_view()
            .filter(|d| d.read(cx).is_loading());
        if let Some(detail) = loading {
            let workspace = workspace.clone();
            let rebuilt = Cell::new(false);
            // Detached: it holds the menu and the workspace weakly, and ends with the menu or
            // the detail pane.
            cx.observe_in(&detail, window, move |menu, detail, window, cx| {
                if rebuilt.get() || detail.read(cx).is_loading() {
                    return;
                }
                rebuilt.set(true);
                let workspace = workspace.clone();
                menu.rebuild(window, cx, move |menu, window, cx| {
                    match workspace.upgrade() {
                        Some(ws) => fill(menu, &ws, window, cx),
                        None => menu,
                    }
                });
            })
            .detach();
        }
        fill(menu, &ws, window, cx)
    })
}

/// What the menu shows, read from the workspace before building it.
struct Entries {
    /// Why nothing can open (the cluster is not connected).
    blocked: Option<String>,
    subject: Subject,
    logs: bool,
    previous: bool,
    shell: bool,
    /// The pod a shell opens in, and its app containers, once known.
    shell_pod: Option<(String, Vec<String>)>,
}

fn entries(ws: &Entity<Workspace>, cx: &App) -> Option<Entries> {
    let workspace = ws.read(cx);
    let detail = workspace.detail_view()?;
    let detail = detail.read(cx);
    Some(Entries {
        blocked: workspace.sessions_blocked(&detail.object().cluster, cx),
        subject: detail.subject(),
        logs: registry::enabled::<OpenLogs>(cx),
        previous: registry::enabled::<OpenPreviousLogs>(cx),
        shell: registry::enabled::<OpenShell>(cx),
        shell_pod: detail.shell_pod().map(|p| (p.pod, p.containers)),
    })
}

fn fill(
    menu: PopupMenu,
    ws: &Entity<Workspace>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(e) = entries(ws, cx) else {
        return menu;
    };
    let mut menu = menu;
    if let Some(reason) = &e.blocked {
        menu = menu.label(reason.clone()).separator();
    }
    let workload = matches!(e.subject, Subject::Workload(_));
    menu = menu.item(
        PopupMenuItem::new(if workload { "Logs of All Pods" } else { "Logs" })
            .action(Box::new(OpenLogs))
            .disabled(!e.logs)
            .on_click(registry::on_click(OpenLogs)),
    );
    if !workload {
        menu = menu.item(
            PopupMenuItem::new("Previous Logs")
                .action(Box::new(OpenPreviousLogs))
                .disabled(!e.previous)
                .on_click(registry::on_click(OpenPreviousLogs)),
        );
    }
    let label = match (&e.shell_pod, workload) {
        (Some((pod, _)), true) => format!("Shell in {pod}"),
        _ => "Shell".to_owned(),
    };
    match e.shell_pod {
        // The container picker.
        Some((_, containers)) if e.shell && containers.len() > 1 => {
            menu.submenu(label, window, cx, move |sub, _, _| {
                containers.iter().fold(sub.label("Container"), |sub, name| {
                    sub.item(
                        PopupMenuItem::new(name.clone()).on_click(registry::on_click(
                            OpenContainerShell {
                                container: name.clone(),
                            },
                        )),
                    )
                })
            })
        }
        _ => menu.item(
            PopupMenuItem::new(label)
                .action(Box::new(OpenShell))
                .disabled(!e.shell)
                .on_click(registry::on_click(OpenShell)),
        ),
    }
}
