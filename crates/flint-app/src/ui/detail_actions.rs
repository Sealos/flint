//! The detail pane's registered actions ([`crate::ui::registry`]), their handlers, and the
//! checks the workspace's guards share with them. The pane's buttons, tabs and toggles dispatch
//! these actions, and so does a pod or workload row's context menu: selecting a row shows it
//! here, so Logs, Previous Logs and Shell act on the selected row (UP-47).
//!
//! Those three may be asked for before the object has loaded, from a row just selected: they
//! then wait ([`Plan::Wait`]) and run once it has, or give up after [`PENDING_TIMEOUT`]. Shell
//! on a workload always waits for a current list of its pods (see `detail_pods`). An action
//! that is refused, at once or once loaded, says why ([`DetailEvent::Refused`]).

use flint_core::{LogSubject, LogTarget, PodSet, WorkloadPod, WorkloadTarget};
use gpui_kit::*;
use serde_json::Value;

use super::super::log_view::LogRequest;
use super::super::registry::{
    ActionError, Bind, Close, OnView, OpenContainerLogs, OpenContainerShell, OpenLogs,
    OpenPreviousLogs, OpenShell, Refresh, ShowOverview, ShowYaml, ToggleAnnotations, ToggleLabels,
    ToggleManagedFields, ToggleSecretValue,
};
use super::super::terminal_model::ShellRequest;
use super::{
    DetailEvent, DetailTab, DetailView, Fetch, PENDING_TIMEOUT, Pending, Pods, Subject, TakeFocus,
    Waiting,
};

/// What Logs, Previous Logs or Shell does for the shown object now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) enum Plan<T> {
    /// Open this tab.
    Open(T),
    /// Wait until the pod (or the workload's pods) has loaded, then decide.
    Wait,
}

/// A tab to open.
enum Opened {
    Logs(LogRequest),
    Shell(ShellRequest),
}

impl Opened {
    fn event(self, focus: TakeFocus) -> DetailEvent {
        match self {
            Opened::Logs(request) => DetailEvent::OpenLogs(request, focus),
            Opened::Shell(request) => DetailEvent::OpenShell(request, focus),
        }
    }
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

/// Why an action was refused, in words for the user.
pub(in crate::ui) fn reason(error: &ActionError) -> String {
    match error {
        ActionError::Rejected { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

/// What a refused Logs, Previous Logs or Shell was to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum Opening {
    Logs,
    PreviousLogs,
    Shell,
    /// Both Logs and Shell, refused for the same reason (the pane's caption says it once).
    LogsAndShell,
}

impl Opening {
    /// What could not be done, as the refusal's sentence starts.
    fn cant(self) -> &'static str {
        match self {
            Opening::Logs => "Can't open the logs",
            Opening::PreviousLogs => "Can't open the previous logs",
            Opening::Shell => "Can't open a shell",
            Opening::LogsAndShell => "Can't open the logs or a shell",
        }
    }
}

/// A refusal as one sentence that names what was refused, then why: "Can't open a shell: no
/// pod of Deployment web is ready." Every refusal the strip over the dock and the pane's
/// captions show is worded here.
pub(in crate::ui) fn refused(opening: Opening, why: &str) -> String {
    let why = why.trim_end();
    let end = if why.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    };
    format!("{}: {why}{end}", opening.cant())
}

impl From<&Pending> for Opening {
    fn from(action: &Pending) -> Self {
        match action {
            Pending::Logs { previous: false } => Opening::Logs,
            Pending::Logs { previous: true } => Opening::PreviousLogs,
            Pending::Shell { .. } => Opening::Shell,
        }
    }
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
            .bind::<OpenLogs>(|view, _, window, cx| {
                view.request(Pending::Logs { previous: false }, window, cx)
            })
            .bind::<OpenPreviousLogs>(|view, _, window, cx| {
                view.request(Pending::Logs { previous: true }, window, cx)
            })
            .bind::<OpenContainerLogs>(|view, action, _, cx| {
                match view.log_plan::<OpenContainerLogs>(Some(&action.container), action.previous) {
                    Ok(Plan::Open(request)) => {
                        cx.emit(DetailEvent::OpenLogs(request, TakeFocus::Yes))
                    }
                    Ok(Plan::Wait) => {}
                    Err(e) => {
                        let previous = action.previous;
                        view.refuse(&Pending::Logs { previous }, &e, cx)
                    }
                }
            })
            .bind::<OpenShell>(|view, _, window, cx| {
                view.request(Pending::Shell { container: None }, window, cx)
            })
            .bind::<OpenContainerShell>(|view, action, window, cx| {
                let container = Some(action.container.clone());
                view.request(Pending::Shell { container }, window, cx)
            })
    }

    /// Runs Logs, Previous Logs or Shell now, or once what it needs has loaded. The latest ask
    /// wins over one still waiting.
    pub(super) fn request(&mut self, action: Pending, window: &mut Window, cx: &mut Context<Self>) {
        match self.plan(&action, false) {
            Ok(Plan::Open(opened)) => {
                self.pending = None;
                cx.emit(opened.event(TakeFocus::Yes));
            }
            Ok(Plan::Wait) => {
                let workload_shell = matches!(
                    (&action, self.subject),
                    (Pending::Shell { .. }, Subject::Workload(_))
                );
                self.wait_for(action, TakeFocus::IfStill(window.focused(cx)), cx);
                // Where focus settles once this ask is handled (a menu the ask came from gives
                // focus back as it closes) is what must still have it when the wait ends.
                let this = cx.weak_entity();
                window.defer(cx, move |window, cx| {
                    let focus = window.focused(cx);
                    let _ = this.update(cx, |view, _| {
                        if let Some(waiting) = &mut view.pending {
                            waiting.focus = TakeFocus::IfStill(focus);
                        }
                    });
                });
                if workload_shell {
                    self.list_pods(flint_core::Freshness::Current, cx);
                }
            }
            Err(e) => {
                self.pending = None;
                self.refuse(&action, &e, cx);
            }
        }
        cx.notify();
    }

    /// Waits to run `action` until what it needs has loaded, for [`PENDING_TIMEOUT`] at most.
    pub(super) fn wait_for(&mut self, action: Pending, focus: TakeFocus, cx: &mut Context<Self>) {
        let timer = cx.background_executor().timer(PENDING_TIMEOUT);
        // Replacing the wait drops this task, so it only ever ends its own wait.
        let timeout = cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |view, cx| view.give_up(cx));
        });
        self.pending = Some(Waiting {
            action,
            focus,
            _timeout: timeout,
        });
    }

    fn give_up(&mut self, cx: &mut Context<Self>) {
        let Some(waiting) = self.pending.take() else {
            return;
        };
        // A list that hangs must not hold the next Shell: it starts its own.
        if let (Pending::Shell { .. }, Subject::Workload(_)) = (&waiting.action, self.subject) {
            self.listing = None;
            self.current_wanted = false;
        }
        let what = self.waiting_for(&waiting.action);
        let seconds = PENDING_TIMEOUT.as_secs();
        cx.emit(DetailEvent::Refused(refused(
            Opening::from(&waiting.action),
            &format!("gave up waiting for {what} ({seconds} s)"),
        )));
        cx.notify();
    }

    /// What a waiting action waits for, in words.
    pub(in crate::ui) fn waiting_for(&self, action: &Pending) -> String {
        match (action, self.subject) {
            (Pending::Shell { .. }, Subject::Workload(kind)) => {
                format!(
                    "the pods of {} {} to be listed",
                    kind.name(),
                    self.object.name
                )
            }
            _ => format!("{} to load", self.object.name),
        }
    }

    /// What is waiting, if anything, and for what: for the pane to show.
    pub(in crate::ui) fn waiting(&self) -> Option<String> {
        let waiting = self.pending.as_ref()?;
        Some(format!(
            "{} opens once {}",
            action_name(&waiting.action),
            self.waiting_for(&waiting.action)
                .replace(" to be listed", " are listed")
                .replace(" to load", " has loaded")
        ))
    }

    fn refuse(&mut self, action: &Pending, error: &ActionError, cx: &mut Context<Self>) {
        cx.emit(DetailEvent::Refused(refused(action.into(), &reason(error))));
    }

    /// Runs the action waiting, now that the object has loaded (or failed to). A workload's
    /// Shell keeps waiting: it waits for its pods ([`DetailView::run_pending_listed`]).
    pub(super) fn run_pending(&mut self, cx: &mut Context<Self>) {
        self.settle(false, cx);
    }

    /// Runs the action waiting, now that a current list of the workload's pods arrived.
    pub(super) fn run_pending_listed(&mut self, cx: &mut Context<Self>) {
        self.settle(true, cx);
    }

    fn settle(&mut self, listed: bool, cx: &mut Context<Self>) {
        let Some(waiting) = &self.pending else {
            return;
        };
        match self.plan(&waiting.action, listed) {
            Ok(Plan::Wait) => {}
            Ok(Plan::Open(opened)) => {
                let waiting = self.pending.take().expect("checked above");
                cx.emit(opened.event(waiting.focus));
            }
            Err(e) => {
                let waiting = self.pending.take().expect("checked above");
                self.refuse(&waiting.action, &e, cx);
            }
        }
        cx.notify();
    }

    /// What `action` does now. `listed`: a current list of a workload's pods just arrived, so
    /// a Shell into it can decide.
    fn plan(&self, action: &Pending, listed: bool) -> Result<Plan<Opened>, ActionError> {
        Ok(match action {
            Pending::Logs { previous: false } => {
                self.log_plan::<OpenLogs>(None, false)?.map(Opened::Logs)
            }
            Pending::Logs { previous: true } => self
                .log_plan::<OpenPreviousLogs>(None, true)?
                .map(Opened::Logs),
            Pending::Shell { container: None } => self
                .shell_plan::<OpenShell>(None, listed)?
                .map(Opened::Shell),
            Pending::Shell {
                container: Some(name),
            } => self
                .shell_plan::<OpenContainerShell>(Some(name), listed)?
                .map(Opened::Shell),
        })
    }

    /// The app containers of the fetched pod; empty for other kinds or before the fetch.
    fn pod_shell_containers(&self) -> impl Iterator<Item = &super::super::object::PodContainer> {
        self.log_containers().iter().filter(|c| !c.init)
    }

    /// Why the fetched pod takes no shell any more: it finished or is being deleted.
    fn pod_finished(&self) -> Option<String> {
        let Fetch::Loaded { detail, .. } = &self.fetch else {
            return None;
        };
        let object = &detail.object;
        let name = &self.object.name;
        if object
            .pointer("/metadata/deletionTimestamp")
            .is_some_and(|t| !t.is_null())
        {
            return Some(format!("pod {name} is being deleted"));
        }
        match object.pointer("/status/phase").and_then(Value::as_str) {
            Some(phase @ ("Succeeded" | "Failed")) => {
                Some(format!("pod {name} has finished ({phase})"))
            }
            _ => None,
        }
    }

    /// Where Shell opens, as far as known now: the shown pod, or the first ready pod of the
    /// workload's pods as last listed ([`WorkloadPod::first_ready`]). Shell itself lists them
    /// again first.
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
            // Listed before the workload changed, a pod named could be gone: none is named
            // until they are listed again.
            Subject::Workload(_) => match &self.pods {
                Pods::Listed(pods) if !self.pods_stale => shell_pod_of(pods),
                _ => None,
            },
            Subject::Other => None,
        }
    }

    /// Whether [`OpenShell`] (no container) or [`OpenContainerShell`] (a container) can be
    /// asked for now, as far as known: what the guards, the row menu and the pane's button
    /// show. Asked for while the object or a workload's pods are not known yet, it waits.
    pub(in crate::ui) fn shell_check<A: Action>(
        &self,
        container: Option<&str>,
    ) -> Result<(), ActionError> {
        if self.gone {
            return Err(ActionError::rejected::<A>(format!(
                "{} was deleted",
                self.object.name
            )));
        }
        let target = match self.subject {
            Subject::Other => {
                return Err(ActionError::rejected::<A>(
                    "a shell opens in a pod or a workload's pod",
                ));
            }
            Subject::Pod => match &self.fetch {
                Fetch::Loading => return Ok(()),
                Fetch::Failed(e) => {
                    return Err(ActionError::rejected::<A>(format!(
                        "the pod could not be fetched: {e}"
                    )));
                }
                Fetch::Loaded { .. } => {
                    if let Some(why) = self.pod_finished() {
                        return Err(ActionError::rejected::<A>(why));
                    }
                    self.shell_pod()
                }
            },
            // Not known, or not to be trusted (listed before the workload changed, or the list
            // failed): Shell lists the pods when asked, so it may run.
            Subject::Workload(_) if self.pods_stale => return Ok(()),
            Subject::Workload(_) => match &self.pods {
                Pods::NotListed | Pods::Failed(_) => return Ok(()),
                Pods::Listed(pods) => shell_pod_of(pods),
            },
        };
        let Some(target) = target else {
            return Err(ActionError::rejected::<A>(self.no_shell_pod()));
        };
        check_container::<A>(&target, container).map(|_| ())
    }

    fn list_failed(&self, kind: flint_core::WorkloadKind, error: &str) -> String {
        format!(
            "the pods of {} {} could not be listed: {error}",
            kind.name(),
            self.object.name
        )
    }

    fn no_shell_pod(&self) -> String {
        match self.subject {
            Subject::Workload(kind) => {
                format!("no pod of {} {} is ready", kind.name(), self.object.name)
            }
            _ => "the pod has no app container".to_owned(),
        }
    }

    /// The shell tab [`OpenShell`] and [`OpenContainerShell`] open: the named container (or the
    /// default one) of the shown pod, once it has loaded, or of the first ready pod of a
    /// workload, once its pods were just listed (`listed`) and never from a list before. A
    /// deleted or finished pod, or a failed list, has no shell.
    pub(in crate::ui) fn shell_plan<A: Action>(
        &self,
        container: Option<&str>,
        listed: bool,
    ) -> Result<Plan<ShellRequest>, ActionError> {
        if self.gone {
            return Err(ActionError::rejected::<A>(format!(
                "{} was deleted",
                self.object.name
            )));
        }
        let target = match self.subject {
            Subject::Other => {
                return Err(ActionError::rejected::<A>(
                    "a shell opens in a pod or a workload's pod",
                ));
            }
            Subject::Pod => match &self.fetch {
                Fetch::Loading => return Ok(Plan::Wait),
                Fetch::Failed(e) => {
                    return Err(ActionError::rejected::<A>(format!(
                        "the pod could not be fetched: {e}"
                    )));
                }
                Fetch::Loaded { .. } => {
                    if let Some(why) = self.pod_finished() {
                        return Err(ActionError::rejected::<A>(why));
                    }
                    self.shell_pod()
                }
            },
            Subject::Workload(_) if !listed => return Ok(Plan::Wait),
            Subject::Workload(kind) => match &self.pods {
                Pods::Listed(pods) => shell_pod_of(pods),
                Pods::Failed(e) => {
                    return Err(ActionError::rejected::<A>(self.list_failed(kind, e)));
                }
                Pods::NotListed => return Ok(Plan::Wait),
            },
        };
        let Some(target) = target else {
            return Err(ActionError::rejected::<A>(self.no_shell_pod()));
        };
        let container = check_container::<A>(&target, container)?;
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

fn shell_pod_of(pods: &[WorkloadPod]) -> Option<ShellPod> {
    let pod = WorkloadPod::first_ready(pods)?;
    Some(ShellPod {
        pod: pod.name.clone(),
        containers: pod.containers.clone(),
        default: pod.default_container.clone()?,
    })
}

/// The container a shell opens in: `container` if `target` has it, else its default one.
fn check_container<A: Action>(
    target: &ShellPod,
    container: Option<&str>,
) -> Result<String, ActionError> {
    match container {
        Some(name) if target.containers.iter().any(|c| c == name) => Ok(name.to_owned()),
        Some(name) => Err(ActionError::rejected::<A>(format!(
            "pod {} has no app container {name}",
            target.pod
        ))),
        None => Ok(target.default.clone()),
    }
}

/// An action's name, as the menu and the palette say it.
fn action_name(action: &Pending) -> &'static str {
    match action {
        Pending::Logs { previous: false } => "Logs",
        Pending::Logs { previous: true } => "Previous Logs",
        Pending::Shell { .. } => "Shell",
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::registry::{Scope, tests::assert_binds_scope};

    #[test]
    fn every_detail_action_is_bound() {
        assert_binds_scope(Scope::Detail, super::DetailView::bind_actions);
    }

    #[test]
    fn a_refusal_is_a_sentence_naming_what_was_refused() {
        use super::{Opening, refused};
        assert_eq!(
            refused(Opening::Shell, "no pod of Deployment notready is ready"),
            "Can't open a shell: no pod of Deployment notready is ready."
        );
        assert_eq!(
            refused(Opening::Logs, "the server said no."),
            "Can't open the logs: the server said no."
        );
        assert_eq!(
            refused(Opening::PreviousLogs, "container app has not restarted "),
            "Can't open the previous logs: container app has not restarted."
        );
    }
}

#[cfg(test)]
#[path = "detail_tests.rs"]
mod view_tests;
