//! UP-16: the activity log, written by separate server processes on one config directory.

use std::path::Path;

use anyhow::Result;
use flint_core::activity::{self, ActivityRecord, Outcome};
use serde_json::json;

use crate::support::{EXPOSED, HIDDEN, NAMESPACE, POD, Setup};

/// Only in what the API server returns: it must never be on disk.
const RETURNED: &str = "placeholder-returned-content";

/// Every file below `dir`, as bytes.
fn files_below(dir: &Path) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>> {
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
