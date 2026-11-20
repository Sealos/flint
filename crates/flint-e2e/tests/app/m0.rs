//! M0 user paths: clusters, connections, Expose to AI, settings (docs/USER-PATHS.md).
//!
//! Not here:
//! - UP-05 (exec-plugin auth when launched from Finder): needs a real cloud cluster and a Finder
//!   launch; manual at release. PATH resolution is unit-tested in flint-core.
//! - UP-09, UP-10, UP-11, UP-15: MCP only, no app; in tests/mcp.rs (they run on Linux too).
//!   UP-15's "the app shows logs and events as written" is in tests/core.rs.
//! - UP-14, UP-16 (scoped exposure, agent activity): not built yet (planned in #70); they get
//!   tests when they land.
//! - UP-13's "follows macOS live" needs the system appearance to change, which a test must not
//!   do to the user's Mac. Dark mode working (M0's bar) is checked here through the View >
//!   Appearance actions.

use std::time::{Duration, Instant};

use anyhow::Result;
use flint_e2e::app::{App, AppOptions, WAIT};
use flint_e2e::backend::{Backend, TestCluster};
use flint_e2e::fake_api::{Fault, Options};
use flint_e2e::mcp::Mcp;
use serde_json::{Value, json};

use crate::support::*;

const ARN: &str = "arn:aws:eks:us-east-1:123456789012:cluster/payments";

fn unreachable_kubeconfig(contexts: &[&str]) -> String {
    let mut text = format!(
        "apiVersion: v1\nkind: Config\nclusters:\n- name: nowhere\n  cluster: {{server: \"https://127.0.0.1:{}\"}}\nusers:\n- name: u\n  user: {{}}\ncontexts:\n",
        closed_port()
    );
    for context in contexts {
        text.push_str(&format!(
            "- name: \"{context}\"\n  context: {{cluster: nowhere, user: u}}\n"
        ));
    }
    text
}

// covers UP-01, UP-91
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_01_launch_and_see_my_clusters() -> Result<()> {
    let two = TwoClusters::start()?;
    // A second file redefines fake-a (at a dead server) and adds an EKS context; a third comes
    // from settings' kubeconfig_paths.
    let second = two.scratch.path().join("second");
    write(&second, &unreachable_kubeconfig(&[TwoClusters::A, ARN]))?;
    let extra = two.scratch.path().join("extra");
    write(&extra, &unreachable_kubeconfig(&["from-settings"]))?;
    let list = format!("{}:{}", two.kubeconfig().display(), second.display());
    let app = App::launch(AppOptions::with_kubeconfig_list(&list).settings(&format!(
        "kubeconfig_paths = [{:?}]\n",
        extra.display().to_string()
    )))?;

    let state = app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    assert_eq!(
        cluster_names(&state),
        [TwoClusters::A, TwoClusters::B, ARN, "from-settings"],
        "every context once, in merge order"
    );
    // The short name is drawn; the full context (its tooltip) stays the cluster's name. The
    // tooltip itself is not in `state`.
    assert_eq!(sidebar(&state, ARN)["label"], "cluster/payments");
    assert_eq!(sidebar(&state, ARN)["name"], ARN);
    assert_eq!(sidebar(&state, TwoClusters::A)["label"], TwoClusters::A);
    assert_eq!(state["active_cluster"], Value::Null);
    assert_eq!(state["main"]["view"], "no_cluster");

    // Listing clusters reached no server.
    std::thread::sleep(Duration::from_millis(500));
    assert!(two.a.requests().is_empty(), "{:?}", two.a.requests());
    assert!(two.b.requests().is_empty(), "{:?}", two.b.requests());
    app.screenshot_themes("up01-clusters")?;

    // The first definition of fake-a wins: it connects to the live server, not the dead one.
    connect(&app, TwoClusters::A)?;
    assert!(!two.a.requests().is_empty());
    assert!(
        two.b.requests().is_empty(),
        "only the selected cluster is reached"
    );
    Ok(())
}

// covers UP-02
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_02_launch_with_no_usable_kubeconfig() -> Result<()> {
    let scratch = flint_e2e::scratch::Scratch::new()?;
    let missing = scratch.path().join("missing");
    let broken = scratch.path().join("broken-config");
    write(
        &broken,
        "users:\n- name: x\n  user:\n    token: BROKENSECRET\nclusters: [\n",
    )?;
    let good = scratch.path().join("good");
    write(&good, &unreachable_kubeconfig(&["still-here"]))?;

    // No KUBECONFIG, and nothing in (the scratch) ~/.kube: an empty state naming the default
    // path and saying KUBECONFIG is unset.
    let app = App::launch(AppOptions::no_kubeconfig())?;
    let state = app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    assert_eq!(state["clusters"], json!([]));
    assert_eq!(state["kubeconfig"]["env"], Value::Null);
    let paths = state["kubeconfig"]["paths"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(paths.len(), 1, "{state}");
    assert!(
        paths[0]["path"]
            .as_str()
            .unwrap_or_default()
            .ends_with("home/.kube/config"),
        "{state}"
    );
    assert_eq!(paths[0]["exists"], false);
    app.screenshot_themes("up02-unset-kubeconfig")?;
    drop(app);

    // Only files that don't exist: an empty state naming them and KUBECONFIG.
    let app = App::launch(AppOptions::new(&missing))?;
    let state = app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    assert_eq!(state["clusters"], json!([]));
    assert_eq!(state["kubeconfig"]["env"], missing.display().to_string());
    assert_eq!(
        state["kubeconfig"]["paths"],
        json!([{"path": missing.display().to_string(), "exists": false}])
    );
    app.screenshot_themes("up02-no-kubeconfig")?;
    drop(app);

    // A broken file names itself and the first line of its error; the valid file still loads.
    let list = format!(
        "{}:{}:{}",
        missing.display(),
        broken.display(),
        good.display()
    );
    let app = App::launch(AppOptions::with_kubeconfig_list(&list))?;
    let state = app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    assert_eq!(cluster_names(&state), ["still-here"]);
    let banners = state["banners"].to_string();
    assert!(banners.contains("broken-config"), "{banners}");
    assert!(!banners.contains("BROKENSECRET"), "{banners}");
    app.screenshot_themes("up02-broken-kubeconfig")?;
    Ok(())
}

// covers UP-03
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_03_connect_to_a_cluster() -> Result<()> {
    let Some(cluster) = Backend::from_env().start()? else {
        return Ok(());
    };
    // The fake server answers slowly at first, so "connecting" is visible.
    if let Some(fake) = cluster.fake_server() {
        fake.inject(Fault::delay(Duration::from_secs(4)).times(1));
    }
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    app.select_cluster(&cluster.context)?;
    if cluster.fake_server().is_some() {
        let state = app.wait_for("/main/view", Equals("connecting".into()))?;
        assert_eq!(cluster_conn(&state, &cluster.context), "connecting");
        // The UI answers while connecting.
        assert!(app.actions()?["actions"].as_array().is_some());
        app.screenshot("up03-connecting")?;
        assert_eq!(app.state()?["main"]["view"], "connecting");
    }
    let state = app.wait_for("/main/view", Equals("connected".into()))?;
    assert_eq!(cluster_conn(&state, &cluster.context), "connected");
    assert_eq!(sidebar(&state, &cluster.context)["active"], true);
    let version = state["main"]["version"].as_str().unwrap_or_default();
    assert!(version.starts_with('v'), "server version: {state}");
    app.screenshot_themes("up03-connected")?;
    Ok(())
}

fn cluster_conn(state: &Value, name: &str) -> String {
    sidebar(state, name)["connection"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

// covers UP-04
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_04_connection_fails_with_a_plain_cause_and_retries() -> Result<()> {
    // `failing` answers every request with 500 until it recovers; `healthy` always works.
    let two = TwoClusters::start()?;
    let (failing, healthy) = (&two.a, &two.b);
    let others = two.scratch.path().join("others");
    let nowhere = format!("https://127.0.0.1:{}", closed_port());
    write(
        &others,
        &format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: nowhere\n  cluster: {{server: \"{nowhere}\"}}\nusers:\n- name: plugin\n  user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1beta1\n      command: flint-e2e-no-such-plugin\n      interactiveMode: Never\n- name: u\n  user: {{}}\ncontexts:\n- name: unreachable\n  context: {{cluster: nowhere, user: u}}\n- name: missing-plugin\n  context: {{cluster: nowhere, user: plugin}}\n"
        ),
    )?;
    failing.inject(Fault::status(500));
    let list = format!("{}:{}", two.kubeconfig().display(), others.display());
    let app = App::launch(AppOptions::with_kubeconfig_list(&list))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;

    // An unreachable server: the cause, and a Retry.
    app.select_cluster("unreachable")?;
    let state = app.wait_for("/main/view", Equals("failed".into()))?;
    let error = state["main"]["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    // In plain words, naming the server, not the client's chain ("ServiceError: client error
    // (Connect): tcp connect error: Connection refused (os error 61)").
    assert_eq!(
        error,
        format!(
            "connection refused by {nowhere}; is the API server running, and is the port right?"
        )
    );
    app.screenshot_themes("up04-unreachable")?;

    // A missing exec plugin is named as one, with how to fix it.
    app.select_cluster("missing-plugin")?;
    let state = app.wait_for("/main/error", Contains("exec auth plugin".into()))?;
    let error = state["main"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("not found on PATH"), "{state}");
    // The program's name is kept (only its directory and arguments are hidden).
    assert!(
        error.starts_with("exec auth plugin `flint-e2e-no-such-plugin` not found"),
        "{state}"
    );
    assert!(error.contains("settings.toml"), "{state}");
    app.screenshot_themes("up04-missing-plugin")?;

    // The failing server fails; the other clusters keep working meanwhile.
    app.select_cluster(TwoClusters::A)?;
    app.wait_for("/main/view", Equals("failed".into()))?;
    connect(&app, TwoClusters::B)?;
    open_live(&app, "pods", 17)?;
    assert!(!healthy.requests().is_empty());
    let state = app.state()?;
    assert_eq!(sidebar(&state, TwoClusters::A)["connection"], "failed");
    assert_eq!(sidebar(&state, "unreachable")["connection"], "failed");

    // Back to the failing one: Retry works once it recovers.
    app.select_cluster(TwoClusters::A)?;
    app.wait_for("/main/view", Equals("failed".into()))?;
    failing.clear_faults();
    app.dispatch("workspace::RetryConnection", Value::Null)?;
    app.wait_for("/main/view", Equals("connected".into()))?;
    app.wait_for("/nav/status", Equals("ready".into()))?;
    open_live(&app, "pods", 17)?;
    Ok(())
}

// covers UP-06
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_06_kubeconfig_changes_on_disk() -> Result<()> {
    let two = TwoClusters::start()?;
    let kubeconfig = two.kubeconfig();
    let both = std::fs::read_to_string(&kubeconfig)?;
    // Only fake-a at first.
    flint_e2e::fake_api::write_kubeconfigs(&kubeconfig, &[(&two.a, TwoClusters::A, None)])?;
    let app = App::launch(AppOptions::new(&kubeconfig))?;
    let state = app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    assert_eq!(cluster_names(&state), [TwoClusters::A]);
    connect(&app, TwoClusters::A)?;

    // A context is added (written in two steps, as a slow writer does): fake-b appears, and
    // fake-a never disappears on the way.
    let half = &both[..both.len() / 2];
    std::fs::write(&kubeconfig, half)?;
    std::thread::sleep(Duration::from_millis(30));
    std::fs::write(&kubeconfig, &both)?;
    let deadline = Instant::now() + WAIT;
    loop {
        let state = app.state()?;
        let names = cluster_names(&state);
        assert!(
            names.iter().any(|n| n == TwoClusters::A),
            "fake-a flickered away: {names:?}"
        );
        assert_eq!(state["active_cluster"], TwoClusters::A);
        if names.len() == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fake-b never appeared: {names:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The active cluster is removed: it is deselected, with a notice.
    flint_e2e::fake_api::write_kubeconfigs(&kubeconfig, &[(&two.b, TwoClusters::B, None)])?;
    let state = app.wait_for("/active_cluster", Equals(Value::Null))?;
    assert_eq!(cluster_names(&state), [TwoClusters::B]);
    let state = app.wait_for("/banners", Any(json!({"id": "last-error"})))?;
    assert!(
        state["banners"].to_string().contains("was removed"),
        "{state}"
    );
    app.screenshot_themes("up06-active-removed")?;
    Ok(())
}

// covers UP-07
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_07_expose_a_cluster_to_ai() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    let settings = app.config_dir().join("settings.toml");

    // The confirmation names the context and its server; nothing is stored before confirming.
    app.dispatch("workspace::ExposeToAi", json!({"cluster": cluster.context}))?;
    let state = app.wait_for("/dialog/title", Exists(true))?;
    let text = state["dialog"]["text"].as_str().unwrap_or_default();
    assert!(text.contains(&cluster.context), "{state}");
    assert!(text.contains(&cluster.server), "{state}");
    assert_eq!(sidebar(&state, &cluster.context)["ai"], "hidden");
    app.screenshot_themes("up07-expose-dialog")?;

    // Cancel (Escape) leaves it hidden.
    app.press_key("escape")?;
    app.wait_for("/dialog", Equals(Value::Null))?;
    assert_eq!(sidebar(&app.state()?, &cluster.context)["ai"], "hidden");
    assert!(!std::fs::read_to_string(&settings)?.contains("ai_exposed_clusters"));

    // Confirm: the badge, and settings.toml stores the context and server.
    app.dispatch("workspace::ExposeToAi", json!({"cluster": cluster.context}))?;
    app.wait_for("/dialog/title", Exists(true))?;
    app.dispatch("ui::Confirm", Value::Null)?;
    let state = app.wait_for(
        "/clusters",
        Any(json!({"name": cluster.context, "ai": "exposed"})),
    )?;
    assert_eq!(state["dialog"], Value::Null);
    let stored = std::fs::read_to_string(&settings)?;
    assert!(
        stored.contains(&cluster.context) && stored.contains(&cluster.server),
        "{stored}"
    );
    app.screenshot_themes("up07-exposed")?;

    // A stray key press on the selected row toggles nothing.
    connect(&app, &cluster.context)?;
    let state = app.state()?;
    assert_eq!(sidebar(&state, &cluster.context)["active"], true);
    assert_eq!(state["focus"], "workspace", "{}", state["focus"]);
    for key in ["space", "enter", "a"] {
        app.press_key(key)?;
    }
    std::thread::sleep(Duration::from_millis(300));
    let state = app.state()?;
    assert_eq!(sidebar(&state, &cluster.context)["ai"], "exposed");
    assert_eq!(state["dialog"], Value::Null);

    // Turning it off needs no confirmation.
    app.dispatch("workspace::HideFromAi", json!({"cluster": cluster.context}))?;
    let state = app.wait_for(
        "/clusters",
        Any(json!({"name": cluster.context, "ai": "hidden"})),
    )?;
    assert_eq!(state["dialog"], Value::Null);
    Ok(())
}

// covers UP-08, UP-12
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_08_up_12_app_and_flint_mcp_agree_on_exposure() -> Result<()> {
    let two = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    let config_dir = app.config_dir();
    let settings = |extra: &str| flint_e2e::scratch::write_settings(&config_dir, extra);
    let mut mcp = Mcp::flint(app.binary(), two.kubeconfig(), &config_dir)?;
    let listed = |mcp: &mut Mcp| -> Result<Vec<String>> {
        let clusters = mcp.call_json("list_clusters", json!({}))?;
        Ok(clusters["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str().map(str::to_owned))
            .collect())
    };

    // Exposed by hand in settings.toml: the app shows it, and `flint mcp` lists it.
    settings(&format!(
        "[[ai_exposed_clusters]]\ncontext = \"{}\"\nserver = \"{}\"\n",
        TwoClusters::A,
        two.a.url()
    ))?;
    app.wait_for(
        "/clusters",
        Any(json!({"name": TwoClusters::A, "ai": "exposed"})),
    )?;
    assert_eq!(listed(&mut mcp)?, [TwoClusters::A]);

    // Re-pointed: fake-b exposed at fake-a's server. The app says "cluster changed" and
    // `flint mcp` treats it as unknown.
    settings(&format!(
        "[[ai_exposed_clusters]]\ncontext = \"{}\"\nserver = \"{}\"\n",
        TwoClusters::B,
        two.a.url()
    ))?;
    let state = app.wait_for(
        "/clusters",
        Any(json!({"name": TwoClusters::B, "ai": "server_changed"})),
    )?;
    assert_eq!(sidebar(&state, TwoClusters::A)["ai"], "hidden");
    assert!(listed(&mut mcp)?.is_empty());
    let pods = mcp.call(
        "list_resources",
        json!({"cluster": TwoClusters::B, "kind": "pods"}),
    )?;
    assert!(
        pods.is_error && pods.text.contains("not found"),
        "{}",
        pods.text
    );
    app.screenshot_themes("up08-server-changed")?;

    // Malformed: a banner in the app, and `flint mcp` exposes nothing.
    settings("ai_exposed_clusters = [\n")?;
    let state = app.wait_for("/banners", Any(json!({"id": "settings-error"})))?;
    assert!(
        state["banners"].to_string().contains("settings.toml"),
        "{state}"
    );
    let broken = mcp.call("list_clusters", json!({}))?;
    assert!(broken.is_error, "{}", broken.text);
    // The appearance actions would write settings.toml, which the app won't overwrite while
    // it is malformed: one screenshot, in the current theme.
    app.screenshot("up12-settings-error")?;
    assert!(two.b.requests().is_empty(), "{:?}", two.b.requests());
    Ok(())
}

// covers UP-13 (M0: dark mode works; following macOS live is not automated, see above)
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn up_13_light_and_dark_appearance() -> Result<()> {
    let cluster = TestCluster::fake(Options::default())?;
    let app = App::launch(AppOptions::new(&cluster.kubeconfig))?;
    connect(&app, &cluster.context)?;
    assert_eq!(app.state()?["appearance"], "system");
    app.dispatch("flint::AppearanceDark", Value::Null)?;
    app.wait_for("/appearance", Equals("dark".into()))?;
    let stored = std::fs::read_to_string(app.config_dir().join("settings.toml"))?;
    assert!(stored.contains("appearance = \"dark\""), "{stored}");
    app.dispatch("flint::AppearanceLight", Value::Null)?;
    app.wait_for("/appearance", Equals("light".into()))?;
    app.dispatch("flint::AppearanceSystem", Value::Null)?;
    app.wait_for("/appearance", Equals("system".into()))?;
    drop(app);

    // FLINT_APPEARANCE forces a theme over the setting, for screenshots.
    let app = App::launch(AppOptions::new(&cluster.kubeconfig).env("FLINT_APPEARANCE", "dark"))?;
    connect(&app, &cluster.context)?;
    app.screenshot("up13-forced-dark")?;
    Ok(())
}
