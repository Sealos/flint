//! `SelfSubjectAccessReview`s on the fake server: what an identity limited by RBAC is told. The
//! policy answers these reviews only, and a review is not recorded as a write.

// module-tests: ../../tests/access_checks.rs

use http::Method;
use serde_json::{Value, json};

use super::fixtures::Gvr;
use super::route::Route;
use super::state::ApiError;

/// What reviews answer. By default everything is allowed.
#[derive(Debug, Clone, Default)]
pub struct AccessPolicy {
    /// The only namespace allowed; cluster-scoped and other namespaces are refused.
    only_namespace: Option<String>,
    /// `(verb, resource or resource/subresource)` refused everywhere.
    denied: Vec<(String, String)>,
    /// `(verb, resource, name)`: the verb on that resource is allowed for this name only, as a
    /// Role with `resourceNames` does.
    named: Vec<(String, String, String)>,
}

impl AccessPolicy {
    /// An identity that can only act in `namespace`.
    pub fn only_namespace(namespace: &str) -> Self {
        AccessPolicy {
            only_namespace: Some(namespace.to_owned()),
            ..AccessPolicy::default()
        }
    }

    /// Also refuses `verb` on `resource` (`pods/exec` for a subresource).
    pub fn deny(mut self, verb: &str, resource: &str) -> Self {
        self.denied.push((verb.to_owned(), resource.to_owned()));
        self
    }

    /// Allows `verb` on `resource` for the object `name` only.
    pub fn only_named(mut self, verb: &str, resource: &str, name: &str) -> Self {
        self.named
            .push((verb.to_owned(), resource.to_owned(), name.to_owned()));
        self
    }

    fn allows(&self, attributes: &Value) -> bool {
        let text = |key: &str| attributes.get(key).and_then(Value::as_str).unwrap_or("");
        if let Some(only) = &self.only_namespace
            && text("namespace") != only
        {
            return false;
        }
        let resource = match text("subresource") {
            "" => text("resource").to_owned(),
            sub => format!("{}/{sub}", text("resource")),
        };
        let other_name = self.named.iter().any(|(verb, limited, name)| {
            verb == text("verb") && *limited == resource && name != text("name")
        });
        !other_name
            && !self
                .denied
                .iter()
                .any(|(verb, denied)| verb == text("verb") && *denied == resource)
    }
}

/// Whether this request creates a `SelfSubjectAccessReview`.
pub(super) fn is_review(method: &Method, route: &Route) -> bool {
    method == Method::POST
        && matches!(
            route,
            Route::Collection { gvr, namespace: None }
                if *gvr == Gvr::new("authorization.k8s.io", "v1", "selfsubjectaccessreviews")
        )
}

/// The review `body` asks for, with its status.
pub(super) fn answer(policy: &AccessPolicy, body: &[u8]) -> Result<Value, ApiError> {
    let mut review: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::new(400, "BadRequest", e.to_string()))?;
    let attributes = review
        .pointer("/spec/resourceAttributes")
        .cloned()
        .ok_or_else(|| ApiError::new(400, "BadRequest", "no resourceAttributes"))?;
    // RBAC's refusal: `allowed: false`, with `denied` left out.
    review["status"] = json!({"allowed": policy.allows(&attributes)});
    Ok(review)
}
