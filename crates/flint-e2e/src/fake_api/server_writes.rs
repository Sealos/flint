//! The writes the resource actions send: `PATCH` and `DELETE` of an object, and a pod's
//! `eviction`. Each checks what the API server checks, and tells watches.

// module-tests: ../../tests/write_actions.rs

use serde_json::json;

use super::*;

/// Whether this request is one of these writes.
pub(super) fn handles(method: &Method, route: &Route) -> bool {
    match route {
        Route::Object { .. } => method == Method::PATCH || method == Method::DELETE,
        Route::Subresource {
            gvr, subresource, ..
        } => {
            (method == Method::POST && *gvr == Gvr::core("pods") && subresource == "eviction")
                || (method == Method::PATCH && subresource == "scale")
        }
        _ => false,
    }
}

pub(super) async fn serve(
    inner: &Arc<Inner>,
    req: Request<Incoming>,
    route: &Route,
) -> Result<Response<Body>, ApiError> {
    let (gvr, namespace, name) = match route {
        Route::Object {
            gvr,
            namespace,
            name,
        }
        | Route::Subresource {
            gvr,
            namespace,
            name,
            ..
        } => (gvr, namespace.as_deref(), name.as_str()),
        _ => return Err(ApiError::not_found("not an object")),
    };
    known(inner, gvr, namespace)?;
    let method = req.method().clone();
    let content_type = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_err(|e| ApiError::new(400, "BadRequest", e.to_string()))?
        .to_bytes();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::new(400, "BadRequest", e.to_string()))?
    };
    if method == Method::PATCH {
        if matches!(route, Route::Subresource { .. }) {
            return patch_scale(inner, gvr, namespace, name, &body);
        }
        return patch(inner, gvr, namespace, name, &content_type, &body);
    }
    let (options, code) = if method == Method::POST {
        (body.get("deleteOptions").unwrap_or(&Value::Null), 201)
    } else {
        (&body, 200)
    };
    remove(inner, gvr, namespace, name, options)?;
    Ok(json_with(
        code,
        "application/json",
        &json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Success"}),
    ))
}

/// The answer to a JSON Patch whose `test` failed: Invalid, with no field named.
fn failed_test() -> ApiError {
    let mut error = ApiError::new(
        422,
        "Invalid",
        "the server rejected our request due to an error in our request",
    );
    error.body["details"] = json!({});
    error
}

fn patch(
    inner: &Arc<Inner>,
    gvr: &Gvr,
    namespace: Option<&str>,
    name: &str,
    content_type: &str,
    body: &Value,
) -> Result<Response<Body>, ApiError> {
    let mut state = lock(&inner.state);
    let mut object = state.object(gvr, namespace, name)?;
    if content_type.starts_with("application/json-patch+json") {
        let operations = body
            .as_array()
            .ok_or_else(|| ApiError::new(400, "BadRequest", "not a JSON Patch"))?;
        apply_json_patch(&mut object, operations)?;
    } else {
        let uid = body.pointer("/metadata/uid");
        if uid.is_some() && uid != object.pointer("/metadata/uid") {
            let mut error = ApiError::new(422, "Invalid", "metadata.uid: field is immutable");
            error.body["details"] = json!({"causes": [{
                "reason": "FieldValueInvalid",
                "message": "field is immutable",
                "field": "metadata.uid",
            }]});
            return Err(error);
        }
        merge(&mut object, body);
    }
    let version = state.bump();
    object["metadata"]["resourceVersion"] = version.to_string().into();
    let row = state
        .row_for_object(gvr, &mut object, &[])
        .map_err(|e| ApiError::new(400, "BadRequest", e))?;
    let change = state.apply_object(gvr, "MODIFIED", object.clone(), row);
    tell_watches(inner, gvr, change);
    drop(state);
    Ok(rebased(inner, object))
}

/// A JSON Patch of `test`, `replace` and `add`.
fn apply_json_patch(object: &mut Value, operations: &[Value]) -> Result<(), ApiError> {
    for operation in operations {
        let path = operation["path"].as_str().unwrap_or_default();
        match operation["op"].as_str() {
            Some("test") if object.pointer(path) == Some(&operation["value"]) => {}
            Some("test") => return Err(failed_test()),
            Some("replace") => {
                *object.pointer_mut(path).ok_or_else(failed_test)? = operation["value"].clone();
            }
            Some("add") => {
                let (parent, key) = path.rsplit_once('/').ok_or_else(failed_test)?;
                let parent = object.pointer_mut(parent).and_then(Value::as_object_mut);
                parent
                    .ok_or_else(failed_test)?
                    .insert(key.to_owned(), operation["value"].clone());
            }
            _ => return Err(ApiError::new(400, "BadRequest", "unsupported operation")),
        }
    }
    Ok(())
}

/// A JSON Patch of the `scale` subresource. As on the API server, a `Scale` omits `replicas` at
/// zero.
fn patch_scale(
    inner: &Arc<Inner>,
    gvr: &Gvr,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
) -> Result<Response<Body>, ApiError> {
    let operations = body
        .as_array()
        .ok_or_else(|| ApiError::new(400, "BadRequest", "not a JSON Patch"))?;
    let mut state = lock(&inner.state);
    let mut object = state.object(gvr, namespace, name)?;
    let replicas = object.pointer("/spec/replicas").and_then(Value::as_u64);
    let spec = match replicas {
        Some(count) if count > 0 => json!({ "replicas": count }),
        _ => json!({}),
    };
    let mut scale = json!({
        "apiVersion": "autoscaling/v1",
        "kind": "Scale",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "uid": object.pointer("/metadata/uid"),
            "resourceVersion": object.pointer("/metadata/resourceVersion"),
        },
        "spec": spec,
    });
    apply_json_patch(&mut scale, operations)?;
    let to = scale.pointer("/spec/replicas").cloned().unwrap_or(json!(0));
    object["spec"]["replicas"] = to;
    let version = state.bump();
    object["metadata"]["resourceVersion"] = version.to_string().into();
    scale["metadata"]["resourceVersion"] = version.to_string().into();
    let row = state
        .row_for_object(gvr, &mut object, &[])
        .map_err(|e| ApiError::new(400, "BadRequest", e))?;
    let change = state.apply_object(gvr, "MODIFIED", object, row);
    tell_watches(inner, gvr, change);
    drop(state);
    Ok(rebased(inner, scale))
}

/// A JSON Merge Patch.
fn merge(into: &mut Value, patch: &Value) {
    let Value::Object(fields) = patch else {
        *into = patch.clone();
        return;
    };
    if !into.is_object() {
        *into = json!({});
    }
    let map = into.as_object_mut().expect("an object");
    for (key, value) in fields {
        if value.is_null() {
            map.remove(key);
        } else {
            merge(map.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// Removes the object, if `options` name its uid or none.
fn remove(
    inner: &Arc<Inner>,
    gvr: &Gvr,
    namespace: Option<&str>,
    name: &str,
    options: &Value,
) -> Result<(), ApiError> {
    let mut state = lock(&inner.state);
    let object = state.object(gvr, namespace, name)?;
    let uid = options.pointer("/preconditions/uid");
    if uid.is_some() && uid != object.pointer("/metadata/uid") {
        return Err(ApiError::new(
            409,
            "Conflict",
            format!("the uid of \"{name}\" does not match the precondition"),
        ));
    }
    let row = state
        .row(gvr, namespace, name)
        .ok_or_else(|| ApiError::not_found(format!("\"{name}\" not found")))?;
    let change = state.change(gvr, "DELETED", row);
    tell_watches(inner, gvr, change);
    Ok(())
}

fn tell_watches(inner: &Inner, gvr: &Gvr, change: Change) {
    let outgoing = Outgoing::Event(Box::new(change));
    lock(&inner.watchers)
        .entry(gvr.clone())
        .or_default()
        .retain(|w| w.tx.send(outgoing.clone()).is_ok());
}
