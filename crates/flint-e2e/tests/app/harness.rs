//! The harness's own guarantees: the teardown checks (UP-90, UP-91) fail a test that breaks
//! them, through [`App::quit`] and through dropping the app.

use anyhow::Result;
use flint_e2e::app::{App, AppOptions};
use flint_e2e::fake_api::FakeApiServer;

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
        )) && error.contains("never selected \"fake-b\"")
            && error.contains("GET /version"),
        "{error}"
    );
    assert!(
        !error.contains(&two.a.url()),
        "fake-a was selected: {error}"
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
        message.contains("before the test selected \"fake-a\""),
        "{message}"
    );

    // The same steps through the harness's exceptions pass.
    let app = App::launch(AppOptions::new(two.kubeconfig()))?;
    app.wait_for("/kubeconfig/loaded", Equals(true.into()))?;
    app.edit_kubeconfig(|| {
        text.push_str("# changed again\n");
        write(&two.kubeconfig(), &text)
    })?;
    app.mark_selected(TwoClusters::B);
    contact(&two.b)?;
    connect(&app, TwoClusters::A)?;
    app.quit()?;
    Ok(())
}
