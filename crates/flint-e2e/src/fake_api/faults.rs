//! Failures the fake server injects into matching requests.

use std::time::Duration;

use super::fixtures::Gvr;
use super::route::{Route, Verb};

/// What happens to a matching request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// 403 with the `Status` the API server sends when RBAC refuses.
    Forbidden,
    /// Any other error status (500, 429, 404...).
    Status(u16),
    /// The response is sent normally after this delay.
    Delay(Duration),
    /// The connection is closed without a response.
    DropConnection,
}

/// A fault: an effect, which requests it applies to, and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    effect: Effect,
    resource: Option<Gvr>,
    verbs: Vec<Verb>,
    /// `Some(None)`: only requests across all namespaces; `Some(Some(ns))`: only in `ns`.
    namespace: Option<Option<String>>,
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
            remaining: None,
        }
    }

    pub fn forbidden() -> Self {
        Fault::new(Effect::Forbidden)
    }

    pub fn status(code: u16) -> Self {
        Fault::new(Effect::Status(code))
    }

    /// Answers normally after `delay` ([`Effect::Delay`]).
    pub fn delay(delay: Duration) -> Self {
        Fault::new(Effect::Delay(delay))
    }

    pub fn drop_connection() -> Self {
        Fault::new(Effect::DropConnection)
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

    /// The effect for a request, counting it against the fault's limit.
    pub fn take(&mut self, verb: Verb, route: &Route) -> Option<Effect> {
        let fault = self.0.iter_mut().find(|f| f.matches(verb, route))?;
        if let Some(n) = &mut fault.remaining {
            *n -= 1;
        }
        Some(fault.effect.clone())
    }
}

/// The message the API server sends when RBAC refuses a request.
pub fn forbidden_message(verb: Verb, route: &Route) -> String {
    let verb = match verb {
        Verb::List => "list",
        Verb::Watch => "watch",
        Verb::Get => "get",
        _ => "get",
    };
    let Some(gvr) = route.gvr() else {
        return format!("forbidden: User \"flint-e2e\" cannot {verb} path");
    };
    let scope = match route.namespace() {
        Some(ns) => format!("in the namespace \"{ns}\""),
        None => "at the cluster scope".to_owned(),
    };
    format!(
        "{} is forbidden: User \"flint-e2e\" cannot {verb} resource \"{}\" in API group \"{}\" {scope}",
        gvr.resource, gvr.resource, gvr.group
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
            faults.take(Verb::List, &route),
            Some(Effect::DropConnection)
        );
        assert_eq!(faults.take(Verb::List, &route), Some(Effect::Forbidden));
        faults.clear();
        assert_eq!(faults.take(Verb::List, &route), None);
    }

    #[test]
    fn forbidden_messages_name_the_scope() {
        let msg = forbidden_message(Verb::List, &Route::parse("/apis/apps/v1/deployments"));
        assert_eq!(
            msg,
            "deployments is forbidden: User \"flint-e2e\" cannot list resource \"deployments\" \
             in API group \"apps\" at the cluster scope"
        );
        let msg = forbidden_message(Verb::Watch, &Route::parse("/api/v1/namespaces/demo/pods"));
        assert!(msg.ends_with("in the namespace \"demo\""));
        assert!(forbidden_message(Verb::Version, &Route::Version).starts_with("forbidden"));
    }
}
