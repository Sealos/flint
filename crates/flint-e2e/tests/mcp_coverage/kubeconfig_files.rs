//! UP-54: `flint mcp` follows the app's handling of a kubeconfig file it can't read.

use std::path::{Path, PathBuf};

use anyhow::Result;
use flint_e2e::fake_api::{FakeApiServer, Options};
use flint_e2e::mcp::Mcp;
use flint_e2e::scratch::Scratch;
use serde_json::{Value, json};

/// Defined only in the second file, so the first file can't change it.
const STEADY: &str = "fake-steady";
/// Defined in both files; the first file's definition wins.
const SHARED: &str = "fake-shared";

/// Two contexts over two kubeconfig files, each entry counting the connections made with it.
struct Files {
    steady: FakeApiServer,
    shared: FakeApiServer,
    scratch: Scratch,
    first: PathBuf,
    second: PathBuf,
    first_text: String,
}

impl Files {
    fn new() -> Result<Files> {
        let steady = FakeApiServer::start_blocking(Options::default())?;
        let shared = FakeApiServer::start_blocking(Options::default())?;
        let scratch = Scratch::new()?;
        let (first, second) = (scratch.path().join("first"), scratch.path().join("second"));
        let first_text = config(&[(&shared, SHARED, &runs(&scratch, "shared-first"))])?;
        let second_text = config(&[
            (&steady, STEADY, &runs(&scratch, "steady")),
            (&shared, SHARED, &runs(&scratch, "shared-second")),
        ])?;
        std::fs::write(&first, &first_text)?;
        std::fs::write(&second, second_text)?;
        scratch.write_settings(&format!(
            "\n[[ai_exposed_clusters]]\ncontext = \"{STEADY}\"\nserver = \"{}\"\n\
             \n[[ai_exposed_clusters]]\ncontext = \"{SHARED}\"\nserver = \"{}\"\n",
            steady.url(),
            shared.url(),
        ))?;
        Ok(Files {
            steady,
            shared,
            scratch,
            first,
            second,
            first_text,
        })
    }

    fn mcp(&self) -> Result<Mcp> {
        let list = std::env::join_paths([&self.first, &self.second])?;
        Mcp::start(list, &self.scratch.config_dir())
    }

    /// How many connections the entry called `name` made.
    fn uses(&self, name: &str) -> usize {
        std::fs::read_to_string(runs(&self.scratch, name)).map_or(0, |text| text.lines().count())
    }
}

/// The file the entry called `name` counts its connections in.
fn runs(scratch: &Scratch, name: &str) -> PathBuf {
    scratch.path().join(format!("{name}.runs"))
}

/// A kubeconfig with each `(server, context, counter)`; connecting adds a line to `counter`. JSON
/// is valid YAML.
fn config(entries: &[(&FakeApiServer, &str, &Path)]) -> Result<String> {
    let credential = r#"{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"placeholder"}}"#;
    let (mut clusters, mut contexts, mut users) = (vec![], vec![], vec![]);
    for (server, context, counter) in entries {
        let mut one = serde_json::to_value(server.kubeconfig_named(context, None))?;
        let script = format!(
            "echo run >> '{}'; printf '%s' '{credential}'",
            counter.display()
        );
        one["users"][0]["user"] = json!({"exec": {
            "apiVersion": "client.authentication.k8s.io/v1",
            "command": "/bin/sh",
            "args": ["-c", script],
            "interactiveMode": "Never",
        }});
        clusters.push(one["clusters"][0].take());
        contexts.push(one["contexts"][0].take());
        users.push(one["users"][0].take());
    }
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Config",
        "clusters": clusters,
        "contexts": contexts,
        "users": users,
    })
    .to_string())
}

fn pods(context: &str) -> Value {
    json!({"cluster": context, "kind": "pods", "namespace": "demo"})
}

/// Asserts `context` answers a read.
fn assert_reads(mcp: &mut Mcp, context: &str) -> Result<()> {
    mcp.call_json("list_resources", pods(context))?;
    Ok(())
}

/// Asserts `context` is refused as paused.
fn assert_paused(mcp: &mut Mcp, context: &str) -> Result<()> {
    let got = mcp.call("list_resources", pods(context))?;
    assert!(got.is_error, "{}", got.text);
    assert!(got.text.contains("is paused"), "{}", got.text);
    Ok(())
}

// covers UP-54: with the first file broken, a working context keeps connecting, a context whose
// definition would change is refused before it connects, and repairing the file brings it back
// on its own definition.
#[test]
fn up_54_a_broken_file_pauses_only_a_changed_definition() -> Result<()> {
    let files = Files::new()?;
    let mut mcp = files.mcp()?;
    assert_reads(&mut mcp, STEADY)?;
    assert_reads(&mut mcp, SHARED)?;
    assert!(files.uses("steady") > 0);
    let first_runs = files.uses("shared-first");
    assert!(first_runs > 0);

    std::fs::write(&files.first, "clusters: [not valid")?;
    assert_reads(&mut mcp, STEADY)?;
    assert_paused(&mut mcp, SHARED)?;
    let listed = mcp.call_json("list_clusters", json!({}))?;
    let row = |name: &str| {
        listed["clusters"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["name"] == name))
            .cloned()
            .unwrap_or_default()
    };
    assert_eq!(row(STEADY)["reachable"], json!(true), "{listed}");
    assert_eq!(row(SHARED)["reachable"], json!(false), "{listed}");
    assert_eq!(files.uses("shared-second"), 0, "never ran");

    std::fs::write(&files.first, &files.first_text)?;
    assert_reads(&mut mcp, SHARED)?;
    assert_reads(&mut mcp, STEADY)?;
    assert_eq!(files.uses("shared-second"), 0, "still never ran");
    assert!(files.steady.arrivals().len() > 1 && files.shared.arrivals().len() > 1);
    Ok(())
}

// covers UP-54: a file emptied by a writer that truncates first counts as unreadable meanwhile.
#[test]
fn up_54_an_emptied_file_pauses_a_changed_definition() -> Result<()> {
    let files = Files::new()?;
    let mut mcp = files.mcp()?;
    assert_reads(&mut mcp, SHARED)?;

    std::fs::write(&files.first, "")?;
    assert_paused(&mut mcp, SHARED)?;
    assert_reads(&mut mcp, STEADY)?;
    assert_eq!(files.uses("shared-second"), 0);

    std::fs::write(&files.first, &files.first_text)?;
    assert_reads(&mut mcp, SHARED)?;
    assert_eq!(files.uses("shared-second"), 0);
    Ok(())
}
