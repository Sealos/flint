//! M3 user paths: protected clusters (UP-54, UP-55), against the fake API server.
//!
//! The fake server listens on `127.0.0.1`, and clusters on this machine start unlocked, so the
//! tests turn on `protect_local_clusters` to meet a protected one. Not here, and why:
//!
//! - **A remote cluster protected by default, and a trusted context re-pointed at another
//!   server** (UP-54, UP-55): the fake server cannot sit at a remote address. `flint-core`'s
//!   `protection.rs`, `settings.rs` and `manager.rs` tests and `ui/protect_tests.rs` cover the
//!   address rule and the pinning.
//! - **Edit, delete, scale, restart, drain and port-forward** (UP-54): not built yet; they go
//!   through the same gate in the core when they arrive (M3, M5).
//! - **The lock button's menu** (UP-55): a dropdown the automation channel cannot open; its
//!   entries are the actions dispatched here, and `ui/protect_tests.rs` reads its tooltips.

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::backend::TestCluster;
use flint_e2e::fake_api::Options;
use serde_json::{Value, json};

use crate::support::*;

/// The fixtures' web pod: nginx, which has bash.
const WEB: &str = "web-69c6f74b8b-qv4xv";

/// The detail pane's entry for `action` (`Logs`, `Shell`).
fn action<'a>(state: &'a Value, action: &str) -> Result<&'a Value> {
    state["detail"]["actions"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["action"] == action))
        .with_context(|| format!("no {action} in {}", state["detail"]))
}

// covers UP-54, UP-55
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_54_55_protected_clusters_are_read_only_until_unlocked_or_trusted() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let fake = cluster.fake_server().expect("a fake server");
    let context = cluster.context.as_str();
    let mut app = App::launch(
        AppOptions::new(&cluster.kubeconfig).settings("protect_local_clusters = true\n"),
    )?;
    connect(&app, context)?;
    assert_eq!(sidebar(&app.state()?, context)["protection"], "protected");

    // Browsing and logs work; Shell is disabled and says why.
    open_live(&app, "pods", 17)?;
    app.select_row(Some("demo"), WEB)?;
    app.wait_for("/detail/fetch", Equals("loaded".into()))?;
    let state = app.wait_for(
        "/detail/actions",
        Any(json!({"action": "Shell", "enabled": false})),
    )?;
    let why = format!("{context} is protected (read-only in Flint) until you unlock it");
    assert_eq!(action(&state, "Shell")?["reason"], why.as_str());
    assert_eq!(action(&state, "Logs")?["enabled"], true);
    assert!(app.dispatch("detail::OpenShell", Value::Null).is_err());
    app.screenshot_themes("up54-protected")?;

    // Unlock: the confirmation names the cluster and its server; Escape keeps it protected.
    app.dispatch("workspace::UnlockCluster", json!({"cluster": context}))?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    let text = state["dialog"]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains(context) && text.contains(&cluster.server),
        "{state}"
    );
    app.screenshot_themes("up55-unlock-dialog")?;
    app.press_key("escape")?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    assert_eq!(sidebar(&app.state()?, context)["protection"], "protected");
    app.dispatch("workspace::UnlockCluster", json!({"cluster": context}))?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": "unlocked"})),
    )?;
    app.wait_for(
        "/detail/actions",
        Any(json!({"action": "Shell", "enabled": true})),
    )?;
    assert!(fake.exec_requests().is_empty(), "nothing ran before");

    // The shell opens and runs.
    app.dispatch("detail::OpenShell", Value::Null)?;
    app.wait_for("/dock/tabs/0/status/state", Equals("running".into()))?;
    assert_eq!(fake.exec_requests().len(), 1);
    app.screenshot_themes("up55-unlocked-shell")?;

    // Lock Again: Shell is disabled again, and Restart Shell is refused, saying why.
    app.dispatch(
        "dock::CloseTab",
        json!({"tab": app.state()?["dock"]["tabs"][0]["id"]}),
    )?;
    app.wait_for("/dock/tabs", Equals(json!([])))?;
    app.dispatch("workspace::LockCluster", json!({"cluster": context}))?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": "protected"})),
    )?;
    let state = app.wait_for(
        "/detail/actions",
        Any(json!({"action": "Shell", "enabled": false})),
    )?;
    assert_eq!(action(&state, "Shell")?["reason"], why.as_str());
    assert_eq!(fake.exec_requests().len(), 1);

    // Trust: confirmed, written to settings.toml with the server, and kept across a restart.
    app.dispatch("workspace::TrustCluster", json!({"cluster": context}))?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    let text = state["dialog"]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains(context) && text.contains(&cluster.server),
        "{state}"
    );
    app.dispatch("ui::Confirm", Value::Null)?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": "trusted"})),
    )?;
    let stored = std::fs::read_to_string(app.config_dir().join("settings.toml"))?;
    assert!(
        stored.contains("trusted_clusters") && stored.contains(&cluster.server),
        "{stored}"
    );
    let (scratch, status) = app.quit()?;
    assert!(status.success(), "{status}");
    app = App::launch(AppOptions::new(&cluster.kubeconfig).reuse(scratch))?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": "trusted"})),
    )?;

    // Stop Trusting: protected again.
    app.dispatch("workspace::UntrustCluster", json!({"cluster": context}))?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": context, "protection": "protected"})),
    )?;
    Ok(())
}
