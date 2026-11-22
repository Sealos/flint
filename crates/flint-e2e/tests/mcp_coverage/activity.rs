//! UP-16: the activity log, written by separate server processes on one config directory.

use std::path::Path;

use anyhow::Result;
use flint_core::activity::{self, ActivityRecord, Outcome};
use serde_json::json;

use crate::support::{EXPOSED, HIDDEN, NAMESPACE, POD, Setup};

/// Only in what the API server returns: it must never be on disk.
const RETURNED: &str = "placeholder-returned-content";

/// Every file below `dir`, as bytes.
pub(crate) fn files_below(dir: &Path) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            found.extend(files_below(&path)?);
        } else {
            let bytes = std::fs::read(&path)?;
            found.push((path, bytes));
        }
    }
    Ok(found)
}

fn last(config_dir: &Path) -> ActivityRecord {
    activity::read(config_dir)
        .pop()
        .expect("the call was recorded")
}

/// A value the redactor recognises, and the part of it that must not be recorded.
const PLANTED: &str = "password=placeholder-arg-value";
const PLANTED_VALUE: &str = "placeholder-arg-value";

// covers UP-16: every argument a record holds gets the same redaction as the reply, on each path
// to the log: a tool that ran, and a call refused for its arguments.
#[test]
fn up_16_recorded_arguments_are_redacted_like_replies() -> Result<()> {
    type Field = fn(&ActivityRecord) -> Option<&str>;
    let setup = Setup::exposed()?;
    let config_dir = setup.scratch.config_dir();
    let mut mcp = setup.mcp()?;

    let cases: Vec<(&str, serde_json::Value, Field)> = vec![
        (
            "list_resources",
            json!({"cluster": PLANTED, "kind": "pods", "namespace": NAMESPACE}),
            |r| r.cluster.as_deref(),
        ),
        (
            "list_resources",
            json!({"cluster": EXPOSED, "kind": PLANTED, "namespace": NAMESPACE}),
            |r| r.kind.as_deref(),
        ),
        (
            "list_resources",
            json!({"cluster": EXPOSED, "kind": "pods", "namespace": PLANTED}),
            |r| r.namespace.as_deref(),
        ),
        (
            "get_resource",
            json!({"cluster": EXPOSED, "kind": "pods", "namespace": NAMESPACE, "name": PLANTED}),
            |r| r.name.as_deref(),
        ),
        (
            "get_pod_logs",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE, "pod": POD, "container": PLANTED}),
            |r| r.container.as_deref(),
        ),
        (
            "get_pod_logs",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE, "pod": PLANTED}),
            |r| r.name.as_deref(),
        ),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE,
                   "object_kind": PLANTED, "object_name": "x"}),
            |r| r.kind.as_deref(),
        ),
        (
            "get_events",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE,
                   "object_kind": "pods", "object_name": PLANTED}),
            |r| r.name.as_deref(),
        ),
        // Refused for its arguments before the tool ran (`kind` is missing).
        (
            "get_resource",
            json!({"cluster": EXPOSED, "namespace": NAMESPACE, "name": PLANTED}),
            |r| r.name.as_deref(),
        ),
        // A tool that does not exist: its name is what the call wrote.
        (PLANTED, json!({}), |r| Some(r.tool.as_str())),
    ];
    for (tool, arguments, field) in cases {
        let context = format!("{tool} {arguments}");
        // A call the protocol refuses has no reply text to check, and is still recorded.
        if let Ok(reply) = mcp.call(tool, arguments) {
            assert!(
                !reply.text.contains(PLANTED_VALUE),
                "{context}: {}",
                reply.text
            );
        }
        let record = last(&config_dir);
        let recorded = field(&record).unwrap_or_default();
        assert!(recorded.contains("[REDACTED:"), "{context}: {record:?}");
    }
    drop(mcp);
    for (path, bytes) in files_below(&config_dir)? {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(PLANTED_VALUE),
            "planted value in {}",
            path.display()
        );
    }
    Ok(())
}

// covers UP-16: redaction leaves an ordinary name as written, so the record still matches its
// cluster.
#[test]
fn up_16_ordinary_arguments_are_recorded_unchanged() -> Result<()> {
    let setup = Setup::exposed()?;
    let config_dir = setup.scratch.config_dir();
    let mut mcp = setup.mcp()?;
    let call = json!({"cluster": EXPOSED, "namespace": NAMESPACE, "pod": POD, "container": "web"});
    mcp.call("get_pod_logs", call)?;
    let record = last(&config_dir);
    assert_eq!(record.cluster.as_deref(), Some(EXPOSED));
    assert_eq!(record.cluster_key, Some(activity::cluster_key(EXPOSED)));
    assert_eq!(record.kind.as_deref(), Some("pods"));
    assert_eq!(record.namespace.as_deref(), Some(NAMESPACE));
    assert_eq!(record.name.as_deref(), Some(POD));
    assert_eq!(record.container.as_deref(), Some("web"));
    Ok(())
}

// covers UP-16: each call from either process is in the log as soon as it returns, with its
// tool, target, size and outcome, and none of the returned content is stored.
#[test]
fn up_16_activity_log_across_processes() -> Result<()> {
    let setup = Setup::exposed()?;
    let log = format!("first {RETURNED}\nsecond {RETURNED}\nthird {RETURNED}\n");
    setup.api.set_log(NAMESPACE, POD, &log);
    setup.expose(&[NAMESPACE], &[])?;
    let config_dir = setup.scratch.config_dir();
    let mut one = setup.mcp()?;
    let mut two = setup.mcp()?;

    let logs = json!({"cluster": EXPOSED, "namespace": NAMESPACE, "pod": POD, "tail_lines": 1});
    let reply = one.call("get_pod_logs", logs)?;
    assert!(reply.text.contains(RETURNED), "{}", reply.text);
    let record = last(&config_dir);
    assert_eq!(record.tool, "get_pod_logs");
    assert_eq!(record.cluster.as_deref(), Some(EXPOSED));
    assert_eq!(record.namespace.as_deref(), Some(NAMESPACE));
    assert_eq!(record.name.as_deref(), Some(POD));
    assert_eq!(record.items, Some(1));
    assert_eq!(record.bytes, reply.text.len() as u64);
    assert!(record.truncated, "{record:?}");
    assert_eq!(record.outcome, Outcome::Ok);

    let pods = json!({"cluster": EXPOSED, "kind": "pods", "namespace": NAMESPACE});
    let reply = two.call("list_resources", pods)?;
    assert!(!reply.is_error, "{}", reply.text);
    let record = last(&config_dir);
    assert_eq!(record.tool, "list_resources");
    assert_eq!(record.kind.as_deref(), Some("pods"));
    assert_eq!(record.bytes, reply.text.len() as u64);
    assert!(!record.truncated, "{record:?}");
    assert_eq!(record.outcome, Outcome::Ok);

    // Outside the scope, and a cluster that is not exposed: denied.
    let outside = json!({"cluster": EXPOSED, "kind": "pods", "namespace": "kube-system"});
    assert!(one.call("list_resources", outside)?.is_error);
    let record = last(&config_dir);
    assert_eq!(record.namespace.as_deref(), Some("kube-system"));
    assert_eq!(record.outcome, Outcome::Denied);
    let hidden = json!({"cluster": HIDDEN, "kind": "pods", "namespace": NAMESPACE});
    assert!(two.call("list_resources", hidden)?.is_error);
    assert_eq!(last(&config_dir).outcome, Outcome::Denied);

    // In scope, but missing: failed, not denied.
    let missing = json!({"cluster": EXPOSED, "kind": "pods", "namespace": NAMESPACE, "name": "x"});
    assert!(one.call("get_resource", missing)?.is_error);
    assert_eq!(last(&config_dir).outcome, Outcome::Failed);

    // Listing the clusters counts on the cluster it probed.
    one.call_json("list_clusters", json!({}))?;
    let record = last(&config_dir);
    assert_eq!(record.tool, "list_clusters");
    assert_eq!(record.probed.len(), 1, "{record:?}");
    assert_eq!(
        record.probed[0].cluster_key,
        activity::cluster_key(EXPOSED),
        "{record:?}"
    );

    assert_eq!(activity::read(&config_dir).len(), 6);
    drop((one, two));
    for (path, bytes) in files_below(&config_dir)? {
        assert!(
            !String::from_utf8_lossy(&bytes).contains(RETURNED),
            "returned content in {}",
            path.display()
        );
    }
    Ok(())
}
