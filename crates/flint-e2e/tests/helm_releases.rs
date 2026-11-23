//! Helm releases through a fake `helm` on a real core and a fake API server: every run is held to
//! the connection admitted, and a plugin credential never reaches `helm`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use flint_core::Settings;
use flint_core::helm::{HelmError, HelmOutput, HelmQuery};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer};

#[allow(dead_code)]
mod core_support;

use core_support::Core;

const LIST: &str = "[{\"name\":\"web\",\"namespace\":\"apps\",\"revision\":\"2\",\
                    \"status\":\"deployed\",\"chart\":\"web-1.0.0\",\"app_version\":\"1\",\
                    \"updated\":\"2026-11-23\"}]";

/// A fake `helm` printing [`LIST`], which counts its runs and keeps the kubeconfig it was given.
fn fake_helm(dir: &Path) -> PathBuf {
    let path = dir.join("fake-helm");
    let script = format!(
        "#!/bin/sh\nprintf x >> '{d}/runs'\n/bin/cat \"$KUBECONFIG\" > '{d}/seen'\nprintf '%s' \
         '{LIST}'\n",
        d = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn runs(dir: &Path) -> usize {
    std::fs::read(dir.join("runs")).map_or(0, |bytes| bytes.len())
}

async fn started(server: &FakeApiServer) -> (Core, tempfile::TempDir, PathBuf) {
    let scripts = tempfile::tempdir().unwrap();
    let helm = fake_helm(scripts.path());
    let settings = Settings {
        helm: Some(helm),
        ..Settings::default()
    };
    let core = Core::start_with(server, Some("demo"), settings).await;
    let file = core.dir.path().join("kubeconfig");
    (core, scripts, file)
}

#[tokio::test]
async fn a_run_uses_only_the_admitted_context() {
    let server = FakeApiServer::start().await.unwrap();
    let (core, scripts, _) = started(&server).await;
    let pin = core.handle.helm_pin(CONTEXT).await.unwrap();
    let output = core
        .handle
        .helm_read(CONTEXT, &pin, HelmQuery::List)
        .await
        .unwrap();
    let HelmOutput::Releases { releases, .. } = output else {
        panic!("a list");
    };
    assert_eq!(releases[0].name, "web");
    let seen: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scripts.path().join("seen")).unwrap()).unwrap();
    assert_eq!(seen["current-context"], CONTEXT);
    assert_eq!(seen["contexts"].as_array().unwrap().len(), 1);
    assert_eq!(seen["clusters"][0]["cluster"]["server"], server.url());
}

#[tokio::test]
async fn a_repointed_context_starts_no_run() {
    let server = FakeApiServer::start().await.unwrap();
    let elsewhere = FakeApiServer::start().await.unwrap();
    let (core, scripts, file) = started(&server).await;
    let pin = core.handle.helm_pin(CONTEXT).await.unwrap();

    elsewhere.write_kubeconfig(&file, Some("demo")).unwrap();
    core.handle.reload_kubeconfig().await.unwrap();

    let refused = core.handle.helm_read(CONTEXT, &pin, HelmQuery::List).await;
    assert_eq!(refused, Err(HelmError::Changed));
    assert_eq!(runs(scripts.path()), 0, "helm never started");

    // Put back, the same URL is still not the definition the view was opened on.
    server.write_kubeconfig(&file, Some("demo")).unwrap();
    core.handle.reload_kubeconfig().await.unwrap();
    let refused = core.handle.helm_read(CONTEXT, &pin, HelmQuery::List).await;
    assert_eq!(refused, Err(HelmError::Changed));
    assert_eq!(runs(scripts.path()), 0, "helm never started");
}

// A query already running on the old server ends when the context moves.
#[tokio::test]
async fn a_repoint_ends_a_running_query() {
    let server = FakeApiServer::start().await.unwrap();
    let elsewhere = FakeApiServer::start().await.unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let helm = scripts.path().join("slow-helm");
    let pid_file = scripts.path().join("pid");
    std::fs::write(
        &helm,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}.new'\n/bin/mv '{0}.new' '{0}'\nexec /bin/sleep 25\n",
            pid_file.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&helm, std::fs::Permissions::from_mode(0o755)).unwrap();
    let settings = Settings {
        helm: Some(helm),
        ..Settings::default()
    };
    let core = Core::start_with(&server, Some("demo"), settings).await;
    let file = core.dir.path().join("kubeconfig");
    let pin = core.handle.helm_pin(CONTEXT).await.unwrap();
    let read = tokio::spawn(core.handle.helm_read(CONTEXT, &pin, HelmQuery::List));
    while !pid_file.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let pid = std::fs::read_to_string(&pid_file).unwrap();

    elsewhere.write_kubeconfig(&file, Some("demo")).unwrap();
    core.handle.reload_kubeconfig().await.unwrap();

    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), read)
        .await
        .expect("the run ends with the re-point")
        .unwrap();
    assert_eq!(ended, Err(HelmError::Changed));
    let mut alive = true;
    for _ in 0..200 {
        alive = std::process::Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!alive, "helm was stopped");
}

#[tokio::test]
async fn another_user_on_the_same_server_starts_no_run() {
    let server = FakeApiServer::start().await.unwrap();
    let (core, scripts, file) = started(&server).await;
    let pin = core.handle.helm_pin(CONTEXT).await.unwrap();

    let mut config = serde_json::to_value(server.kubeconfig(Some("demo"))).unwrap();
    config["users"][0]["user"]["token"] = "another-identity".into();
    std::fs::write(&file, config.to_string()).unwrap();
    core.handle.reload_kubeconfig().await.unwrap();

    let refused = core.handle.helm_read(CONTEXT, &pin, HelmQuery::List).await;
    assert_eq!(refused, Err(HelmError::Changed));
    assert_eq!(runs(scripts.path()), 0, "helm never started");
}

#[tokio::test]
async fn an_exec_plugin_credential_is_refused_and_never_runs() {
    let server = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("plugin");
    let ran = dir.path().join("plugin-ran");
    std::fs::write(
        &plugin,
        format!(
            "#!/bin/sh\nprintf x >> '{}'\nprintf '{{\"apiVersion\":\"client.authentication.k8s.io/\
             v1\",\"kind\":\"ExecCredential\",\"status\":{{\"token\":\"t\"}}}}'\n",
            ran.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = serde_json::to_value(server.kubeconfig(Some("demo"))).unwrap();
    config["users"][0]["user"] = serde_json::json!({ "exec": {
        "apiVersion": "client.authentication.k8s.io/v1",
        "command": plugin.to_str().unwrap(),
        "interactiveMode": "Never",
    }});
    let file = dir.path().join("kubeconfig");
    std::fs::write(&file, config.to_string()).unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let settings = Settings {
        helm: Some(fake_helm(scripts.path())),
        ..Settings::default()
    };
    let core = Core::start_on(dir, vec![file], settings).await;
    let plugin_runs = std::fs::read(&ran).map_or(0, |b| b.len());

    let pin = core.handle.helm_pin(CONTEXT).await.unwrap();
    assert_eq!(
        core.handle.helm_read(CONTEXT, &pin, HelmQuery::List).await,
        Err(HelmError::CredentialRunsCommand)
    );
    assert_eq!(runs(scripts.path()), 0, "helm never started");
    assert_eq!(
        std::fs::read(&ran).map_or(0, |b| b.len()),
        plugin_runs,
        "asking started no plugin"
    );
}

#[tokio::test]
async fn a_missing_helm_is_said_plainly() {
    let server = FakeApiServer::start().await.unwrap();
    let settings = Settings {
        helm: Some("/nonexistent/helm".into()),
        ..Settings::default()
    };
    let core = Core::start_with(&server, Some("demo"), settings).await;
    let pin = core.handle.helm_pin(CONTEXT).await.unwrap();
    assert_eq!(
        core.handle.helm_read(CONTEXT, &pin, HelmQuery::List).await,
        Err(HelmError::BadConfiguredPath)
    );
}
