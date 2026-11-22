//! UP-14 and UP-15 at the MCP boundary: values planted where an agent may not read them are
//! searched for in every tool's reply and in every file the server leaves on disk.

use anyhow::Result;
use flint_core::activity;
use serde_json::{Value, json};

use crate::activity::files_below;
use crate::support::{EXPOSED, HIDDEN, NAMESPACE, POD, Setup};

const OTHER_NAMESPACE: &str = "kube-system";
const THIRD_NAMESPACE: &str = "placeholder-third";
/// Held only by objects outside the scope, or on the hidden cluster.
const OUTSIDE: &str = "placeholder-outside-scope";
/// In an event the scope allows: proves the sweep reads real data.
const INSIDE: &str = "placeholder-inside-scope";
const ENCODED_VALUE: &str = "cGxhY2Vob2xkZXI=";
const STRING_VALUE: &str = "placeholder-string-data";
const APPLIED_VALUE: &str = "placeholder-last-applied";

fn pod(namespace: &str, name: &str, note: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "annotations": {"example.io/note": note},
        },
        "spec": {"containers": [{"name": "web", "image": "example.io/app:1.0"}]},
    })
}

fn event(namespace: &str, name: &str, about: (&str, &str), message: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Event",
        "metadata": {"name": name, "namespace": namespace},
        "involvedObject": {"kind": about.0, "name": about.1, "namespace": namespace},
        "reason": "Failed",
        "message": message,
        "type": "Warning",
        "count": 1,
    })
}

/// Plants [`OUTSIDE`] in another namespace, in a kind beyond a `pods` scope, in an event about
/// such a kind, and on the hidden cluster; and [`INSIDE`] in an event the scope allows.
fn plant(setup: &Setup) {
    let api = &setup.api;
    let (pods, events) = (api.gvr("pods"), api.gvr("events"));
    api.set_object(&pods, pod(OTHER_NAMESPACE, "outside", OUTSIDE));
    api.set_log(OTHER_NAMESPACE, "outside", &format!("{OUTSIDE}\n"));
    api.set_object(
        &events,
        event(OTHER_NAMESPACE, "outside", ("Pod", "outside"), OUTSIDE),
    );
    api.set_object(
        &events,
        event(NAMESPACE, "about-secret", ("Secret", "held"), OUTSIDE),
    );
    api.set_object(&events, event(NAMESPACE, "about-pod", ("Pod", POD), INSIDE));
    api.set_object(
        &api.gvr("configmaps"),
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "outside", "namespace": NAMESPACE},
            "data": {"note": OUTSIDE},
        }),
    );
    let hidden = &setup.hidden;
    hidden.set_object(&hidden.gvr("pods"), pod(NAMESPACE, "hidden", OUTSIDE));
}

/// One call of every tool, for what is in scope and for what is not.
fn sweep() -> Vec<(&'static str, Value)> {
    let cluster = json!({"cluster": EXPOSED});
    let list = |kind: &str, namespace: &str| json!({"cluster": EXPOSED, "kind": kind, "namespace": namespace});
    let get = |kind: &str, namespace: &str, name: &str| json!({"cluster": EXPOSED, "kind": kind, "namespace": namespace, "name": name});
    vec![
        ("list_clusters", json!({})),
        ("list_resource_kinds", cluster.clone()),
        (
            "list_resources",
            json!({"cluster": EXPOSED, "kind": "pods"}),
        ),
        ("list_resources", list("pods", NAMESPACE)),
        ("list_resources", list("pods", OTHER_NAMESPACE)),
        ("list_resources", list("configmaps", NAMESPACE)),
        ("list_resources", list("events", NAMESPACE)),
        ("get_resource", get("pods", NAMESPACE, POD)),
        ("get_resource", get("pods", OTHER_NAMESPACE, "outside")),
        ("get_resource", get("configmaps", NAMESPACE, "outside")),
        ("get_resource", get("events", NAMESPACE, "about-secret")),
        (
            "get_pod_logs",
            json!({"cluster": EXPOSED, "namespace": OTHER_NAMESPACE, "pod": "outside"}),
        ),
        ("get_events", cluster),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE}),
        ),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": OTHER_NAMESPACE}),
        ),
        (
            "get_events",
            json!({"cluster": EXPOSED, "object_kind": "Secret", "object_name": "held"}),
        ),
        (
            "list_resources",
            json!({"cluster": HIDDEN, "kind": "pods", "namespace": NAMESPACE}),
        ),
        (
            "get_resource",
            json!({"cluster": HIDDEN, "kind": "pods", "namespace": NAMESPACE, "name": "hidden"}),
        ),
    ]
}

fn assert_absent_on_disk(setup: &Setup, values: &[&str]) -> Result<()> {
    for (path, bytes) in files_below(&setup.scratch.config_dir())? {
        let text = String::from_utf8_lossy(&bytes);
        for value in values {
            assert!(
                !text.contains(value),
                "{value} on disk in {}",
                path.display()
            );
        }
    }
    Ok(())
}

// covers UP-14: under a scope, no tool returns anything held beyond it or on a hidden cluster,
// and the activity log of those calls holds none of it either.
#[test]
fn up_14_nothing_out_of_scope_crosses_any_tool() -> Result<()> {
    let setup = Setup::exposed()?;
    plant(&setup);
    let mut mcp = setup.mcp()?;

    // With no scope the planted values are readable, so their absence below means something.
    let controls = [
        (
            "get_resource",
            json!({"cluster": EXPOSED, "kind": "configmaps", "namespace": NAMESPACE, "name": "outside"}),
        ),
        (
            "get_pod_logs",
            json!({"cluster": EXPOSED, "namespace": OTHER_NAMESPACE, "pod": "outside"}),
        ),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE, "object_kind": "Secret"}),
        ),
    ];
    for (tool, args) in &controls {
        let got = mcp.call(tool, args.clone())?;
        assert!(got.text.contains(OUTSIDE), "{tool} {args}: {}", got.text);
    }
    setup.expose(&[NAMESPACE], &["pods"])?;

    let calls = sweep();
    let mut replies = String::new();
    for (tool, args) in &calls {
        let got = mcp.call(tool, args.clone())?;
        assert!(!got.text.contains(OUTSIDE), "{tool} {args}: {}", got.text);
        if args["cluster"] != HIDDEN {
            assert!(!got.text.contains(HIDDEN), "{tool} {args}: {}", got.text);
            assert!(
                !got.text.contains(&setup.hidden.url()),
                "{tool} {args}: {}",
                got.text
            );
        }
        replies.push_str(&got.text);
    }
    assert!(replies.contains(POD), "the sweep read the pod in scope");
    assert!(replies.contains(INSIDE), "the sweep read the allowed event");

    assert_eq!(
        activity::read(&setup.scratch.config_dir()).len(),
        controls.len() + calls.len()
    );
    drop(mcp);
    assert_absent_on_disk(&setup, &[OUTSIDE, INSIDE])?;
    assert!(setup.hidden.arrivals().is_empty());
    Ok(())
}

// covers UP-14: a scope of two namespaces lists both, and no third, for objects and events.
#[test]
fn up_14_a_scope_of_two_namespaces_reads_both_and_no_other() -> Result<()> {
    let setup = Setup::exposed()?;
    let (pods, events) = (setup.api.gvr("pods"), setup.api.gvr("events"));
    for namespace in [OTHER_NAMESPACE, THIRD_NAMESPACE] {
        let name = format!("in-{namespace}");
        setup.api.set_object(&pods, pod(namespace, &name, "note"));
        setup.api.set_object(
            &events,
            event(namespace, &name, ("Pod", &name), &format!("about {name}")),
        );
    }
    setup.expose(&[NAMESPACE, OTHER_NAMESPACE], &[])?;
    let mut mcp = setup.mcp()?;

    let listed = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "pods"}),
    )?;
    let mut namespaces: Vec<&str> = listed["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["namespace"].as_str())
        .collect();
    namespaces.sort_unstable();
    namespaces.dedup();
    assert_eq!(namespaces, [NAMESPACE, OTHER_NAMESPACE], "{listed}");

    let got = mcp.call_json("get_events", json!({"cluster": EXPOSED}))?;
    let text = got.to_string();
    assert!(
        text.contains(&format!("about in-{OTHER_NAMESPACE}")),
        "{got}"
    );
    assert!(!text.contains(THIRD_NAMESPACE), "{got}");
    Ok(())
}

// covers UP-14: narrowing a scope, then withdrawing the cluster, takes effect on the next call
// of a server that has already read the data.
#[test]
fn up_14_narrowing_applies_to_a_running_server() -> Result<()> {
    let setup = Setup::exposed()?;
    plant(&setup);
    let mut mcp = setup.mcp()?;
    let args = json!({
        "cluster": EXPOSED,
        "kind": "pods",
        "namespace": OTHER_NAMESPACE,
        "name": "outside",
    });
    let open = mcp.call("get_resource", args.clone())?;
    assert!(!open.is_error, "{}", open.text);
    assert!(open.text.contains(OUTSIDE), "{}", open.text);

    setup.expose(&[NAMESPACE], &[])?;
    let narrowed = mcp.call("get_resource", args.clone())?;
    assert!(narrowed.is_error, "{}", narrowed.text);
    assert!(!narrowed.text.contains(OUTSIDE), "{}", narrowed.text);

    setup.scratch.write_settings("")?;
    let withdrawn = mcp.call("get_resource", args)?;
    assert!(withdrawn.is_error, "{}", withdrawn.text);
    assert!(withdrawn.text.contains("not found"), "{}", withdrawn.text);
    assert!(!withdrawn.text.contains(OUTSIDE), "{}", withdrawn.text);
    let clusters = mcp.call_json("list_clusters", json!({}))?;
    assert_eq!(clusters["clusters"], json!([]), "{clusters}");
    Ok(())
}

// covers UP-15: a Secret's `data`, `stringData` and last-applied values reach no tool's reply
// and no file, while the object itself stays readable.
#[test]
fn up_15_a_secret_object_crosses_no_tool() -> Result<()> {
    let setup = Setup::exposed()?;
    let applied = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "stringData": {"password": APPLIED_VALUE},
    })
    .to_string();
    setup.api.set_object(
        &setup.api.gvr("secrets"),
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {
                "name": "held",
                "namespace": NAMESPACE,
                "annotations": {"kubectl.kubernetes.io/last-applied-configuration": applied},
            },
            "type": "Opaque",
            "data": {"password": ENCODED_VALUE},
            "stringData": {"token": STRING_VALUE},
        }),
    );
    let mut mcp = setup.mcp()?;
    let values = [ENCODED_VALUE, STRING_VALUE, APPLIED_VALUE];

    let list = |namespace: Option<&str>| json!({"cluster": EXPOSED, "kind": "secrets", "namespace": namespace});
    let get = |kind: &str| json!({"cluster": EXPOSED, "kind": kind, "namespace": NAMESPACE, "name": "held"});
    let calls = [
        ("list_clusters", json!({})),
        ("list_resource_kinds", json!({"cluster": EXPOSED})),
        ("list_resources", list(None)),
        ("list_resources", list(Some(NAMESPACE))),
        ("get_resource", get("secrets")),
        ("get_resource", get("secret")),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE, "object_name": "held"}),
        ),
        (
            "get_pod_logs",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE, "pod": POD}),
        ),
    ];
    for (tool, args) in &calls {
        let got = mcp.call(tool, args.clone())?;
        assert!(!got.is_error, "{tool} {args}: {}", got.text);
        for value in values {
            assert!(!got.text.contains(value), "{tool} {args}: {}", got.text);
        }
        if *tool == "get_resource" {
            assert!(got.text.contains("name: held"), "{}", got.text);
            assert!(got.text.contains("<redacted>"), "{}", got.text);
        }
    }

    assert_eq!(
        activity::read(&setup.scratch.config_dir()).len(),
        calls.len()
    );
    drop(mcp);
    assert_absent_on_disk(&setup, &values)
}
