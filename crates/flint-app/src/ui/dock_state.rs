//! The dock's part of the visible view tree ([`crate::ui::view_state`]): whether it shows, its
//! height, and every tab with its cluster, its status and what its view reports (log tabs in
//! `log_state.rs`, shell tabs in `terminal_state.rs`).

use flint_core::LogSubject;
use gpui_kit::*;

use super::super::view_state::{
    ActivityTabState, CompareSideState, CompareTabState, DockState, Focus, TabClusterState,
    TabState, TabStatusState,
};
use super::{Dock, DockTab, TabKey};

impl Dock {
    /// The dock as data; `height` is its height in points.
    pub fn view_state(&self, height: f32, window: &Window, cx: &App) -> DockState {
        let tabs = self
            .tabs
            .iter()
            .map(|tab| {
                let item = tab.tab.item(cx);
                let (status, status_text, logs, shell) = match &tab.tab {
                    DockTab::Logs(view) => {
                        let view = view.read(cx);
                        let (status, text) = view.status_state();
                        (status, text, Some(view.view_state()), None)
                    }
                    DockTab::Shell(view) => {
                        let view = view.read(cx);
                        let (status, text) = view.status_state();
                        (status, text, None, Some(view.view_state()))
                    }
                    DockTab::Activity(_) | DockTab::Compare(_) | DockTab::Helm(_) => {
                        (TabStatusState::Watching, "Live".to_owned(), None, None)
                    }
                };
                let activity = match &tab.tab {
                    DockTab::Activity(view) => Some(ActivityTabState {
                        rows: view.read(cx).rows().iter().map(|r| r.label()).collect(),
                    }),
                    _ => None,
                };
                let compare = match &tab.tab {
                    DockTab::Compare(view) => {
                        let view = view.read(cx);
                        let model = view.model();
                        Some(CompareTabState {
                            title: model.title.clone(),
                            verdict: model.verdict.clone(),
                            show_same: view.show_same(),
                            include_volatile: view.include_volatile(),
                            sides: model
                                .headers
                                .iter()
                                .map(|side| CompareSideState {
                                    name: side.name.clone(),
                                    status: side.status.clone(),
                                })
                                .collect(),
                            rows: model
                                .rows
                                .iter()
                                .map(|row| format!("{}: {}", row.path, row.cells.join(", ")))
                                .collect(),
                        })
                    }
                    _ => None,
                };
                TabState {
                    id: tab.id,
                    kind: tab_kind(&item.tab_key()),
                    title: item.tab_title().to_string(),
                    active: self.active == Some(tab.id),
                    focused: item.tab_focus().contains_focused(window, cx),
                    cluster: TabClusterState {
                        context: tab.cluster.context.clone(),
                        server: tab
                            .cluster
                            .server
                            .as_deref()
                            .map(flint_core::redact::server_url),
                        chip: self.label(&tab.cluster.context),
                        repointed: tab.repointed,
                        removed: self.is_removed(&tab.cluster.context),
                    },
                    status,
                    status_text,
                    logs,
                    shell,
                    activity,
                    compare,
                }
            })
            .collect();
        DockState {
            visible: self.visible,
            height,
            focused: self.has_focus(window, cx),
            active: self.active,
            tabs,
        }
    }

    /// Where focus is in the dock, if it is: a tab (its lines, search field or terminal), or the
    /// dock itself.
    pub fn focus_in(&self, window: &Window, cx: &App) -> Option<Focus> {
        if !self.has_focus(window, cx) {
            return None;
        }
        let in_tab = self.tabs.iter().find_map(|tab| match &tab.tab {
            DockTab::Logs(view) => view.read(cx).focus_in(window, cx),
            DockTab::Shell(view) => view.read(cx).focus_in(window, cx),
            DockTab::Activity(_) | DockTab::Compare(_) | DockTab::Helm(_) => None,
        });
        Some(in_tab.unwrap_or(Focus::Dock))
    }
}

/// What kind of tab `key` opens: `pod_logs`, `workload_logs`, `previous_logs` or `shell`.
pub fn tab_kind(key: &TabKey) -> &'static str {
    match key {
        TabKey::Logs(request) if request.previous => "previous_logs",
        TabKey::Logs(request) => match request.subject {
            LogSubject::Pod(_) => "pod_logs",
            LogSubject::Workload(_) => "workload_logs",
        },
        TabKey::Shell(_) => "shell",
        TabKey::Activity(_) => "agent_activity",
        TabKey::Compare(..) => "compare",
        TabKey::Helm(_) => "helm_releases",
    }
}

#[cfg(test)]
mod tests {
    use flint_core::{LogSubject, LogTarget, PodSet, WorkloadKind, WorkloadTarget};

    use super::tab_kind;
    use crate::ui::dock::TabKey;
    use crate::ui::log_view::LogRequest;
    use crate::ui::terminal_model::ShellRequest;

    #[test]
    fn tabs_are_named_by_kind() {
        let pod = LogSubject::Pod(LogTarget {
            namespace: "demo".into(),
            pod: "web-1".into(),
            container: None,
        });
        let logs = |subject: &LogSubject, previous| {
            tab_kind(&TabKey::Logs(LogRequest {
                cluster: "test".into(),
                subject: subject.clone(),
                previous,
            }))
        };
        assert_eq!(logs(&pod, false), "pod_logs");
        assert_eq!(logs(&pod, true), "previous_logs");
        let workload = LogSubject::Workload(WorkloadTarget {
            namespace: "demo".into(),
            pods: PodSet::Workload {
                kind: WorkloadKind::Deployment,
                name: "web".into(),
            },
            container: None,
        });
        assert_eq!(logs(&workload, false), "workload_logs");
        assert_eq!(
            tab_kind(&TabKey::Shell(ShellRequest {
                cluster: "test".into(),
                namespace: "demo".into(),
                pod: "web-1".into(),
                container: None,
            })),
            "shell"
        );
    }
}
