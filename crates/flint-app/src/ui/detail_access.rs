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
use super::{DetailView, Subject};

/// An action RBAC can forbid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum Gated {
    Edit,
    Logs,
    Shell,
    Write(ResourceAction),
}

/// The checks that would allow `what`: forbidden only when every one of them is refused.
///
/// A shell takes `create` on `pods/exec`, or `get` on servers that authorize its WebSocket
/// upgrade by the request's verb.
fn checks(kind: &KindId, namespace: Option<&str>, what: Gated) -> Vec<AccessCheck> {
    let on = |group: &str, resource: &str, subresource: Option<&str>, verb: &str| AccessCheck {
        namespace: namespace.map(str::to_owned),
        group: group.to_owned(),
        resource: resource.to_owned(),
        subresource: subresource.map(str::to_owned),
        verb: verb.to_owned(),
    };
    let object = |verb: &str| on(kind.group(), kind.plural(), None, verb);
    match what {
        Gated::Edit => vec![object("update")],
        Gated::Logs => vec![on("", "pods", Some("log"), "get")],
        Gated::Shell => vec![
            on("", "pods", Some("exec"), "create"),
            on("", "pods", Some("exec"), "get"),
        ],
        Gated::Write(ResourceAction::Delete) => vec![object("delete")],
        Gated::Write(ResourceAction::Scale) => {
            vec![on(kind.group(), kind.plural(), Some("scale"), "patch")]
        }
        Gated::Write(
            ResourceAction::RolloutRestart
            | ResourceAction::Cordon
            | ResourceAction::Uncordon
            | ResourceAction::Drain,
        ) => vec![object("patch")],
    }
}

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

/// The connection the answers are for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Followed {
    server: Option<String>,
}

/// The object's checks and their answers.
pub(in crate::ui) struct ObjectAccess {
    kind: KindId,
    namespace: Option<String>,
    answers: HashMap<AccessCheck, Access>,
    followed: Option<Followed>,
    _task: Option<Task<()>>,
}

impl ObjectAccess {
    pub(super) fn new(kind: &KindId, namespace: Option<&str>) -> Self {
        ObjectAccess {
            kind: kind.clone(),
            namespace: namespace.map(str::to_owned),
            answers: HashMap::new(),
            followed: None,
            _task: None,
        }
    }

    /// Why RBAC forbids `what`, only if every way to do it was clearly refused.
    pub(in crate::ui) fn forbidden(&self, what: Gated) -> Option<String> {
        let checks = checks(&self.kind, self.namespace.as_deref(), what);
        let refused = checks
            .iter()
            .all(|check| self.answers.get(check) == Some(&Access::Forbidden));
        refused.then(|| reason(&checks[0]))
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
        let followed = (!self.protected
            && self.sessions_blocked.is_none()
            && self.server.is_some())
        .then(|| Followed {
            server: self.server.clone(),
        });
        if self.access.followed == followed {
            return;
        }
        self.access.followed = followed.clone();
        self.access.answers.clear();
        self.access._task = followed.map(|followed| self.ask_access(followed, cx));
    }

    fn ask_access(&self, followed: Followed, cx: &mut Context<Self>) -> Task<()> {
        let mut wanted: Vec<AccessCheck> = gated_for(&self.object.kind, self.subject)
            .into_iter()
            .flat_map(|what| checks(&self.object.kind, self.object.namespace.as_deref(), what))
            .collect();
        wanted.sort_by(|a, b| {
            (&a.resource, &a.subresource, &a.verb).cmp(&(&b.resource, &b.subresource, &b.verb))
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
        for check in checks(&self.object.kind, self.object.namespace.as_deref(), what) {
            self.access.answers.insert(check, access);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use flint_core::KindId;
    use flint_core::access::Access;

    use super::{Gated, ObjectAccess, ResourceAction, checks};

    fn deployment() -> ObjectAccess {
        ObjectAccess::new(&KindId::new("apps", "deployments"), Some("demo"))
    }

    fn answer(access: &mut ObjectAccess, what: Gated, answer: Access) {
        let wanted = checks(&access.kind, access.namespace.as_deref(), what);
        for check in wanted {
            access.answers.insert(check, answer);
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

    #[test]
    fn a_shell_needs_both_of_its_verbs_refused() {
        let mut access = deployment();
        let [create, get] = &checks(&access.kind, Some("demo"), Gated::Shell)[..] else {
            panic!("two ways to open a shell");
        };
        let (create, get) = (create.clone(), get.clone());
        access.answers.insert(create.clone(), Access::Forbidden);
        assert_eq!(access.forbidden(Gated::Shell), None, "get is not known");
        access.answers.insert(get.clone(), Access::Allowed);
        assert_eq!(access.forbidden(Gated::Shell), None, "get is allowed");
        access.answers.insert(get, Access::Forbidden);
        assert!(access.forbidden(Gated::Shell).is_some());
    }

    #[test]
    fn each_action_asks_for_the_verb_it_sends() {
        let kind = KindId::new("apps", "deployments");
        let one = |what| checks(&kind, Some("demo"), what).remove(0);
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
        let logs = one(Gated::Logs);
        assert_eq!(
            (
                logs.group.as_str(),
                logs.resource.as_str(),
                logs.subresource.as_deref()
            ),
            ("", "pods", Some("log"))
        );
    }

    #[test]
    fn a_cluster_scoped_reason_names_no_namespace() {
        let mut access = ObjectAccess::new(&KindId::new("", "nodes"), None);
        let cordon = Gated::Write(ResourceAction::Cordon);
        answer(&mut access, cordon, Access::Forbidden);
        assert_eq!(
            access.forbidden(cordon).as_deref(),
            Some("your access does not allow patch on nodes on this cluster")
        );
    }
}
