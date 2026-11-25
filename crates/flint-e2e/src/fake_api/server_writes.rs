//! The writes the fake server applies: `POST` to a collection (with
//! [`Options::accept_creates`]), `PUT`, `PATCH` and `DELETE` of an object, a pod's `eviction`
//! and a `PATCH` of `scale`. Each checks what the API server checks and tells watches; with
//! `dryRun=All` it checks and answers, and stores nothing.

// module-tests: ../../tests/write_actions.rs

use serde_json::json;

use super::*;
use crate::record::normalize::stable_uid;

/// A write with its body read.
pub(super) struct WriteRequest {
    pub(super) method: Method,
    pub(super) content_type: String,
    pub(super) body: Bytes,
}

/// Whether a request is a write to record: a mutating method, and not a stream being opened.
pub(super) fn is_write(method: &Method, route: &Route, upgrade: bool) -> bool {
    let mutating = [Method::POST, Method::PUT, Method::PATCH, Method::DELETE].contains(method);
    mutating && !upgrade && route.subresource() != Some("exec")
}

/// Reads a write's body.
pub(super) async fn read(req: Request<Incoming>) -> Result<WriteRequest, ApiError> {
    let method = req.method().clone();
    let content_type = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = req
        .into_body()
        .collect()
        .await
        .map_err(|e| ApiError::new(400, "BadRequest", e.to_string()))?
        .to_bytes();
    Ok(WriteRequest {
        method,
        content_type,
        body,
    })
}

/// The `DeleteOptions` a delete or an eviction carries in its body.
pub(super) fn delete_options<'a>(
    method: &Method,
    route: &Route,
    body: &'a Value,
) -> Option<&'a Value> {
    match route {
        Route::Object { .. } if method == Method::DELETE => Some(body),
        Route::Subresource {
            gvr, subresource, ..
        } if method == Method::POST && *gvr == Gvr::core("pods") && subresource == "eviction" => {
            body.get("deleteOptions")
        }
        _ => None,
    }
}

/// Whether `DeleteOptions` ask for a dry run: `"dryRun": ["All"]`.
pub(super) fn asks_dry_run(options: &Value) -> bool {
    options
        .get("dryRun")
        .and_then(Value::as_array)
        .is_some_and(|stages| stages.iter().any(|stage| stage == "All"))
}

/// Whether this server applies this write.
pub(super) fn handles(inner: &Inner, method: &Method, route: &Route) -> bool {
    match route {
        Route::Collection { .. } => inner.options.accept_creates && method == Method::POST,
        Route::Object { .. } => {
            method == Method::PATCH || method == Method::DELETE || method == Method::PUT
        }
        Route::Subresource {
            gvr, subresource, ..
        } => {
            (method == Method::POST && *gvr == Gvr::core("pods") && subresource == "eviction")
                || (method == Method::PATCH && subresource == "scale")
        }
        _ => false,
    }
}

pub(super) fn serve(
    inner: &Arc<Inner>,
    write: &WriteRequest,
    route: &Route,
    query: &Query,
) -> Result<Response<Body>, ApiError> {
    if let Route::Collection { gvr, namespace } = route {
        return create(inner, gvr, namespace.as_deref(), &write.body, query.dry_run);
    }
    if write.method == Method::PUT {
        return put_object(inner, write, route, query);
    }
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
    let body: Value = if write.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&write.body)
            .map_err(|e| ApiError::new(400, "BadRequest", e.to_string()))?
    };
    let dry_run = query.dry_run;
    if write.method == Method::PATCH {
        if matches!(route, Route::Subresource { .. }) {
            return patch_scale(inner, gvr, namespace, name, &body, dry_run);
        }
        let content_type = write.content_type.as_str();
        return patch(inner, gvr, namespace, name, content_type, &body, dry_run);
    }
    let options = delete_options(&write.method, route, &body).unwrap_or(&Value::Null);
    let code = if write.method == Method::POST {
        201
    } else {
        200
    };
    let dry_run = dry_run || asks_dry_run(options);
    remove(inner, gvr, namespace, name, options, dry_run)?;
    Ok(json_with(
        code,
        "application/json",
        &json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Success"}),
    ))
}

/// Creates an object from a `POST`: the name is the body's or made from `generateName`, the
/// namespace the path's, and the metadata only the API server sets is its own.
fn create(
    inner: &Arc<Inner>,
    gvr: &Gvr,
    namespace: Option<&str>,
    body: &[u8],
    dry_run: bool,
) -> Result<Response<Body>, ApiError> {
    let resource = known(inner, gvr, namespace)?;
    if resource.namespaced && namespace.is_none() {
        return Err(ApiError::new(
            405,
            "MethodNotAllowed",
            format!("{} are created in a namespace", gvr.resource),
        ));
    }
    if !resource.supports("create") {
        return Err(ApiError::new(
            405,
            "MethodNotAllowed",
            format!("create is not supported on {}", gvr.resource),
        ));
    }
    let mut object: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::new(400, "BadRequest", e.to_string()))?;
    let sent_namespace = object
        .pointer("/metadata/namespace")
        .and_then(Value::as_str);
    if sent_namespace.is_some_and(|sent| Some(sent) != namespace) {
        return Err(ApiError::new(
            400,
            "BadRequest",
            "the namespace does not match",
        ));
    }
    let mut state = lock(&inner.state);
    let text = |field: &str| {
        object
            .pointer(&format!("/metadata/{field}"))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let name = match (text("name"), text("generateName")) {
        (Some(name), _) => name,
        (None, Some(prefix)) => format!("{prefix}{:05x}", state.resource_version() + 1),
        (None, None) => {
            return Err(ApiError::new(
                422,
                "Invalid",
                "metadata.name: Required value: name or generateName is required",
            ));
        }
    };
    if state.row(gvr, namespace, &name).is_some() {
        return Err(ApiError::new(
            409,
            "AlreadyExists",
            format!("{} \"{name}\" already exists", gvr.resource),
        ));
    }
    let identity = format!(
        "fake-create/{gvr}/{}/{name}/{}",
        namespace.unwrap_or_default(),
        state.resource_version()
    );
    let has_spec = object.get("spec").is_some();
    let Some(meta) = object.get_mut("metadata").and_then(Value::as_object_mut) else {
        return Err(ApiError::new(
            400,
            "BadRequest",
            "the object has no metadata",
        ));
    };
    for field in SERVER_METADATA {
        meta.remove(field);
    }
    meta.insert("name".into(), name.into());
    meta.insert("uid".into(), stable_uid(&identity).into());
    meta.insert("creationTimestamp".into(), inner.clock.fixture_now().into());
    if has_spec {
        meta.insert("generation".into(), 1.into());
    }
    if let Some(namespace) = namespace {
        meta.insert("namespace".into(), namespace.into());
    }
    let stored = if dry_run {
        state
            .row_for_object(gvr, &mut object, &[])
            .map_err(|e| ApiError::new(400, "BadRequest", e))?;
        object
    } else {
        let row = state
            .row_for_object(gvr, &mut object, &[])
            .map_err(|e| ApiError::new(400, "BadRequest", e))?;
        let change = state.apply_object(gvr, "ADDED", object, row);
        let stored = change.object.clone();
        tell_watches(inner, gvr, change);
        stored
    };
    drop(state);
    let mut stored = stored;
    inner.clock.rebase(&mut stored);
    Ok(json_with(201, "application/json", &stored))
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
    dry_run: bool,
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
    // Checked first, so a dry run refuses what the write would, and a refusal moves no version.
    state
        .row_for_object(gvr, &mut object, &[])
        .map_err(|e| ApiError::new(400, "BadRequest", e))?;
    if dry_run {
        drop(state);
        return Ok(rebased(inner, object));
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
    dry_run: bool,
) -> Result<Response<Body>, ApiError> {
    let operations = body
        .as_array()
        .ok_or_else(|| ApiError::new(400, "BadRequest", "not a JSON Patch"))?;
    let mut state = lock(&inner.state);
    let mut object = state.object(gvr, namespace, name)?;
    let pointer = super::variants::replicas_pointer(&inner.options.scaled, gvr);
    let replicas = object.pointer(&pointer).and_then(Value::as_u64);
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
    if dry_run {
        drop(state);
        return Ok(rebased(inner, scale));
    }
    let to = scale.pointer("/spec/replicas").cloned().unwrap_or(json!(0));
    match object.pointer_mut(&pointer) {
        Some(count) => *count = to,
        None => return Err(ApiError::new(422, "Invalid", "no replica count to scale")),
    }
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

/// Removes the object, if `options` name its uid or none; a dry run only checks.
fn remove(
    inner: &Arc<Inner>,
    gvr: &Gvr,
    namespace: Option<&str>,
    name: &str,
    options: &Value,
    dry_run: bool,
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
    let version = options.pointer("/preconditions/resourceVersion");
    if version.is_some() && version != object.pointer("/metadata/resourceVersion") {
        return Err(ApiError::new(
            409,
            "Conflict",
            format!("the resourceVersion of \"{name}\" does not match the precondition"),
        ));
    }
    if dry_run {
        return Ok(());
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
