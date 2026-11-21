//! Failures the fake server injects into matching requests.

use std::time::Duration;

use super::fixtures::Gvr;
use super::route::{Route, Verb};

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
}

impl Effect {
    /// Only WebSocket upgrades match it.
    fn upgrades_only(&self) -> bool {
        matches!(self, Effect::RefuseUpgrade | Effect::StripUpgrade)
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

    /// The effect for a request, counting it against the fault's limit. A refused upgrade only
    /// or stripped upgrade only matches (and counts) a WebSocket `upgrade` request.
    pub fn take(&mut self, verb: Verb, route: &Route, upgrade: bool) -> Option<Effect> {
        let fault = self
            .0
            .iter_mut()
            .find(|f| f.matches(verb, route) && (upgrade || !f.effect.upgrades_only()))?;
        if let Some(n) = &mut fault.remaining {
            *n -= 1;
        }
        Some(fault.effect.clone())
    }
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
    // A schema.GroupResource prints as `resource.group`, or `resource` in the core group.
    let qualified = if gvr.group.is_empty() {
        gvr.resource.clone()
    } else {
        format!("{}.{}", gvr.resource, gvr.group)
    };
    let subject = match route.name() {
        Some(name) => format!("{qualified} \"{name}\""),
        None => qualified,
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
            faults.take(Verb::List, &route, false),
            Some(Effect::DropConnection)
        );
        assert_eq!(
            faults.take(Verb::List, &route, false),
            Some(Effect::Forbidden)
        );
        faults.clear();
        assert_eq!(faults.take(Verb::List, &route, false), None);
        // A refused upgrade waits for an upgrade, other requests pass it by.
        faults.add(Fault::refuse_upgrade().times(1));
        assert_eq!(faults.take(Verb::List, &route, false), None);
        assert_eq!(
            faults.take(Verb::Get, &route, true),
            Some(Effect::RefuseUpgrade)
        );
        assert_eq!(faults.take(Verb::Get, &route, true), None);
        faults.add(Fault::strip_upgrade());
        faults.add(Fault::forbidden_as("create"));
        assert_eq!(
            faults.take(Verb::Get, &route, false),
            Some(Effect::ForbiddenAs("create".into()))
        );
        assert_eq!(
            faults.take(Verb::Get, &route, true),
            Some(Effect::StripUpgrade)
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
