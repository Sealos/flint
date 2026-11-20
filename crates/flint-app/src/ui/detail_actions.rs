//! The detail pane's registered actions ([`crate::ui::registry`]), their handlers, and the
//! checks the workspace's guards share with them. The pane's buttons, tabs and toggles dispatch
//! these actions, and so does a pod or workload row's context menu: selecting a row shows it
//! here, so Logs, Previous Logs and Shell act on the selected row (UP-47).
//!
//! Those three may be asked for before the object (or a workload's pods) has loaded, from a
//! row just selected: they then wait ([`Plan::Wait`]) and run once it has.

use flint_core::{LogSubject, LogTarget, PodSet, WorkloadPod, WorkloadTarget};
use gpui_kit::*;

use super::super::log_view::LogRequest;
use super::super::registry::{
    ActionError, Bind, Close, OnView, OpenContainerLogs, OpenContainerShell, OpenLogs,
    OpenPreviousLogs, OpenShell, Refresh, ShowOverview, ShowYaml, ToggleAnnotations, ToggleLabels,
    ToggleManagedFields, ToggleSecretValue,
};
use super::super::terminal_model::ShellRequest;
use super::{DetailEvent, DetailTab, DetailView, Fetch, Pending, Subject};

/// What Logs, Previous Logs or Shell does for the shown object now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) enum Plan<T> {
    /// Open this tab.
    Open(T),
    /// Wait until the pod (or the workload's pods) has loaded, then decide.
    Wait,
}

impl<T> Plan<T> {
    fn map<U>(self, f: impl FnOnce(T) -> U) -> Plan<U> {
        match self {
            Plan::Open(t) => Plan::Open(f(t)),
            Plan::Wait => Plan::Wait,
        }
    }
}

/// Where a shell opens: a pod and its app containers, with the one picked by default first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) struct ShellPod {
    pub pod: String,
    /// The app containers, in spec order.
    pub containers: Vec<String>,
    pub default: String,
}

impl DetailView {
    /// Every detail action and its handler, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<Refresh>(|view, _, window, cx| view.refresh(window, cx))
            .bind::<Close>(|_, _, _, cx| cx.emit(DetailEvent::Close))
            .bind::<ShowOverview>(|view, _, _, cx| view.show_tab(DetailTab::Overview, cx))
            .bind::<ShowYaml>(|view, _, _, cx| view.show_tab(DetailTab::Yaml, cx))
            .bind::<ToggleManagedFields>(|view, _, window, cx| {
                if view.has_managed_fields() {
                    view.toggle_managed_fields(window, cx)
                }
            })
            .bind::<ToggleLabels>(|view, _, _, cx| {
                view.show_labels = !view.show_labels;
                cx.notify();
            })
            .bind::<ToggleAnnotations>(|view, _, _, cx| {
                view.show_annotations = !view.show_annotations;
                cx.notify();
            })
            .bind::<ToggleSecretValue>(Self::toggle_secret_value)
            .bind::<OpenLogs>(|view, _, _, cx| view.run(Pending::Logs { previous: false }, cx))
            .bind::<OpenPreviousLogs>(|view, _, _, cx| {
                view.run(Pending::Logs { previous: true }, cx)
            })
            .bind::<OpenContainerLogs>(|view, action, _, cx| {
                match view.log_plan::<OpenContainerLogs>(Some(&action.container), action.previous) {
                    Ok(Plan::Open(request)) => cx.emit(DetailEvent::OpenLogs(request)),
                    Ok(Plan::Wait) => {}
                    Err(e) => tracing::warn!("{e}"),
                }
            })
            .bind::<OpenShell>(|view, _, _, cx| view.run(Pending::Shell, cx))
            .bind::<OpenContainerShell>(|view, action, _, cx| {
                match view.shell_plan::<OpenContainerShell>(Some(&action.container)) {
                    Ok(Plan::Open(request)) => cx.emit(DetailEvent::OpenShell(request)),
                    Ok(Plan::Wait) => {}
                    Err(e) => tracing::warn!("{e}"),
                }
            })
    }

    /// Runs Logs, Previous Logs or Shell now, or once what it needs has loaded.
    pub(super) fn run(&mut self, action: Pending, cx: &mut Context<Self>) {
        let plan = match action {
            Pending::Logs { previous: false } => self
                .log_plan::<OpenLogs>(None, false)
                .map(|p| p.map(DetailEvent::OpenLogs)),
            Pending::Logs { previous: true } => self
                .log_plan::<OpenPreviousLogs>(None, true)
                .map(|p| p.map(DetailEvent::OpenLogs)),
            Pending::Shell => self
                .shell_plan::<OpenShell>(None)
                .map(|p| p.map(DetailEvent::OpenShell)),
        };
        match plan {
            Ok(Plan::Open(event)) => {
                self.pending = None;
                cx.emit(event);
            }
            // The latest ask wins, as if it came after the load.
            Ok(Plan::Wait) => self.pending = Some(action),
            Err(e) => {
                self.pending = None;
                tracing::warn!("{e}");
            }
        }
    }

    /// Runs the action asked for while loading, once the object or the pods arrived.
    pub(super) fn run_pending(&mut self, cx: &mut Context<Self>) {
        if let Some(action) = self.pending {
            self.run(action, cx);
        }
    }

    /// The app containers of the fetched pod; empty for other kinds or before the fetch.
    fn pod_shell_containers(&self) -> impl Iterator<Item = &super::super::object::PodContainer> {
        self.log_containers().iter().filter(|c| !c.init)
    }

    /// Where Shell opens, once known: the shown pod, or a workload's first ready pod
    /// ([`WorkloadPod::first_ready`]).
    pub(in crate::ui) fn shell_pod(&self) -> Option<ShellPod> {
        match self.subject {
            Subject::Pod => {
                let containers: Vec<String> = self
                    .pod_shell_containers()
                    .map(|c| c.name.clone())
                    .collect();
                let default = self
                    .pod_shell_containers()
                    .find(|c| c.default)
                    .or_else(|| self.pod_shell_containers().next())?
                    .name
                    .clone();
                Some(ShellPod {
                    pod: self.object.name.clone(),
                    containers,
                    default,
                })
            }
            Subject::Workload(_) => {
                let pods = self.pods.as_ref()?.as_ref().ok()?;
                let pod = WorkloadPod::first_ready(pods)?;
                Some(ShellPod {
                    pod: pod.name.clone(),
                    containers: pod.containers.clone(),
                    default: pod.default_container.clone()?,
                })
            }
            Subject::Other => None,
        }
    }

    /// The shell tab [`OpenShell`] and [`OpenContainerShell`] open: the named container (or the
    /// default one) of the shown pod, or of a workload's first ready pod. Without a container
    /// it waits while the pod or the workload's pods load, or while a workload's pods are
    /// listed again. A deleted object has no shell.
    pub(in crate::ui) fn shell_plan<A: Action>(
        &self,
        container: Option<&str>,
    ) -> Result<Plan<ShellRequest>, ActionError> {
        if self.gone {
            return Err(ActionError::rejected::<A>(format!(
                "{} was deleted",
                self.object.name
            )));
        }
        let loading = match self.subject {
            Subject::Pod => match &self.fetch {
                Fetch::Loading => true,
                Fetch::Failed(e) => {
                    return Err(ActionError::rejected::<A>(format!(
                        "the pod could not be fetched: {e}"
                    )));
                }
                Fetch::Loaded { .. } => false,
            },
            // A relist runs once the workload changed: the pods listed before may be stale.
            Subject::Workload(_) if self.listing_pods => true,
            Subject::Workload(kind) => match &self.pods {
                None => true,
                Some(Err(e)) => {
                    return Err(ActionError::rejected::<A>(format!(
                        "the pods of {} {} could not be listed: {e}",
                        kind.name(),
                        self.object.name
                    )));
                }
                Some(Ok(_)) => false,
            },
            Subject::Other => {
                return Err(ActionError::rejected::<A>(
                    "a shell opens in a pod or a workload's pod",
                ));
            }
        };
        if loading {
            return match container {
                None => Ok(Plan::Wait),
                Some(_) => Err(ActionError::rejected::<A>(
                    "the containers are not known yet (still loading)",
                )),
            };
        }
        let Some(target) = self.shell_pod() else {
            return Err(ActionError::rejected::<A>(match self.subject {
                Subject::Workload(kind) => {
                    format!("no pod of {} {} is ready", kind.name(), self.object.name)
                }
                _ => "the pod has no app container".to_owned(),
            }));
        };
        let container = match container {
            Some(name) if target.containers.iter().any(|c| c == name) => name.to_owned(),
            Some(name) => {
                return Err(ActionError::rejected::<A>(format!(
                    "pod {} has no app container {name}",
                    target.pod
                )));
            }
            None => target.default,
        };
        Ok(Plan::Open(ShellRequest {
            cluster: self.object.cluster.clone(),
            namespace: self.object.namespace.clone().unwrap_or_default(),
            pod: target.pod,
            container: Some(container),
        }))
    }

    /// The fetched pod's containers; empty for other kinds or before the fetch.
    pub(in crate::ui) fn log_containers(&self) -> &[super::super::object::PodContainer] {
        match &self.fetch {
            Fetch::Loaded { overview, .. } => &overview.containers,
            _ => &[],
        }
    }

    /// The log tab [`OpenLogs`], [`OpenPreviousLogs`] and [`OpenContainerLogs`] open: for a
    /// pod, the named container (or the default one), and its previous instance only if it ran
    /// before (waiting for the pod to load when no container is named); for a workload, every
    /// container of its pods, merged (UP-45).
    pub(in crate::ui) fn log_plan<A: Action>(
        &self,
        container: Option<&str>,
        previous: bool,
    ) -> Result<Plan<LogRequest>, ActionError> {
        let cluster = self.object.cluster.clone();
        let namespace = self.object.namespace.clone().unwrap_or_default();
        match self.subject {
            Subject::Workload(kind) => {
                if previous || container.is_some() {
                    return Err(ActionError::rejected::<A>(format!(
                        "a {}'s logs merge its pods; open one pod for a container or its \
                         previous instance",
                        kind.name()
                    )));
                }
                return Ok(Plan::Open(LogRequest {
                    cluster,
                    subject: LogSubject::Workload(WorkloadTarget {
                        namespace,
                        pods: PodSet::Workload {
                            kind,
                            name: self.object.name.clone(),
                        },
                        container: None,
                    }),
                    previous: false,
                }));
            }
            Subject::Other => {
                return Err(ActionError::rejected::<A>(
                    "only pods and workloads have logs",
                ));
            }
            Subject::Pod => {}
        }
        match &self.fetch {
            Fetch::Loading if container.is_none() => return Ok(Plan::Wait),
            Fetch::Loading => {
                return Err(ActionError::rejected::<A>(
                    "the containers are not known yet (still loading)",
                ));
            }
            Fetch::Failed(e) => {
                return Err(ActionError::rejected::<A>(format!(
                    "the pod could not be fetched: {e}"
                )));
            }
            Fetch::Loaded { .. } => {}
        }
        let containers = self.log_containers();
        let found = match container {
            Some(name) => containers.iter().find(|c| c.name == name),
            None => containers.iter().find(|c| c.default),
        };
        let Some(found) = found else {
            return Err(ActionError::rejected::<A>(match container {
                Some(name) => format!("the pod has no container {name}"),
                None => "the pod has no app container".to_owned(),
            }));
        };
        if previous && !found.has_previous {
            return Err(ActionError::rejected::<A>(format!(
                "container {} has not restarted, so it has no previous logs",
                found.name
            )));
        }
        Ok(Plan::Open(LogRequest {
            cluster,
            subject: LogSubject::Pod(LogTarget {
                namespace,
                pod: self.object.name.clone(),
                container: Some(found.name.clone()),
            }),
            previous,
        }))
    }

    pub(super) fn register_actions<E: InteractiveElement>(element: E, cx: &Context<Self>) -> E {
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }

    fn show_tab(&mut self, tab: DetailTab, cx: &mut Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            cx.notify();
        }
    }

    /// [`ToggleManagedFields`] needs a fetched object that has managedFields.
    pub(in crate::ui) fn has_managed_fields(&self) -> bool {
        matches!(&self.fetch, Fetch::Loaded { detail, .. } if detail.managed_fields.is_some())
    }

    /// [`ToggleSecretValue`] applies to Secrets only.
    pub(in crate::ui) fn is_secret(&self) -> bool {
        self.is_secret
    }

    /// [`ToggleSecretValue`] takes a key of the fetched Secret's data.
    pub(in crate::ui) fn check_secret_key(
        &self,
        action: &ToggleSecretValue,
    ) -> Result<(), ActionError> {
        let known = match &self.fetch {
            Fetch::Loaded { overview, .. } => overview
                .secret
                .as_ref()
                .is_some_and(|s| s.entries.iter().any(|e| e.key == action.key)),
            _ => false,
        };
        if known {
            Ok(())
        } else {
            Err(ActionError::rejected::<ToggleSecretValue>(format!(
                "the Secret has no key {}",
                action.key
            )))
        }
    }

    fn toggle_secret_value(
        &mut self,
        action: &ToggleSecretValue,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_secret || self.check_secret_key(action).is_err() {
            return;
        }
        if !self.revealed.remove(&action.key) {
            self.revealed.insert(action.key.clone());
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    #[test]
    fn every_detail_action_is_bound() {
        assert_binds_scope(Scope::Detail, super::DetailView::bind_actions);
    }
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod view_tests;
