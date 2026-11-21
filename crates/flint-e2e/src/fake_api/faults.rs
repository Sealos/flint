//! Failures the fake server injects into matching requests.

use std::time::Duration;

use super::fixtures::Gvr;
use super::route::{Route, Verb};
use super::state::ApiError;

/// What happens to a matching request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// 403 with the `Status` the API server sends when RBAC refuses.
    Forbidden,
    /// 403 naming this RBAC verb instead of the request's: `create` for an exec under
    /// `AuthorizePodWebsocketUpgradeCreatePermission` when the role grants `get` but not
    /// `create` on `pods/exec`.
    ForbiddenAs(String),
    /// Any other error status (500, 429, 404...).
    Status(u16),
    /// The 429 the Eviction API answers when a PodDisruptionBudget does not allow it.
    DisruptionBudget,
    /// The response is sent normally after this delay.
    Delay(Duration),
    /// The connection is closed without a response.
    DropConnection,
    /// A WebSocket upgrade (an exec) is answered with a plain 200 instead of a 101, as a proxy
    /// that does not pass upgrades on answers it. Other requests pass the fault by, and do not
    /// count against [`Fault::times`].
    RefuseUpgrade,
    /// A WebSocket upgrade reaches the API server without its `Upgrade` and `Connection`
    /// headers, as a proxy that strips hop-by-hop headers forwards it: an exec gets the API
    /// server's 400 "Upgrade request required". Like [`Effect::RefuseUpgrade`], only upgrades
    /// match it.
    StripUpgrade,
    /// The 409 a write gets when the object changed since it was read. Only writes match it.
    Conflict,
    /// An admission webhook refuses the write, with the 403 webhooks commonly answer. Only
    /// writes match it.
    AdmissionDenied {
        /// The webhook's name.
        webhook: String,
        /// Why it refused.
        message: String,
    },
}

impl Effect {
    /// Only WebSocket upgrades match it.
    fn upgrades_only(&self) -> bool {
        matches!(self, Effect::RefuseUpgrade | Effect::StripUpgrade)
    }

    /// Only writes match it: reads of the same object pass it by, uncounted.
    fn writes_only(&self) -> bool {
        matches!(self, Effect::Conflict | Effect::AdmissionDenied { .. })
    }
}

/// A fault: an effect, which requests it applies to, and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    effect: Effect,
    resource: Option<Gvr>,
    verbs: Vec<Verb>,
    /// `Some(None)`: only requests across all namespaces; `Some(Some(ns))`: only in `ns`.
    namespace: Option<Option<String>>,
    name: Option<String>,
    subresource: Option<String>,
    remaining: Option<u32>,
}

impl Fault {
    /// A fault that applies to every request until narrowed.
    pub fn new(effect: Effect) -> Self {
        Fault {
            effect,
            resource: None,
            verbs: Vec::new(),
            namespace: None,
            name: None,
            subresource: None,
            remaining: None,
        }
    }

    pub fn forbidden() -> Self {
        Fault::new(Effect::Forbidden)
    }

    /// A 403 that names `verb` ([`Effect::ForbiddenAs`]).
    pub fn forbidden_as(verb: &str) -> Self {
        Fault::new(Effect::ForbiddenAs(verb.to_owned()))
    }

    pub fn status(code: u16) -> Self {
        Fault::new(Effect::Status(code))
    }

    /// A PodDisruptionBudget refuses the eviction ([`Effect::DisruptionBudget`]).
    pub fn disruption_budget() -> Self {
        Fault::new(Effect::DisruptionBudget)
    }

    /// Answers normally after `delay` ([`Effect::Delay`]).
    pub fn delay(delay: Duration) -> Self {
        Fault::new(Effect::Delay(delay))
    }

    pub fn drop_connection() -> Self {
        Fault::new(Effect::DropConnection)
    }

    /// Answers WebSocket upgrades with a plain 200 ([`Effect::RefuseUpgrade`]).
    pub fn refuse_upgrade() -> Self {
        Fault::new(Effect::RefuseUpgrade)
    }

    /// Strips WebSocket upgrades of their headers on the way ([`Effect::StripUpgrade`]).
    pub fn strip_upgrade() -> Self {
        Fault::new(Effect::StripUpgrade)
    }

    /// Answers writes with a 409 ([`Effect::Conflict`]).
    pub fn conflict() -> Self {
        Fault::new(Effect::Conflict)
    }

    /// An admission webhook refuses writes ([`Effect::AdmissionDenied`]).
    pub fn admission_denied(webhook: &str, message: &str) -> Self {
        Fault::new(Effect::AdmissionDenied {
            webhook: webhook.to_owned(),
            message: message.to_owned(),
        })
    }

    /// Only requests about this resource.
    pub fn on(mut self, gvr: &Gvr) -> Self {
        self.resource = Some(gvr.clone());
        self
    }

    /// Only these verbs (may be called more than once).
    pub fn verb(mut self, verb: Verb) -> Self {
        self.verbs.push(verb);
        self
    }

    /// Only requests across all namespaces (or for cluster-scoped resources).
    pub fn cluster_wide(mut self) -> Self {
        self.namespace = Some(None);
        self
    }

    /// Only requests in this namespace.
    pub fn in_namespace(mut self, namespace: &str) -> Self {
        self.namespace = Some(Some(namespace.to_owned()));
        self
    }

    /// Only requests about the object of this name (its subresources included).
    pub fn named(mut self, name: &str) -> Self {
        self.name = Some(name.to_owned());
        self
    }

    /// Only requests for this subresource (`log`, `exec`), such as one pod's log with
    /// [`Fault::named`]. Without it, a fault on a resource also matches its subresources.
    pub fn subresource(mut self, subresource: &str) -> Self {
        self.subresource = Some(subresource.to_owned());
        self
    }

    /// Only the next `n` matching requests.
    pub fn times(mut self, n: u32) -> Self {
        self.remaining = Some(n);
        self
    }

    pub fn effect(&self) -> &Effect {
        &self.effect
    }

    /// The resource the fault is limited to, if any.
    pub fn resource(&self) -> Option<&Gvr> {
        self.resource.as_ref()
    }

    fn matches(&self, verb: Verb, route: &Route) -> bool {
        self.resource
            .as_ref()
            .is_none_or(|r| route.gvr() == Some(r))
            && (self.verbs.is_empty() || self.verbs.contains(&verb))
            && self
                .namespace
                .as_ref()
                .is_none_or(|ns| ns.as_deref() == route.namespace())
            && self.name.as_deref().is_none_or(|n| route.name() == Some(n))
            && self
                .subresource
                .as_deref()
                .is_none_or(|s| route.subresource() == Some(s))
            && self.remaining != Some(0)
    }
}

/// The active faults, first match wins.
#[derive(Debug, Default)]
pub struct Faults(Vec<Fault>);

impl Faults {
    pub fn add(&mut self, fault: Fault) {
        self.0.push(fault);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The effect for a request, counting it against the fault's limit. A refused or stripped
    /// upgrade only matches (and counts) a WebSocket `upgrade` request, and a conflict or an
    /// admission refusal only a `write`.
    pub fn take(
        &mut self,
        verb: Verb,
        route: &Route,
        upgrade: bool,
        write: bool,
    ) -> Option<Effect> {
        let fault = self.0.iter_mut().find(|f| {
            f.matches(verb, route)
                && (upgrade || !f.effect.upgrades_only())
                && (write || !f.effect.writes_only())
        })?;
        if let Some(n) = &mut fault.remaining {
            *n -= 1;
        }
        Some(fault.effect.clone())
    }
}

/// A resource as a `schema.GroupResource` prints: `resource.group`, or `resource` in the core
/// group.
fn qualified(gvr: &Gvr) -> String {
    if gvr.group.is_empty() {
        gvr.resource.clone()
    } else {
        format!("{}.{}", gvr.resource, gvr.group)
    }
}

/// The 409 of `NewConflict` around the optimistic-lock error.
pub fn conflict(route: &Route) -> ApiError {
    let name = route.name().unwrap_or_default();
    let resource = route.gvr().map(qualified).unwrap_or_default();
    let mut error = ApiError::new(
        409,
        "Conflict",
        format!(
            "Operation cannot be fulfilled on {resource} \"{name}\": the object has been \
             modified; please apply your changes to the latest version and try again"
        ),
    );
    if let Some(gvr) = route.gvr() {
        error.body["details"] = serde_json::json!({
            "name": name, "group": gvr.group, "kind": gvr.resource,
        });
    }
    error
}

/// The refusal of an admission webhook, worded as the API server wraps it.
pub fn admission_denied(webhook: &str, message: &str) -> ApiError {
    ApiError::new(
        403,
        "Forbidden",
        format!("admission webhook \"{webhook}\" denied the request: {message}"),
    )
}

/// The message the API server sends when RBAC refuses a request: `NewForbidden` around
/// `forbiddenMessage` (`k8s.io/apiserver/pkg/endpoints/handlers/responsewriters/errors.go`).
pub fn forbidden_message(verb: Verb, route: &Route) -> String {
    let verb = match verb {
        Verb::List => "list",
        Verb::Watch => "watch",
        Verb::Get => "get",
        _ => "get",
    };
    forbidden_message_as(verb, route)
}

/// [`forbidden_message`] naming the RBAC verb `verb`.
pub fn forbidden_message_as(verb: &str, route: &Route) -> String {
    let Some(gvr) = route.gvr() else {
        return format!("forbidden: User \"flint-e2e\" cannot {verb} path");
    };
    let scope = match route.namespace() {
        Some(ns) => format!("in the namespace \"{ns}\""),
        None => "at the cluster scope".to_owned(),
    };
    let subject = match route.name() {
        Some(name) => format!("{} \"{name}\"", qualified(gvr)),
        None => qualified(gvr),
    };
    let resource = match route.subresource() {
        Some(sub) => format!("{}/{sub}", gvr.resource),
        None => gvr.resource.clone(),
    };
    format!(
        "{subject} is forbidden: User \"flint-e2e\" cannot {verb} resource \"{resource}\" in API group \"{}\" {scope}",
        gvr.group
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_match_resource_verb_and_namespace() {
        let pods = Gvr::core("pods");
        let all = Route::parse("/api/v1/pods");
        let demo = Route::parse("/api/v1/namespaces/demo/pods");
        let nodes = Route::parse("/api/v1/nodes");
        let fault = Fault::forbidden().on(&pods).verb(Verb::List).cluster_wide();
        assert!(fault.matches(Verb::List, &all));
        assert!(!fault.matches(Verb::Watch, &all));
        assert!(!fault.matches(Verb::List, &demo));
        assert!(!fault.matches(Verb::List, &nodes));
        let in_demo = Fault::status(500).in_namespace("demo");
        assert!(in_demo.matches(Verb::Get, &demo) && !in_demo.matches(Verb::Get, &all));
        let one_log = Fault::forbidden()
            .on(&pods)
            .named("web-1")
            .subresource("log");
        let log = Route::parse("/api/v1/namespaces/demo/pods/web-1/log");
        assert!(one_log.matches(Verb::Get, &log));
        assert!(!one_log.matches(
            Verb::Get,
            &Route::parse("/api/v1/namespaces/demo/pods/web-1")
        ));
        assert!(!one_log.matches(
            Verb::Get,
            &Route::parse("/api/v1/namespaces/demo/pods/web-2/log")
        ));
        assert!(Fault::status(500).on(&pods).matches(Verb::Get, &log));
        let exec = Route::parse("/api/v1/namespaces/demo/pods/web-1/exec");
        let refuse = Fault::refuse_upgrade().on(&pods).subresource("exec");
        assert!(refuse.matches(Verb::Get, &exec) && !refuse.matches(Verb::Get, &log));
        assert_eq!(
            Fault::delay(Duration::from_secs(1)).effect(),
            &Effect::Delay(Duration::from_secs(1))
        );
    }

    #[test]
    fn counted_faults_run_out_and_the_first_match_wins() {
        let route = Route::parse("/api/v1/pods");
        let mut faults = Faults::default();
        faults.add(Fault::drop_connection().times(1));
        faults.add(Fault::forbidden());
        assert_eq!(
            faults.take(Verb::List, &route, false, false),
            Some(Effect::DropConnection)
        );
        assert_eq!(
            faults.take(Verb::List, &route, false, false),
            Some(Effect::Forbidden)
        );
        faults.clear();
        assert_eq!(faults.take(Verb::List, &route, false, false), None);
        // A refused upgrade waits for an upgrade, other requests pass it by.
        faults.add(Fault::refuse_upgrade().times(1));
        assert_eq!(faults.take(Verb::List, &route, false, false), None);
        assert_eq!(
            faults.take(Verb::Get, &route, true, false),
            Some(Effect::RefuseUpgrade)
        );
        assert_eq!(faults.take(Verb::Get, &route, true, false), None);
        faults.add(Fault::strip_upgrade());
        faults.add(Fault::forbidden_as("create"));
        assert_eq!(
            faults.take(Verb::Get, &route, false, false),
            Some(Effect::ForbiddenAs("create".into()))
        );
        assert_eq!(
            faults.take(Verb::Get, &route, true, false),
            Some(Effect::StripUpgrade)
        );
    }

    #[test]
    fn conflicts_and_admission_refusals_only_match_writes() {
        let route = Route::parse("/apis/apps/v1/namespaces/demo/deployments/web");
        let mut faults = Faults::default();
        faults.add(Fault::conflict().times(1));
        faults.add(Fault::admission_denied("policy.example.com", "no").times(1));
        assert_eq!(faults.take(Verb::Get, &route, false, false), None);
        assert_eq!(
            faults.take(Verb::Other, &route, false, true),
            Some(Effect::Conflict)
        );
        assert!(matches!(
            faults.take(Verb::Other, &route, false, true),
            Some(Effect::AdmissionDenied { .. })
        ));
        assert_eq!(faults.take(Verb::Other, &route, false, true), None);
        let refused = conflict(&route);
        assert_eq!(refused.code, 409);
        assert_eq!(refused.body["reason"], "Conflict");
        assert_eq!(refused.body["details"]["group"], "apps");
        assert!(
            refused.body["message"]
                .as_str()
                .unwrap()
                .starts_with("Operation cannot be fulfilled on deployments.apps \"web\"")
        );
        let denied = admission_denied("policy.example.com", "no");
        assert_eq!(
            (denied.code, &denied.body["reason"]),
            (403, &"Forbidden".into())
        );
        assert_eq!(
            denied.body["message"],
            "admission webhook \"policy.example.com\" denied the request: no"
        );
    }

    #[test]
    fn forbidden_messages_name_the_scope() {
        let msg = forbidden_message(Verb::List, &Route::parse("/apis/apps/v1/deployments"));
        assert_eq!(
            msg,
            "deployments.apps is forbidden: User \"flint-e2e\" cannot list resource \"deployments\" \
             in API group \"apps\" at the cluster scope"
        );
        let msg = forbidden_message(
            Verb::Get,
            &Route::parse("/api/v1/namespaces/demo/pods/web-1/log"),
        );
        assert_eq!(
            msg,
            "pods \"web-1\" is forbidden: User \"flint-e2e\" cannot get resource \"pods/log\" \
             in API group \"\" in the namespace \"demo\""
        );
        // Recorded on kind: a service account with no rights is refused `get` on `pods/exec`,
        // which a WebSocket exec needs. Under `AuthorizePodWebsocketUpgradeCreatePermission` it
        // needs `create` too, and a role with only `get` is refused that.
        let msg = forbidden_message(
            Verb::Get,
            &Route::parse("/api/v1/namespaces/demo/pods/shell/exec"),
        );
        assert_eq!(
            msg,
            "pods \"shell\" is forbidden: User \"flint-e2e\" cannot get resource \"pods/exec\" \
             in API group \"\" in the namespace \"demo\""
        );
        let msg = forbidden_message_as(
            "create",
            &Route::parse("/api/v1/namespaces/demo/pods/shell/exec"),
        );
        assert!(
            msg.contains("cannot create resource \"pods/exec\""),
            "{msg}"
        );
        let msg = forbidden_message(Verb::Watch, &Route::parse("/api/v1/namespaces/demo/pods"));
        assert!(msg.ends_with("in the namespace \"demo\""));
        assert!(forbidden_message(Verb::Version, &Route::Version).starts_with("forbidden"));
    }
}
