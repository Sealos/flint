//! The context menu of a row: Logs, Previous Logs and Shell on a pod or workload (UP-47), and the
//! resource actions its kind has (UP-53, [`resource_actions`]).
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
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;

use super::detail::{
    Opening, PROTECTED_NOTE, ShellPod, Subject, log_item, protected_reason, reason, refused,
    shell_item,
};
use super::menu_fit::{
    dismiss_when_shrunk, fit_menu, fit_picker, note_width, pod_name_chars, wrapped_label,
};
use super::object::PodContainer;
use super::registry::{
    self, OpenContainerLogs, OpenLogs, OpenPreviousLogs, OpenShell, UnlockCluster,
};
use super::resource_actions::{self, ResourceAction, has_sessions};
#[cfg(any(test, feature = "automation"))]
use super::view_state::{MenuEntryState, SubmenuEntryState};
use super::workspace::Workspace;

/// Builds a row's context menu (gpui-component's `context_menu` builder).
pub type RowMenu = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// The longest reason an action gives that the menu shows; the rest is cut. Long enough for a
/// long context name and all that is said about it: reasons wrap ([`say`]).
const MAX_REASON: usize = 200;

/// The most of a pod's name the Shell entry shows, with its key, inside the menu's width.
const SHELL_POD_NAME: usize = 28;

/// Whether rows of `kind` get the menu: pods, workloads, and kinds with a resource action
/// (`deletable`: discovery lists the `delete` verb).
pub fn has_row_actions(kind: &KindId, deletable: bool) -> bool {
    Subject::of(kind) != Subject::Other || resource_actions::kind_has_actions(kind, deletable)
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
    pub unlock: Option<UnlockEntry>,
    /// The resource actions the row's kind has, in menu order, and whether each can run now.
    pub writes: Vec<WriteEntry>,
}

/// A resource action's entry in a row's menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteEntry {
    pub action: ResourceAction,
    pub can: Availability,
}

/// The Unlock entry of a protected cluster's row menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlockEntry {
    pub cluster: String,
    /// As shown: a custom name comes with the real one.
    pub name: String,
    /// The full context and server, for telling apart clusters with the same short name.
    pub tip: String,
}

/// The tooltip of the Unlock entry: exactly which cluster it unlocks.
fn unlock_tip(context: &str, server: Option<&str>) -> String {
    format!("{context}\n{}", super::workspace::server_text(server))
}

/// What the selected row's actions can do now, if its kind has any. The actions' guards
/// (`workspace_actions`) use the same checks.
pub fn row_actions(ws: &Workspace, cx: &App) -> Option<RowActions> {
    let detail = ws.detail_view()?;
    let detail = detail.read(cx);
    let subject = detail.subject();
    let writes: Vec<WriteEntry> = resource_actions::offered(ws, detail, cx)
        .into_iter()
        .map(|action| WriteEntry {
            action,
            can: resource_actions::available(ws, action, cx)
                .map_err(|why| first_line(&why, MAX_REASON)),
        })
        .collect();
    if subject == Subject::Other && writes.is_empty() {
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
        unlock: (blocked.is_none() && ws.is_protected(&detail.object().cluster, cx)).then(|| {
            UnlockEntry {
                cluster: detail.object().cluster.clone(),
                name: ws.cluster_names(cx).with_real(&detail.object().cluster),
                tip: unlock_tip(
                    &detail.object().cluster,
                    ws.cluster_summary(&detail.object().cluster, cx)
                        .and_then(|c| c.info.server.as_deref()),
                ),
            }
        }),
        blocked,
        subject,
        writes,
    })
}

/// Shell's availability as the menu draws it: on a protected cluster a short note, since the
/// Unlock entry right after it names the cluster. Everything else (refusals, the automation
/// state) keeps [`RowActions::shell`], which names it.
fn shell_in_menu(a: &RowActions) -> Availability {
    match (&a.shell, &a.unlock) {
        (Err(why), Some(unlock)) if *why == protected_reason(&unlock.cluster) => {
            Err(PROTECTED_NOTE.to_owned())
        }
        (shell, _) => shell.clone(),
    }
}

/// A resource action's availability as the menu draws it: on a protected cluster the short note,
/// as for Shell.
fn write_in_menu(a: &RowActions, write: &WriteEntry) -> Availability {
    match (&write.can, &a.unlock) {
        (Err(why), Some(unlock)) if *why == protected_reason(&unlock.cluster) => {
            Err(PROTECTED_NOTE.to_owned())
        }
        (can, _) => can.clone(),
    }
}

/// Whether the menu says "protected" once, right above Unlock, rather than under each entry:
/// when it has resource actions, all disabled for the same reason.
fn protected_once(a: &RowActions) -> bool {
    a.unlock.is_some()
        && a.writes
            .iter()
            .any(|w| write_in_menu(a, w).err().as_deref() == Some(PROTECTED_NOTE))
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
        dismiss_when_shrunk(window, cx);
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
    let mut menu = fit_menu(menu, window);
    if let Some(why) = &a.blocked {
        menu = say(menu, why).separator();
    }
    // The cluster's reason is said once, above.
    let says_why = a.blocked.is_none();
    let workspace = workspace.clone();
    let once = protected_once(&a);
    if has_sessions(a.subject) {
        menu = session_entries(menu, &a, once, &workspace, window, cx);
    }
    if !a.writes.is_empty() {
        if has_sessions(a.subject) {
            menu = menu.separator();
        }
        for write in &a.writes {
            let can = write_in_menu(&a, write);
            // The protected note is said once, above Unlock.
            let says = says_why
                && !(once && can.as_ref().err().map(String::as_str) == Some(PROTECTED_NOTE));
            menu = entry(
                menu,
                write.action.label().into(),
                write.action.boxed(),
                &can,
                says,
                &workspace,
            );
        }
    }
    if once {
        menu = say(menu, PROTECTED_NOTE);
    }
    match a.unlock {
        Some(UnlockEntry { cluster, name, tip }) => {
            let label = SharedString::from(unlock_label(&name));
            let tip = SharedString::from(tip);
            menu.item(
                PopupMenuItem::element(move |window, _| {
                    let tip = tip.clone();
                    div()
                        .id("menu-unlock")
                        .max_w(note_width(window))
                        .role(Role::MenuItem)
                        .aria_label(label.clone())
                        .aria_description(tip.clone())
                        .child(label.clone())
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                        .test_support()
                })
                .on_click(registry::on_click(UnlockCluster { cluster })),
            )
        }
        None => menu,
    }
}

/// Logs, Previous Logs and Shell. With `once`, Shell's protected note waits for the one above
/// Unlock.
fn session_entries(
    menu: PopupMenu,
    a: &RowActions,
    once: bool,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let mut menu = menu;
    // The cluster's reason is said once, above.
    let says_why = a.blocked.is_none();
    let plain =
        |menu, label, action, can, says_why| entry(menu, label, action, can, says_why, workspace);
    // A pod with several containers: Logs and Previous Logs pick one, as Shell does.
    let several = a.log_containers.len() > 1;
    menu = if several && a.logs.is_ok() {
        let containers = a.log_containers.clone();
        log_picker(menu, "Logs", containers, false, workspace, window, cx)
    } else {
        plain(menu, "Logs".into(), Box::new(OpenLogs), &a.logs, says_why)
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
                workspace,
                window,
                cx,
            )
        } else {
            plain(
                menu,
                "Previous Logs".into(),
                Box::new(OpenPreviousLogs),
                previous,
                says_why,
            )
        };
    }
    let label = shell_entry_label_within(a, pod_name_chars(SHELL_POD_NAME, window));
    let shell_shown = shell_in_menu(a);
    let says_shell = says_why
        && !(once && shell_shown.as_ref().err().map(String::as_str) == Some(PROTECTED_NOTE));
    match a.shell_pod.clone() {
        // The container picker.
        Some(shell) if a.shell_picker && a.blocked.is_none() => {
            let workspace = workspace.clone();
            menu.submenu(label, window, cx, move |sub, window, cx| {
                let sub = fit_picker(sub, window, cx).label("Container");
                shell.containers.iter().fold(sub, |sub, name| {
                    let running = !shell.not_running.contains(name);
                    let (item, action) = shell_item(name, *name == shell.default, running);
                    sub.item(item.on_click(registry::on_click_or(
                        Box::new(action),
                        refuse_in(&workspace),
                    )))
                })
            })
        }
        _ => plain(
            menu,
            label.into(),
            Box::new(OpenShell),
            &shell_shown,
            says_shell,
        ),
    }
}

/// Adds `text` to the menu as a note that wraps: a menu's labels neither wrap nor shrink, and a
/// reason must stay whole to say why.
fn say(menu: PopupMenu, text: &str) -> PopupMenu {
    let text = SharedString::from(text.to_owned());
    menu.item(
        PopupMenuItem::element(move |window, _| {
            wrapped_label(text.clone()).max_w(note_width(window))
        })
        .disabled(true),
    )
}

/// The Shell entry: a workload's names the pod it opens in, with at most `pod_chars` of its name
/// to fit a narrow window.
fn shell_entry_label_within(a: &RowActions, pod_chars: usize) -> String {
    match (&a.shell_pod, a.subject) {
        (Some(shell), Subject::Workload(_)) => {
            let pod = flint_core::display::middle_truncate(&shell.pod, pod_chars);
            format!("Shell in {pod}")
        }
        _ => "Shell".to_owned(),
    }
}

/// The Unlock entry of a protected cluster's row menu.
fn unlock_label(name: &str) -> String {
    format!("Unlock {name}…")
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
    menu.submenu(label, window, cx, move |sub, window, cx| {
        let sub = fit_picker(sub, window, cx).label("Container");
        containers.iter().fold(sub, |sub, c| {
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
        Err(why) if says_why => say(menu, why),
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
    if let Some(write) = ResourceAction::of(action) {
        let why = match error {
            registry::ActionError::NotAvailable(_) => row_actions(ws, cx)
                .and_then(|a| a.writes.into_iter().find(|w| w.action == write))
                .and_then(|w| w.can.err()),
            other => Some(reason(other)),
        };
        return refused(
            Opening::Write(write),
            why.as_deref().unwrap_or("it cannot run now"),
        );
    }
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
            Opening::Write(_) => return None,
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
        reason: can.as_ref().err().cloned(),
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
    if has_sessions(a.subject) {
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
        let label = shell_entry_label_within(a, SHELL_POD_NAME);
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
            _ => entry(label, &shell_in_menu(a)),
        });
    }
    for write in &a.writes {
        entries.push(entry(
            write.action.label().to_owned(),
            &write_in_menu(a, write),
        ));
    }
    if let Some(unlock) = &a.unlock {
        entries.push(entry(unlock_label(&unlock.name), &Ok(())));
    }
    (a.blocked.clone(), entries)
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

    use flint_core::redact::first_line;

    use super::{
        MAX_REASON, PROTECTED_NOTE, RowActions, SHELL_POD_NAME, UnlockEntry, WriteEntry,
        menu_state, protected_once, protected_reason, shell_entry_label_within, unlock_tip,
    };
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
            writes: Vec::new(),
        }
    }

    fn state(a: &RowActions) -> serde_json::Value {
        let (note, entries) = menu_state(a);
        json!({"note": note, "entries": entries})
    }

    // The state names a row menu's entries as `fill` draws them: plain entries with why they are
    // disabled, pickers with their containers, and the cluster's reason once above.
    #[test]
    fn the_unlock_tip_shows_the_cleaned_server_address() {
        let tip = unlock_tip(
            "alpha",
            Some("https://admin:hunter2@prod.example:6443/?token=abc"),
        );
        assert_eq!(tip, "alpha\nhttps://prod.example:6443");
        assert_eq!(unlock_tip("alpha", None), "alpha\nno server");
    }

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
        // The menu's note is short; the reason refusals use still names the cluster.
        let why = PROTECTED_NOTE.to_owned();
        let protected = RowActions {
            shell: Err(protected_reason("alpha")),
            unlock: Some(UnlockEntry {
                cluster: "alpha".into(),
                name: "alpha".into(),
                tip: "alpha\nhttps://example.invalid".into(),
            }),
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

    // covers UP-54: on a protected cluster every resource action is disabled, the state says why
    // for each, and Unlock follows them.
    #[test]
    fn a_protected_rows_resource_actions_say_why() {
        use crate::ui::resource_actions::ResourceAction;

        let why = protected_reason("alpha");
        let protected = RowActions {
            subject: Subject::Workload(WorkloadKind::Deployment),
            previous: None,
            shell: Err(why.clone()),
            writes: [ResourceAction::Scale, ResourceAction::Delete]
                .into_iter()
                .map(|action| WriteEntry {
                    action,
                    can: Err(why.clone()),
                })
                .collect(),
            unlock: Some(UnlockEntry {
                cluster: "alpha".into(),
                name: "alpha".into(),
                tip: "alpha\nhttps://example.invalid".into(),
            }),
            ..pod_actions()
        };
        assert!(protected_once(&protected));
        let note = PROTECTED_NOTE;
        assert_eq!(
            state(&protected)["entries"].as_array().unwrap()[1..],
            [
                json!({"label": "Shell", "enabled": false, "reason": note, "checked": null, "submenu": []}),
                json!({"label": "Scale…", "enabled": false, "reason": note, "checked": null, "submenu": []}),
                json!({"label": "Delete…", "enabled": false, "reason": note, "checked": null, "submenu": []}),
                json!({"label": "Unlock alpha…", "enabled": true, "reason": null, "checked": null, "submenu": []}),
            ]
        );
    }

    // covers UP-55: two clusters can share a short name, so the Unlock entry's tooltip says
    // which context and server it unlocks.
    #[test]
    fn the_unlock_tooltip_names_the_full_context_and_server() {
        let arn = "arn:aws:eks:eu-west-1:123456789012:cluster/example-prod";
        assert_eq!(
            unlock_tip(arn, Some("https://example.invalid")),
            format!("{arn}\nhttps://example.invalid")
        );
        assert_eq!(unlock_tip("a", None), "a\nno server");
    }

    // Menu labels neither wrap nor shrink, so a long one is drawn past the menu's edge (#338).
    // Pickers show names in full, so containers that differ in the middle stay apart; a reason
    // stays whole, so it still says why; only the one pod in "Shell in" is cut.
    #[test]
    fn long_names_stay_readable_in_a_row_menu() {
        let http = "mesh-production-app-http-instrumentation-proxy";
        let grpc = "mesh-production-app-grpc-instrumentation-proxy";
        let picker = RowActions {
            log_containers: vec![container(http, true, false), container(grpc, false, false)],
            ..pod_actions()
        };
        let (_, entries) = menu_state(&picker);
        let labels: Vec<&str> = entries[0]
            .submenu
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(labels, [format!("{http} (default)").as_str(), grpc]);

        let cluster =
            "arn:aws:eks:eu-north-1:123456789012:cluster/production-platform-services-primary";
        let why = format!("{cluster} is protected (read-only in Flint) until you unlock it");
        assert_eq!(first_line(&why, MAX_REASON), why, "it still says why");

        let workload = RowActions {
            subject: Subject::Workload(WorkloadKind::Deployment),
            shell_pod: Some(ShellPod {
                pod: "web-platform-services-canary-release-".repeat(6),
                containers: vec!["app".into()],
                default: "app".into(),
                not_running: Vec::new(),
            }),
            ..pod_actions()
        };
        let label = shell_entry_label_within(&workload, SHELL_POD_NAME);
        assert!(label.starts_with("Shell in web-platform"), "{label}");
        assert_eq!(label.chars().count(), "Shell in ".len() + SHELL_POD_NAME);
        let narrow = shell_entry_label_within(&workload, 10);
        assert_eq!(narrow.chars().count(), "Shell in ".len() + 10, "{narrow}");
    }
}
