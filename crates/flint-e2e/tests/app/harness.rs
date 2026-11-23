//! The harness's own guarantees: the teardown checks (UP-90, UP-91) fail a test that breaks
//! them, through [`App::quit`] and through dropping the app.

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use flint_e2e::app::{App, AppOptions};
use flint_e2e::fake_api::{FakeApiServer, Fault, kubeconfig, write_kubeconfigs};
use kube::config::{ExecConfig, Kubeconfig};

use crate::support::*;

/// One request to `server`, as the app would send it.
fn contact(server: &FakeApiServer) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async {
        server.client().await?.apiserver_version().await?;
        Ok(())
    })
}

// covers UP-90, UP-91
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn teardown_checks_fail_a_test_that_breaks_them() -> Result<()> {
    let two = TwoClusters::start()?;

    // Quit: a kubeconfig changed behind the app's back, and a request to a cluster nobody
    // selected, are both reported.
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    connect(&app, TwoClusters::A)?;
    let mut text = std::fs::read_to_string(two.kubeconfig())?;
    text.push_str("# changed\n");
    write(&two.kubeconfig(), &text)?;
    contact(&two.b)?;
    let error = app.quit().expect_err("the teardown checks to fail");
    let error = format!("{error:#}");
    assert!(error.contains("app e2e teardown checks failed"), "{error}");
    assert!(
        error.contains("UP-90: the kubeconfig") && error.contains("changed during the test"),
        "{error}"
    );
    assert!(
        error.contains(&format!(
            "UP-91: 1 request(s) reached the fake server at {}",
            two.b.url()
        )) && error.contains("never selected \"kind-fake-b\"")
            && error.contains("GET /version"),
        "{error}"
    );
    assert!(
        !error.contains(&two.a.url()),
        "kind-fake-a was selected: {error}"
    );

    // Drop: a request before the test selected the cluster fails the test with a panic.
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    contact(&two.a)?;
    connect(&app, TwoClusters::A)?;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(app)))
        .expect_err("dropping the app to fail the test");
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(
        message.contains("before the test selected \"kind-fake-a\""),
        "{message}"
    );

    // The same steps through the harness's exceptions pass.
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    app.edit_kubeconfig([two.kubeconfig()], || {
        text.push_str("# changed again\n");
        write(&two.kubeconfig(), &text)
    })?;
    app.mark_selected(TwoClusters::B);
    contact(&two.b)?;
    connect(&app, TwoClusters::A)?;
    app.quit()?;
    Ok(())
}

/// The message of the panic dropping `app` raises, failing if it raises none.
fn drop_panics(app: App) -> String {
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(app)))
        .expect_err("dropping the app to fail the test");
    panic.downcast_ref::<String>().cloned().unwrap_or_default()
}

// With no session running Quit would end the app and close the dialog unnoticed.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn teardown_fails_a_test_that_left_a_dialog_open() -> Result<()> {
    let two = TwoClusters::start()?;
    let leave_dialog = || -> Result<App> {
        let app = App::launch(AppOptions::new(two.kubeconfig()))?;
        app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
        app.dispatch(
            "workspace::ExposeToAi",
            serde_json::json!({"cluster": TwoClusters::A}),
        )?;
        app.wait_for("/dialog/title", Exists(true))?;
        Ok(app)
    };

    let error = format!(
        "{:#}",
        leave_dialog()?
            .quit()
            .expect_err("the teardown checks to fail")
    );
    assert!(error.contains("unexpected dialog open"), "{error}");

    let message = drop_panics(leave_dialog()?);
    assert!(message.contains("unexpected dialog open"), "{message}");

    // A test already failing keeps its own error: dropping the app does not panic over it.
    let app = leave_dialog()?;
    app.mark_failing();
    drop(app);
    Ok(())
}

/// Records its process id and its child's, then both wait. They give up after a minute, so a
/// failing test leaves nothing running for long.
const HANGING_PLUGIN: &str = r#"#!/bin/sh
dir=$(dirname "$0")
sleep 60 </dev/null >/dev/null 2>&1 &
echo $! > "$dir/child.tmp" && mv "$dir/child.tmp" "$dir/child"
echo $$ > "$dir/plugin.tmp" && mv "$dir/plugin.tmp" "$dir/plugin"
wait
"#;

/// Whether a process with this id still runs (a zombie does not count: nothing is left to end).
fn alive(pid: i32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|ps| {
            let stat = String::from_utf8_lossy(&ps.stdout);
            !stat.trim().is_empty() && !stat.trim().starts_with('Z')
        })
}

// A dialog found while a credential plugin hangs still fails the test, and neither the plugin
// nor what it started outlives the app.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn teardown_with_a_dialog_ends_a_hanging_plugin() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let two = TwoClusters::start()?;
    let dir = two.scratch.path().join("plugin");
    std::fs::create_dir(&dir)?;
    let program = dir.join("flint-e2e-hanging-plugin");
    std::fs::write(&program, HANGING_PLUGIN)?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
    let mut config = Kubeconfig::read_from(two.kubeconfig())?;
    let user = config
        .auth_infos
        .iter_mut()
        .find(|user| user.name == TwoClusters::A)
        .and_then(|user| user.auth_info.as_mut())
        .context("kind-fake-a's user")?;
    user.exec = Some(serde_json::from_value::<ExecConfig>(serde_json::json!({
        "apiVersion": "client.authentication.k8s.io/v1beta1",
        "command": program,
        "interactiveMode": "Never",
    }))?);
    kubeconfig::write(&two.kubeconfig(), &config)?;

    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    app.select_cluster(TwoClusters::A)?;
    let pid = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
    };
    let plugin = until("the plugin to start", || pid("plugin"), Option::is_some)?
        .context("the plugin's id")?;
    let child =
        until("its child to start", || pid("child"), Option::is_some)?.context("the child's id")?;
    app.dispatch(
        "workspace::ExposeToAi",
        serde_json::json!({"cluster": TwoClusters::A}),
    )?;
    app.wait_for("/dialog/title", Exists(true))?;

    let error = format!("{:#}", app.quit().expect_err("the teardown checks to fail"));
    assert!(error.contains("unexpected dialog open"), "{error}");
    until("the plugin to end", || alive(plugin), |running| !running)?;
    until("its child to end", || alive(child), |running| !running)?;
    Ok(())
}

// covers UP-91: a request is counted when it arrives, not when a delayed answer leaves.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn teardown_counts_a_delayed_request_from_its_arrival() -> Result<()> {
    let two = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    // Sent before the selection, answered after it.
    two.a.inject(Fault::delay(Duration::from_secs(3)).times(1));
    std::thread::scope(|scope| -> Result<()> {
        let pending = scope.spawn(|| contact(&two.a));
        until(
            "the delayed request to arrive",
            || two.a.arrivals().len(),
            |n| *n == 1,
        )?;
        assert!(two.a.requests().is_empty(), "not answered yet");
        connect(&app, TwoClusters::A)?;
        pending.join().expect("the request thread")?;
        Ok(())
    })?;
    let message = drop_panics(app);
    assert!(
        message.contains("before the test selected \"kind-fake-a\"")
            && message.contains("GET /version"),
        "{message}"
    );
    Ok(())
}

// covers UP-90, UP-91: a kubeconfig file settings add is checked from before the app reads it.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn teardown_checks_kubeconfig_files_added_through_settings() -> Result<()> {
    let two = TwoClusters::start()?;
    let only_a = two.scratch.path().join("only-a");
    write_kubeconfigs(&only_a, &[(&two.a, TwoClusters::A, None)])?;
    let app = App::launch(AppOptions::new(&only_a))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    app.write_settings(&format!(
        "kubeconfig_paths = [{:?}]\n",
        two.kubeconfig().display().to_string()
    ))?;
    app.wait_for(
        "/clusters",
        Any(serde_json::json!({"name": TwoClusters::B})),
    )?;
    contact(&two.b)?;
    let mut text = std::fs::read_to_string(two.kubeconfig())?;
    text.push_str("# changed\n");
    write(&two.kubeconfig(), &text)?;
    let error = format!("{:#}", app.quit().expect_err("the teardown checks to fail"));
    assert!(
        error.contains(&format!(
            "UP-91: 1 request(s) reached the fake server at {}",
            two.b.url()
        )) && error.contains("never selected \"kind-fake-b\""),
        "{error}"
    );
    assert!(
        error.contains(&format!(
            "UP-90: the kubeconfig {}",
            two.kubeconfig().display()
        )),
        "{error}"
    );
    Ok(())
}

// A request that failed on the way, and was handled, does not make the teardown lenient; only
// `App::mark_failing` does.
#[test]
#[ignore = "app e2e: needs macOS with a GUI session (see docs/TESTING.md)"]
fn teardown_stays_fatal_after_a_handled_timeout() -> Result<()> {
    let two = TwoClusters::start()?;
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    let started = Instant::now();
    let timeout = app.wait_for_within(
        "/active_cluster",
        Equals("no-such-cluster".into()),
        Duration::from_millis(300),
    );
    assert_eq!(timeout.expect_err("a timeout").code, "timeout");
    assert!(started.elapsed() < Duration::from_secs(20));
    contact(&two.b)?;
    let message = drop_panics(app);
    assert!(
        message.contains("never selected \"kind-fake-b\""),
        "{message}"
    );

    // Marked as failing: the report is printed, and the test's own error stands.
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    contact(&two.b)?;
    app.mark_failing();
    drop(app);
    Ok(())
}
