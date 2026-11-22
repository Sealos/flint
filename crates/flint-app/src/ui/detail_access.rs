//! What RBAC says about the selected object's actions, for showing them (#267): an action the
//! cluster clearly forbids is disabled, saying which permission it lacks.
//!
//! Display only. No answer permits anything: the write gate and the API server still decide every
//! request. An answer that is not clear ([`Access::Unknown`]) leaves the action enabled, so the
//! server answers it. The reason is Flint's own words, never the server's.
//!
//! The core caches the checks ([`flint_core::access`]). The pane asks again when the connection
//! it is on changes, and once the answers are due, while it stays open.

use std::collections::HashMap;

use flint_core::KindId;
use flint_core::access::{ACCESS_FRESH_FOR, Access, AccessCheck};
use gpui_kit::*;

use super::super::resource_actions::{self, ResourceAction};
use super::{DetailView, Pods, Subject};

/// An action RBAC can forbid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum Gated {
    Edit,
    Logs,
    Shell,
    Write(ResourceAction),
}

/// The most pods of a workload whose logs are asked about one by one. Past it Logs stays enabled:
/// a refusal of some pods would not show that all of them are refused.
const MAX_LOG_TARGETS: usize = 16;

/// What the pane of an object of `kind` may need to show.
fn gated_for(kind: &KindId, subject: Subject) -> Vec<Gated> {
    let mut gated = vec![Gated::Edit];
    if subject != Subject::Other {
        gated.extend([Gated::Logs, Gated::Shell]);
    }
    gated.extend(
        resource_actions::offered_for(kind, true, None)
            .into_iter()
            .map(Gated::Write),
    );
    gated
}

/// Why `check` is refused, in Flint's words.
fn reason(check: &AccessCheck) -> String {
    let resource = match &check.subresource {
        Some(sub) => format!("{}/{sub}", check.resource),
        None => check.resource.clone(),
    };
    let place = match &check.namespace {
        Some(namespace) => format!("in {namespace}"),
        None => "on this cluster".to_owned(),
    };
    format!(
        "your access does not allow {} on {resource} {place}",
        check.verb
    )
}

/// The pods a workload's Logs and Shell would reach, once they are listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Targets {
    logs: Vec<String>,
    shell: Option<String>,
}

/// The connection the answers are for, and the targets asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Followed {
    server: Option<String>,
    targets: Targets,
}

/// The object's checks and their answers.
pub(in crate::ui) struct ObjectAccess {
    kind: KindId,
    namespace: Option<String>,
    name: String,
    subject: Subject,
    targets: Targets,
    answers: HashMap<AccessCheck, Access>,
    followed: Option<Followed>,
    _task: Option<Task<()>>,
}

impl ObjectAccess {
    pub(super) fn new(
        kind: &KindId,
        namespace: Option<&str>,
        name: &str,
        subject: Subject,
    ) -> Self {
        ObjectAccess {
            kind: kind.clone(),
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
            subject,
            targets: Targets::default(),
            answers: HashMap::new(),
            followed: None,
            _task: None,
        }
    }

    /// What `what` needs, asked by the name of its target (a Role can limit a verb with
    /// `resourceNames`). A workload's Logs and Shell ask nothing until its pods are listed.
    ///
    /// A shell always sends a GET WebSocket upgrade, so `get` on `pods/exec` is required. `create`
    /// is required too only on servers enforcing `AuthorizePodWebsocketUpgradeCreatePermission`,
    /// so its refusal alone proves nothing. A drain patches the node and lists pods cluster-wide.
    fn checks(&self, what: Gated) -> Vec<AccessCheck> {
        let on =
            |group: &str, resource: &str, sub: Option<&str>, name: Option<&str>, verb: &str| {
                AccessCheck {
                    namespace: self.namespace.clone(),
                    group: group.to_owned(),
                    resource: resource.to_owned(),
                    subresource: sub.map(str::to_owned),
                    name: name.map(str::to_owned),
                    verb: verb.to_owned(),
                }
            };
        let (group, plural) = (self.kind.group(), self.kind.plural());
        let object = |verb: &str| on(group, plural, None, Some(&self.name), verb);
        let pod = |sub: &str, name: &str| on("", "pods", Some(sub), Some(name), "get");
        match what {
            Gated::Edit => vec![object("update")],
            Gated::Logs => match self.subject {
                Subject::Pod => vec![pod("log", &self.name)],
                Subject::Workload(_) => self.targets.logs.iter().map(|p| pod("log", p)).collect(),
                Subject::Other => Vec::new(),
            },
            Gated::Shell => match self.subject {
                Subject::Pod => vec![pod("exec", &self.name)],
                Subject::Workload(_) => self.targets.shell.iter().map(|p| pod("exec", p)).collect(),
                Subject::Other => Vec::new(),
            },
            Gated::Write(ResourceAction::Delete) => vec![object("delete")],
            Gated::Write(ResourceAction::Scale) => {
                vec![on(group, plural, Some("scale"), Some(&self.name), "patch")]
            }
            Gated::Write(ResourceAction::Drain) => vec![
                object("patch"),
                AccessCheck {
                    namespace: None,
                    ..on("", "pods", None, None, "list")
                },
            ],
            Gated::Write(
                ResourceAction::RolloutRestart | ResourceAction::Cordon | ResourceAction::Uncordon,
            ) => vec![object("patch")],
        }
    }

    /// Why RBAC forbids `what`, only on a clear refusal: of anything it requires, or for a
    /// workload's logs, which merge its pods, of every pod.
    pub(in crate::ui) fn forbidden(&self, what: Gated) -> Option<String> {
        let checks = self.checks(what);
        let refused = |check: &&AccessCheck| self.answers.get(*check) == Some(&Access::Forbidden);
        let blocker = if what == Gated::Logs && matches!(self.subject, Subject::Workload(_)) {
            checks
                .iter()
                .all(|check| refused(&check))
                .then(|| checks.first())
                .flatten()
        } else {
            checks.iter().find(refused)
        };
        blocker.map(reason)
    }

    /// Takes new answers; whether what is shown changed.
    fn set(&mut self, answers: HashMap<AccessCheck, Access>) -> bool {
        let changed = self.answers != answers;
        self.answers = answers;
        changed
    }
}

impl DetailView {
    /// Keeps the answers to the connection shown: none while no check can run (protected, not
    /// connected), and new ones when it changes. Cheap when nothing did.
    pub(super) fn follow_access(&mut self, cx: &mut Context<Self>) {
        let targets = self.access_targets();
        self.access.targets = targets.clone();
        let followed = (!self.protected
            && self.sessions_blocked.is_none()
            && self.server.is_some())
        .then(|| Followed {
            server: self.server.clone(),
            targets,
        });
        if self.access.followed == followed {
            return;
        }
        // New pods of the same connection keep the answers: they are per name.
        let server = |f: &Option<Followed>| f.as_ref().map(|f| f.server.clone());
        if server(&self.access.followed) != server(&followed) {
            self.access.answers.clear();
        }
        self.access.followed = followed.clone();
        self.access._task = followed.map(|followed| self.ask_access(followed, cx));
    }

    /// The pods of a workload that Logs and Shell would reach, from a list still current.
    fn access_targets(&self) -> Targets {
        match &self.pods {
            Pods::Listed(pods)
                if matches!(self.subject, Subject::Workload(_)) && !self.pods_stale =>
            {
                Targets {
                    logs: if pods.len() <= MAX_LOG_TARGETS {
                        pods.iter().map(|p| p.name.clone()).collect()
                    } else {
                        Vec::new()
                    },
                    shell: self.shell_pod().map(|p| p.pod),
                }
            }
            _ => Targets::default(),
        }
    }

    fn ask_access(&self, followed: Followed, cx: &mut Context<Self>) -> Task<()> {
        let mut wanted: Vec<AccessCheck> = gated_for(&self.object.kind, self.subject)
            .into_iter()
            .flat_map(|what| self.access.checks(what))
            .collect();
        wanted.sort_by(|a, b| {
            (
                &a.namespace,
                &a.group,
                &a.resource,
                &a.subresource,
                &a.name,
                &a.verb,
            )
                .cmp(&(
                    &b.namespace,
                    &b.group,
                    &b.resource,
                    &b.subresource,
                    &b.name,
                    &b.verb,
                ))
        });
        wanted.dedup();
        let core = self.core.clone();
        let cluster = self.object.cluster.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let answers = core
                    .check_access_all(&cluster, followed.server.clone(), wanted.clone())
                    .await
                    .into_iter()
                    .collect();
                let kept = this.update(cx, |view, cx| {
                    if view.access.set(answers) {
                        cx.notify();
                    }
                });
                if kept.is_err() {
                    return;
                }
                cx.background_executor().timer(ACCESS_FRESH_FOR).await;
            }
        })
    }

    /// Sets the answers as a check would, in tests.
    #[cfg(test)]
    pub(in crate::ui) fn set_access_for_tests(
        &mut self,
        what: Gated,
        access: Access,
        cx: &mut Context<Self>,
    ) {
        // Settled on the connection shown, and no running check overwrites what is set here.
        self.follow_access(cx);
        self.access._task = None;
        for check in self.access.checks(what) {
            self.access.answers.insert(check, access);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use flint_core::KindId;
    use flint_core::access::{Access, AccessCheck};

    use super::{Gated, ObjectAccess, ResourceAction, Subject, Targets};

    fn object(group: &str, plural: &str, namespace: Option<&str>, name: &str) -> ObjectAccess {
        let kind = KindId::new(group, plural);
        let subject = Subject::of(&kind);
        ObjectAccess::new(&kind, namespace, name, subject)
    }

    fn deployment() -> ObjectAccess {
        object("apps", "deployments", Some("demo"), "web")
    }

    fn wanted(access: &ObjectAccess, what: Gated) -> Vec<AccessCheck> {
        access.checks(what)
    }

    fn answer(access: &mut ObjectAccess, what: Gated, answer: Access) {
        for check in wanted(access, what) {
            access.answers.insert(check, answer);
        }
    }

    fn on_pods(subresource: &str, verb: &str, name: Option<&str>) -> AccessCheck {
        AccessCheck {
            namespace: Some("demo".into()),
            group: String::new(),
            resource: "pods".into(),
            subresource: Some(subresource.into()),
            name: name.map(str::to_owned),
            verb: verb.into(),
        }
    }

    #[test]
    fn only_a_clear_refusal_disables_an_action() {
        let mut access = deployment();
        let delete = Gated::Write(ResourceAction::Delete);
        assert_eq!(access.forbidden(delete), None, "not asked yet");
        answer(&mut access, delete, Access::Unknown);
        assert_eq!(access.forbidden(delete), None, "the check failed");
        answer(&mut access, delete, Access::Allowed);
        assert_eq!(access.forbidden(delete), None);
        answer(&mut access, delete, Access::Forbidden);
        assert_eq!(
            access.forbidden(delete).as_deref(),
            Some("your access does not allow delete on deployments in demo")
        );
    }

    // A Role with `resourceNames` refuses the unnamed question and allows the object.
    #[test]
    fn an_action_on_an_object_asks_by_its_name() {
        let access = deployment();
        for what in [
            Gated::Edit,
            Gated::Write(ResourceAction::Delete),
            Gated::Write(ResourceAction::Scale),
            Gated::Write(ResourceAction::RolloutRestart),
        ] {
            let asked = wanted(&access, what);
            assert!(
                asked.iter().all(|c| c.name.as_deref() == Some("web")),
                "{what:?}: {asked:?}"
            );
        }
        let pod = object("", "pods", Some("demo"), "web-1");
        for what in [Gated::Logs, Gated::Shell] {
            let asked = wanted(&pod, what);
            assert!(
                !asked.is_empty() && asked.iter().all(|c| c.name.as_deref() == Some("web-1")),
                "{what:?}: {asked:?}"
            );
        }
    }

    #[test]
    fn a_workload_with_unknown_pods_is_not_refused_by_an_unnamed_denial() {
        let mut access = deployment();
        access
            .answers
            .insert(on_pods("log", "get", None), Access::Forbidden);
        access
            .answers
            .insert(on_pods("exec", "get", None), Access::Forbidden);
        access
            .answers
            .insert(on_pods("exec", "create", None), Access::Forbidden);
        assert_eq!(access.forbidden(Gated::Logs), None);
        assert_eq!(access.forbidden(Gated::Shell), None);
    }

    #[test]
    fn a_workloads_logs_are_refused_only_when_every_pod_is() {
        let mut access = deployment();
        access.targets = Targets {
            logs: vec!["web-1".into(), "web-2".into()],
            shell: Some("web-1".into()),
        };
        let refused = on_pods("log", "get", Some("web-1"));
        access.answers.insert(refused, Access::Forbidden);
        assert_eq!(access.forbidden(Gated::Logs), None, "web-2 is not known");
        let other = on_pods("log", "get", Some("web-2"));
        access.answers.insert(other.clone(), Access::Allowed);
        assert_eq!(access.forbidden(Gated::Logs), None, "web-2 is allowed");
        access.answers.insert(other, Access::Forbidden);
        assert_eq!(
            access.forbidden(Gated::Logs).as_deref(),
            Some("your access does not allow get on pods/log in demo")
        );
    }

    #[test]
    fn a_workloads_shell_follows_the_pod_it_opens_in() {
        let mut access = deployment();
        access.targets = Targets {
            logs: vec!["web-1".into(), "web-2".into()],
            shell: Some("web-2".into()),
        };
        access
            .answers
            .insert(on_pods("exec", "get", Some("web-1")), Access::Forbidden);
        assert_eq!(access.forbidden(Gated::Shell), None, "not its target");
        access
            .answers
            .insert(on_pods("exec", "get", Some("web-2")), Access::Forbidden);
        assert!(access.forbidden(Gated::Shell).is_some());
    }

    // Shell always sends a GET upgrade; `create` is required only on some servers.
    #[test]
    fn a_shell_is_refused_by_get_alone() {
        let mut pod = object("", "pods", Some("demo"), "web-1");
        let get = on_pods("exec", "get", Some("web-1"));
        let create = on_pods("exec", "create", Some("web-1"));
        pod.answers.insert(create, Access::Forbidden);
        assert_eq!(pod.forbidden(Gated::Shell), None, "create alone");
        pod.answers.insert(get.clone(), Access::Unknown);
        assert_eq!(pod.forbidden(Gated::Shell), None, "get is not known");
        pod.answers.insert(get, Access::Forbidden);
        assert_eq!(
            pod.forbidden(Gated::Shell).as_deref(),
            Some("your access does not allow get on pods/exec in demo")
        );
    }

    #[test]
    fn a_drain_needs_the_node_patch_and_a_cluster_wide_pod_list() {
        let drain = Gated::Write(ResourceAction::Drain);
        let list = AccessCheck {
            namespace: None,
            group: String::new(),
            resource: "pods".into(),
            subresource: None,
            name: None,
            verb: "list".into(),
        };
        let patch = AccessCheck {
            namespace: None,
            group: String::new(),
            resource: "nodes".into(),
            subresource: None,
            name: Some("node-1".into()),
            verb: "patch".into(),
        };

        let mut access = object("", "nodes", None, "node-1");
        access.answers.insert(patch.clone(), Access::Allowed);
        access.answers.insert(list.clone(), Access::Forbidden);
        assert_eq!(
            access.forbidden(drain).as_deref(),
            Some("your access does not allow list on pods on this cluster")
        );

        access.answers.insert(list.clone(), Access::Unknown);
        assert_eq!(access.forbidden(drain), None, "the list is not known");

        access.answers.insert(list, Access::Allowed);
        access.answers.insert(patch, Access::Forbidden);
        assert_eq!(
            access.forbidden(drain).as_deref(),
            Some("your access does not allow patch on nodes on this cluster")
        );
    }

    #[test]
    fn each_action_asks_for_the_verb_it_sends() {
        let access = deployment();
        let one = |what| wanted(&access, what).remove(0);
        assert_eq!(one(Gated::Edit).verb, "update");
        let scale = one(Gated::Write(ResourceAction::Scale));
        assert_eq!(
            (scale.verb.as_str(), scale.subresource.as_deref()),
            ("patch", Some("scale"))
        );
        assert_eq!(
            one(Gated::Write(ResourceAction::RolloutRestart)).verb,
            "patch"
        );
    }

    #[test]
    fn a_cluster_scoped_reason_names_no_namespace() {
        let mut access = object("", "nodes", None, "node-1");
        let cordon = Gated::Write(ResourceAction::Cordon);
        answer(&mut access, cordon, Access::Forbidden);
        assert_eq!(
            access.forbidden(cordon).as_deref(),
            Some("your access does not allow patch on nodes on this cluster")
        );
    }
}
