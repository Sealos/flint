//! Signing a cluster in once, against fake API servers and a fake exec plugin: a running-set
//! member from the set view, and a cluster kept watching from its fleet home card. The click
//! runs the plugin once, the cluster comes back live, and nothing is kept that would run it
//! again. A click on a view that no longer shows the definition runs nothing.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flint_core::Settings;
use flint_core::handle::{AdmittedMember, Command, MemberStatus};
use flint_core::keep_watching::{BackgroundSignIn, KeepWatching};
use flint_core::settings::ClusterPin;
use flint_e2e::fake_api::FakeApiServer;

#[allow(dead_code)]
mod core_support;

use core_support::{Core, WAIT};

/// A token that never needs a refresh.
const LASTING: &str = "2099-01-01T00:00:00Z";
/// A token already due: the first request runs the plugin again.
const EXPIRED: &str = "2020-01-01T00:00:00Z";

/// A fake plugin that notes each run in `plugin-runs` and answers with a token expiring at
/// `expires`.
fn plugin(dir: &Path, expires: &str) -> (PathBuf, PathBuf) {
    let runs = dir.join("plugin-runs");
    let script = dir.join("plugin.sh");
    let text = format!(
        "#!/bin/sh\necho run >> '{}'\n\
         printf '{{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
         \"kind\":\"ExecCredential\",\"status\":{{\"token\":\"t\",\
         \"expirationTimestamp\":\"{expires}\"}}}}'\n",
        runs.display()
    );
    std::fs::write(&script, text).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, runs)
}

fn runs(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or_default()
}

/// A kubeconfig with context `a` on `a`, and `b` on `b` signing in with `script`.
fn config(a: &FakeApiServer, b: &FakeApiServer, script: &Path) -> String {
    let mut merged = serde_json::to_value(a.kubeconfig_named("a", Some("demo"))).unwrap();
    let mut other = serde_json::to_value(b.kubeconfig_named("b", Some("demo"))).unwrap();
    other["users"][0]["name"] = "b-user".into();
    other["users"][0]["user"] = serde_json::json!({ "exec": {
        "apiVersion": "client.authentication.k8s.io/v1",
        "command": script.to_str().unwrap(),
        "interactiveMode": "IfAvailable",
    }});
    other["clusters"][0]["name"] = "b-cluster".into();
    other["contexts"][0]["context"]["user"] = "b-user".into();
    other["contexts"][0]["context"]["cluster"] = "b-cluster".into();
    for key in ["users", "clusters", "contexts"] {
        let entry = other[key][0].clone();
        merged[key].as_array_mut().unwrap().push(entry);
    }
    merged.to_string()
}

struct Fixture {
    core: Core,
    b: FakeApiServer,
    runs: PathBuf,
    member: AdmittedMember,
    _a: FakeApiServer,
}

/// A core on `a` with the set of `a` and `b` open, `b` waiting for a sign-in.
async fn fixture(expires: &str) -> Fixture {
    let a = FakeApiServer::start().await.unwrap();
    let b = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (script, runs) = plugin(dir.path(), expires);
    let file = dir.path().join("kubeconfig");
    std::fs::write(&file, config(&a, &b, &script)).unwrap();
    let core = Core::start_on_context(dir, vec![file], Settings::default(), "a").await;
    let opened = core
        .handle
        .open_running_set(vec![
            ClusterPin::new("a", a.url()),
            ClusterPin::new("b", b.url()),
        ])
        .await
        .expect("the set opens");
    let member = opened
        .members
        .into_iter()
        .find(|m| m.context() == "b")
        .expect("b is admitted");
    let f = Fixture {
        core,
        b,
        runs,
        member,
        _a: a,
    };
    f.until_status(|s| matches!(s, MemberStatus::SignInNeeded { .. }))
        .await;
    f
}

impl Fixture {
    async fn until_status(&self, wanted: impl Fn(&MemberStatus) -> bool) {
        tokio::time::timeout(WAIT, async {
            loop {
                let reports = self.core.handle.running_set().await;
                if reports
                    .iter()
                    .any(|r| r.cluster == "b" && wanted(&r.status))
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("b should reach the status");
    }

    /// What the member's Sign in click sends.
    async fn click(&self, shown: impl Fn(Option<u64>) -> Option<u64>) -> Result<(), String> {
        let view = self.core.handle.keep_watching().await["b"].clone();
        self.core
            .handle
            .sign_in_member(&self.member, view.definition, shown(view.shown_sign_in))
            .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_signs_a_member_in_once() {
    let f = fixture(LASTING).await;
    assert_eq!(runs(&f.runs), 0, "nothing runs before the click");
    assert!(f.b.requests().is_empty(), "nothing reached b");

    f.click(|shown| shown).await.expect("b can sign in");
    f.until_status(|s| *s == MemberStatus::Live).await;

    assert_eq!(runs(&f.runs), 1);
    assert!(!f.b.requests().is_empty(), "b is read again");
    assert!(f.b.writes().is_empty(), "{:?}", f.b.writes());
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.sign_in, BackgroundSignIn::Off, "no background sign-in");
    assert_eq!(view.status, KeepWatching::Off);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(runs(&f.runs), 1, "nothing ran it again");
    assert!(f.click(|shown| shown).await.is_err(), "b no longer waits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_on_a_changed_view_runs_nothing() {
    let f = fixture(LASTING).await;

    let refused = f.click(|shown| shown.map(|s| s ^ 1)).await;

    assert!(refused.unwrap_err().contains("changed"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(runs(&f.runs), 0);
    assert!(f.b.requests().is_empty(), "nothing reached b");
    f.until_status(|s| matches!(s, MemberStatus::SignInNeeded { .. }))
        .await;
}

// A token that is due needs the plugin again, which the click did not agree to: the start is
// refused, nothing retries it, and the member waits for a sign-in again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_due_token_is_not_refreshed_in_the_background() {
    let f = fixture(EXPIRED).await;

    f.click(|shown| shown).await.expect("b can sign in");
    tokio::time::timeout(WAIT, async {
        while runs(&f.runs) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plugin should have run");
    f.until_status(|s| matches!(s, MemberStatus::SignInNeeded { .. }))
        .await;

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(runs(&f.runs), 1, "run once, and no retry");
    f.until_status(|s| matches!(s, MemberStatus::SignInNeeded { .. }))
        .await;
}

// One sign-in of a cluster kept watching, from its fleet home card.

/// A core on `a` with `b` kept watching and no background sign-in consent: its card needs a
/// sign-in, and nothing ran.
struct Watched {
    core: Core,
    b: FakeApiServer,
    runs: PathBuf,
    _a: FakeApiServer,
}

async fn watched(expires: &str) -> Watched {
    let a = FakeApiServer::start().await.unwrap();
    let b = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (script, runs) = plugin(dir.path(), expires);
    let file = dir.path().join("kubeconfig");
    std::fs::write(&file, config(&a, &b, &script)).unwrap();
    let core = Core::start_on_context(dir, vec![file], Settings::default(), "a").await;
    let definition = core.handle.keep_watching().await["b"].definition;
    core.handle.send(Command::SetKeepWatching {
        cluster: "b".to_owned(),
        definition,
    });
    let w = Watched {
        core,
        b,
        runs,
        _a: a,
    };
    w.until_status(KeepWatching::NeedsConfirming).await;
    w
}

impl Watched {
    async fn until_status(&self, wanted: KeepWatching) {
        tokio::time::timeout(WAIT, async {
            while self.core.handle.keep_watching().await["b"].status != wanted {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("b should reach {wanted:?}"));
    }

    /// What the card's Sign in click sends.
    async fn click(&self, shown: impl Fn(Option<u64>) -> Option<u64>) -> Result<(), String> {
        let view = self.core.handle.keep_watching().await["b"].clone();
        let server = self.b.url();
        self.core
            .handle
            .sign_in_watched("b", &server, view.definition, shown(view.shown_sign_in))
            .await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watched_cluster_signs_in_once_from_its_card() {
    let w = watched(LASTING).await;
    assert_eq!(runs(&w.runs), 0, "nothing runs before the click");
    assert!(w.b.requests().is_empty(), "nothing reached b");

    w.click(|shown| shown).await.expect("b can sign in");
    w.until_status(KeepWatching::Watching).await;
    tokio::time::timeout(WAIT, async {
        while w.core.handle.telemetry("b").is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("b's card has a feed");

    assert_eq!(runs(&w.runs), 1);
    assert!(w.b.writes().is_empty(), "{:?}", w.b.writes());
    let view = w.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.sign_in, BackgroundSignIn::Off, "no background sign-in");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(runs(&w.runs), 1, "nothing ran it again");
    assert!(w.click(|shown| shown).await.is_err(), "b no longer waits");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_on_a_changed_card_runs_nothing() {
    let w = watched(LASTING).await;

    let refused = w.click(|shown| shown.map(|s| s ^ 1)).await;

    assert!(refused.unwrap_err().contains("changed"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(runs(&w.runs), 0);
    assert!(w.b.requests().is_empty(), "nothing reached b");
    w.until_status(KeepWatching::NeedsConfirming).await;
}

// A token that is due needs the plugin again, which the click did not agree to: the start is
// refused, nothing retries it, and the card needs a sign-in again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watched_clusters_due_token_is_not_refreshed_in_the_background() {
    let w = watched(EXPIRED).await;

    w.click(|shown| shown).await.expect("b can sign in");
    tokio::time::timeout(WAIT, async {
        while runs(&w.runs) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plugin should have run");
    w.until_status(KeepWatching::NeedsConfirming).await;

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(runs(&w.runs), 1, "run once, and no retry");
    assert_eq!(
        w.core.handle.keep_watching().await["b"].status,
        KeepWatching::NeedsConfirming
    );
}
