//! UP-14: what a scope lets an agent read.

use anyhow::Result;
use serde_json::{Value, json};

use crate::support::{EXPOSED, HIDDEN, NAMESPACE, POD, Setup};

const OTHER_NAMESPACE: &str = "kube-system";

fn out_of_scope() -> String {
    format!(
        "this request is outside the scope exposed to agents on cluster \"{EXPOSED}\". Call \
         list_clusters for the namespaces and kinds in scope."
    )
}

// covers UP-14: every tool answers a request beyond the scope with the same bytes, whether what
// it named exists or not, and sends the API server nothing about the other namespace.
#[test]
fn up_14_scope_limits_what_an_agent_reads() -> Result<()> {
    let setup = Setup::exposed()?;
    setup.api.set_object(
        &setup.api.gvr("pods"),
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "exists", "namespace": OTHER_NAMESPACE},
        }),
    );
    setup.expose(&[NAMESPACE], &["pods"])?;
    let mut mcp = setup.mcp()?;
    let before = setup.api.requests().len();

    let object = |namespace: &str, name: &str| json!({"cluster": EXPOSED, "kind": "pods", "namespace": namespace, "name": name});
    let logs = |namespace: &str, pod: &str| json!({"cluster": EXPOSED, "namespace": namespace, "pod": pod});
    let denied: Vec<(&str, Value)> = vec![
        (
            "list_resources",
            json!({"cluster": EXPOSED, "kind": "pods", "namespace": OTHER_NAMESPACE}),
        ),
        (
            "list_resources",
            json!({"cluster": EXPOSED, "kind": "pods", "namespace": "no-such-namespace"}),
        ),
        ("get_resource", object(OTHER_NAMESPACE, "exists")),
        ("get_resource", object(OTHER_NAMESPACE, "missing")),
        ("get_pod_logs", logs(OTHER_NAMESPACE, "exists")),
        ("get_pod_logs", logs(OTHER_NAMESPACE, "missing")),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": OTHER_NAMESPACE}),
        ),
    ];
    for (tool, args) in denied {
        let got = mcp.call(tool, args.clone())?;
        assert!(got.is_error, "{tool} {args}: {}", got.text);
        assert_eq!(got.text, out_of_scope(), "{tool} {args}");
    }
    let sent = setup.api.requests();
    assert!(
        sent[before..]
            .iter()
            .all(|r| !r.path.contains(OTHER_NAMESPACE)),
        "{:?}",
        &sent[before..]
    );

    // Another kind in the namespace in scope, a cluster-scoped kind not named, and a missing one.
    for kind in ["services", "nodes", "gizmos"] {
        let args = json!({"cluster": EXPOSED, "kind": kind, "namespace": NAMESPACE});
        let got = mcp.call("list_resources", args)?;
        assert!(got.is_error, "{kind}: {}", got.text);
        assert_eq!(got.text, out_of_scope(), "{kind}");
    }

    // What is in scope still reads.
    let pods = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "pods", "namespace": NAMESPACE}),
    )?;
    assert!(!pods["rows"].as_array().unwrap().is_empty(), "{pods}");
    mcp.call_json("get_pod_logs", logs(NAMESPACE, POD))?;

    // A cluster-scoped kind appears once it is named.
    setup.expose(&[NAMESPACE], &["pods", "nodes"])?;
    let nodes = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "nodes"}),
    )?;
    assert!(!nodes["rows"].as_array().unwrap().is_empty(), "{nodes}");

    // Clearing the scope restores everything, on the next call.
    setup.expose(&[], &[])?;
    let restored = mcp.call("get_resource", object(OTHER_NAMESPACE, "exists"))?;
    assert!(!restored.is_error, "{}", restored.text);
    mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "services", "namespace": NAMESPACE}),
    )?;
    assert!(setup.hidden.requests().is_empty());
    Ok(())
}

// covers UP-14: a cluster that is not exposed answers as one that does not exist, and reaches
// no server.
#[test]
fn up_14_an_unexposed_cluster_reads_as_unknown() -> Result<()> {
    let setup = Setup::exposed()?;
    let mut mcp = setup.mcp()?;
    let call = |cluster: &str| json!({"cluster": cluster, "kind": "pods", "namespace": NAMESPACE});
    let hidden = mcp.call("list_resources", call(HIDDEN))?;
    let unknown = mcp.call("list_resources", call("no-such-cluster"))?;
    assert!(hidden.is_error && unknown.is_error);
    assert!(hidden.text.contains("not found"), "{}", hidden.text);
    assert_eq!(
        hidden.text.replace(HIDDEN, "X"),
        unknown.text.replace("no-such-cluster", "X")
    );
    assert!(setup.hidden.requests().is_empty());
    Ok(())
}

// covers UP-14: the tool set is read-only, with or without a scope.
#[test]
fn up_14_every_tool_is_declared_read_only() -> Result<()> {
    let setup = Setup::exposed()?;
    for (namespaces, kinds) in [(&[][..], &[][..]), (&[NAMESPACE][..], &["pods"][..])] {
        setup.expose(namespaces, kinds)?;
        let mut mcp = setup.mcp()?;
        assert_eq!(
            mcp.tools()?,
            [
                "get_events",
                "get_pod_logs",
                "get_resource",
                "list_clusters",
                "list_resource_kinds",
                "list_resources",
            ]
        );
        let tools = mcp.request("tools/list", json!({}))?;
        for tool in tools["tools"].as_array().unwrap() {
            assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
            assert_ne!(tool["annotations"]["destructiveHint"], true, "{tool}");
        }
    }
    let writes = setup.api.requests();
    assert!(writes.iter().all(|r| r.method == "GET"), "{writes:?}");
    Ok(())
}
