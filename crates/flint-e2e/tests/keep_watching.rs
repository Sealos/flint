//! Keeping a cluster watched in the background, against fake API servers and fake exec plugins:
//! a plugin runs only with consent bound to its definition, without a prompt, stops at its first
//! failure and dies with its watch, and a background cluster sends no write.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flint_core::handle::{Command, Event};
use flint_core::keep_watching::{
    BackgroundSignIn, KeepWatching, MAX_WATCHED_CLUSTERS, PluginOffer, WatchedCluster,
};
use flint_core::telemetry::Availability;
use flint_core::{Settings, settings};
use flint_e2e::fake_api::{FakeApiServer, Fault, Gvr, WatchStep};

#[allow(dead_code)]
mod core_support;

use core_support::{Core, WAIT};

const TOKEN: &str = "printf '{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
                     \"kind\":\"ExecCredential\",\"status\":{\"token\":\"t\"}}'\n";

/// A fake plugin running `body`, which records each run and what it was given.
fn plugin(dir: &Path, body: &str) -> (PathBuf, PathBuf) {
    let runs = dir.join("plugin-runs");
    let script = dir.join("plugin.sh");
    let text = format!(
        "#!/bin/sh\n\
         if [ -t 0 ]; then tty=tty; else tty=no-tty; fi\n\
         input=$(cat)\n\
         echo \"$$ $tty [$input] $KUBERNETES_EXEC_INFO\" >> '{}'\n{body}",
        runs.display()
    );
    std::fs::write(&script, text).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, runs)
}

fn run_lines(runs: &Path) -> Vec<String> {
    std::fs::read_to_string(runs)
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn exec_user(script: &Path, args: &[&str]) -> serde_json::Value {
    serde_json::json!({ "exec": {
        "apiVersion": "client.authentication.k8s.io/v1",
        "command": script.to_str().unwrap(),
        "args": args,
        "interactiveMode": "IfAvailable",
    }})
}

/// A kubeconfig with context `a` on `a` and context `b` on `b` with `user`.
fn config(a: &FakeApiServer, b: &FakeApiServer, user: serde_json::Value) -> String {
    let mut merged = serde_json::to_value(a.kubeconfig_named("a", Some("demo"))).unwrap();
    let mut other = serde_json::to_value(b.kubeconfig_named("b", Some("demo"))).unwrap();
    other["users"][0]["name"] = "b-user".into();
    other["users"][0]["user"] = user;
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
    a: FakeApiServer,
    b: FakeApiServer,
    file: PathBuf,
    earlier: PathBuf,
    script: PathBuf,
    runs: PathBuf,
}

async fn fixture(body: &str, settings: Settings) -> Fixture {
    let a = FakeApiServer::start().await.unwrap();
    let b = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (script, runs) = plugin(dir.path(), body);
    let file = dir.path().join("kubeconfig");
    std::fs::write(&file, config(&a, &b, exec_user(&script, &["get-token"]))).unwrap();
    let earlier = dir.path().join("earlier");
    std::fs::write(&earlier, "apiVersion: v1\nkind: Config\n").unwrap();
    let paths = vec![earlier.clone(), file.clone()];
    let core = Core::start_on_context(dir, paths, settings, "a").await;
    Fixture {
        core,
        a,
        b,
        file,
        earlier,
        script,
        runs,
    }
}

impl Fixture {
    async fn status(&self) -> KeepWatching {
        self.core.handle.keep_watching().await["b"].status
    }

    async fn until_status(&self, wanted: KeepWatching) {
        tokio::time::timeout(WAIT, async {
            while self.status().await != wanted {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never reached {wanted:?}"));
    }

    /// Turns on keep watching, and waits until the core has it.
    async fn keep(&self) {
        let definition = self.core.handle.keep_watching().await["b"].definition;
        self.core.handle.send(Command::SetKeepWatching {
            cluster: "b".to_owned(),
            definition,
        });
        tokio::time::timeout(WAIT, async {
            while self.status().await == KeepWatching::Off {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("keep watching should turn on");
    }

    /// What the background sign-in toggle's click does.
    async fn offer(&self) -> Result<PluginOffer, String> {
        let shown = self.core.handle.keep_watching().await["b"]
            .shown_sign_in
            .expect("a plugin definition");
        self.core.handle.offer_keep_watching("b", shown).await
    }

    /// Turns on keep watching and the background sign-in toggle.
    async fn turn_on(&self) {
        self.keep().await;
        let offer = self.offer().await.expect("the toggle can be turned on");
        self.core
            .handle
            .send(Command::ApproveKeepWatching(offer.approve()));
    }

    async fn until_runs(&self, count: usize) {
        tokio::time::timeout(WAIT, async {
            while run_lines(&self.runs).len() < count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the plugin should have run");
    }

    async fn until_feed(&self) {
        tokio::time::timeout(WAIT, async {
            while self.core.handle.telemetry("b").is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a feed should have started");
    }

    async fn rewrite(&self, user: serde_json::Value) {
        std::fs::write(&self.file, config(&self.a, &self.b, user)).unwrap();
        self.core.handle.reload_kubeconfig().await.unwrap();
    }
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(400)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_plugin_runs_without_consent() {
    let f = fixture(TOKEN, Settings::default()).await;
    settle().await;
    assert_eq!(f.status().await, KeepWatching::Off);
    assert!(run_lines(&f.runs).is_empty());
    assert!(f.core.handle.telemetry("b").is_none());
    assert!(f.b.requests().is_empty(), "nothing reached the cluster");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consented_plugin_runs_without_a_prompt_and_sends_no_write() {
    let f = fixture(TOKEN, Settings::default()).await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    let command = view.command.expect("the command to name");
    assert!(command.contains("plugin.sh get-token"), "{command}");

    f.turn_on().await;
    f.until_status(KeepWatching::Watching).await;
    f.until_runs(1).await;
    f.until_feed().await;

    let run = &run_lines(&f.runs)[0];
    assert!(run.contains("no-tty []"), "no terminal, empty stdin: {run}");
    assert!(run.contains("\"interactive\":false"), "{run}");
    settle().await;
    assert!(f.b.writes().is_empty(), "{:?}", f.b.writes());
    assert_eq!(run_lines(&f.runs).len(), 1, "one run for one token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_definition_change_voids_consent() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.turn_on().await;
    f.until_feed().await;
    let runs = run_lines(&f.runs).len();

    let mut changed_env = exec_user(&f.script, &["get-token"]);
    changed_env["exec"]["env"] = serde_json::json!([{"name": "AWS_PROFILE", "value": "other"}]);
    let mut changed_command = exec_user(&f.script, &["get-token"]);
    changed_command["exec"]["command"] = "/bin/echo".into();
    for user in [
        exec_user(&f.script, &["get-token", "--other"]),
        changed_env,
        changed_command,
    ] {
        f.rewrite(user).await;
        f.until_status(KeepWatching::NeedsConfirming).await;
        settle().await;
        assert!(f.core.handle.telemetry("b").is_none(), "its feed stopped");
        assert_eq!(run_lines(&f.runs).len(), runs, "the plugin did not run");
    }

    // Undoing the change does not bring the consent back: it has to be given again.
    f.rewrite(exec_user(&f.script, &["get-token"])).await;
    settle().await;
    assert_eq!(f.status().await, KeepWatching::NeedsConfirming);
    assert_eq!(run_lines(&f.runs).len(), runs);

    f.turn_on().await;
    f.until_status(KeepWatching::Watching).await;
    f.until_runs(runs + 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_confirmation_is_refused() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.keep().await;
    let offer = f.offer().await.unwrap();
    f.rewrite(exec_user(&f.script, &["get-token", "--moved"]))
        .await;
    f.core
        .handle
        .send(Command::ApproveKeepWatching(offer.approve()));
    settle().await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.sign_in, BackgroundSignIn::Off);
    assert_eq!(view.status, KeepWatching::NeedsConfirming);
    assert!(run_lines(&f.runs).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_plugin_stops_until_the_person_acts() {
    let f = fixture("exit 1\n", Settings::default()).await;
    f.turn_on().await;
    f.until_status(KeepWatching::SignInNeeded).await;
    settle().await;
    settle().await;
    assert_eq!(run_lines(&f.runs).len(), 1, "no retry");

    // Selecting it is acting on it: the next time it is in the background it may run once more.
    f.core.handle.send(Command::Activate("b".to_owned()));
    f.until_runs(2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_token_refresh_stops_without_a_retry() {
    // The first token is already due, so the next request runs the plugin again, which fails.
    let body = "if [ \"$(wc -l < \"$0.runs\" 2>/dev/null || echo 0)\" -gt 0 ]; then exit 1; fi\n\
                echo run > \"$0.runs\"\n\
                printf '{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
                \"kind\":\"ExecCredential\",\"status\":{\"token\":\"t\",\
                \"expirationTimestamp\":\"2020-01-01T00:00:00Z\"}}'\n";
    let f = fixture(body, Settings::default()).await;
    f.turn_on().await;
    f.until_status(KeepWatching::SignInNeeded).await;
    settle().await;
    settle().await;
    assert_eq!(run_lines(&f.runs).len(), 2, "one refresh, and no retry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_login_keeps_the_last_values_with_their_age() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.turn_on().await;
    f.until_feed().await;
    tokio::time::timeout(WAIT, async {
        loop {
            let read = f.core.handle.telemetry("b").map(|t| t.snapshot);
            if read.is_some_and(|s| {
                matches!(
                    s.availability,
                    Availability::Checked | Availability::Partial
                )
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the feed should have read the cluster");
    f.b.wait_for_watches(&Gvr::core("pods"), 1, WAIT)
        .await
        .unwrap();
    // Past the feed's event: what it published is what is kept.
    settle().await;

    f.b.inject(Fault::status(401));
    f.b.send_watch(&Gvr::core("pods"), WatchStep::End);
    f.until_status(KeepWatching::SignInNeeded).await;

    let held = f.core.handle.telemetry("b").expect("the last values stay");
    let Availability::CannotBeChecked(Some(failure)) = &held.snapshot.availability else {
        panic!("not marked: {:?}", held.snapshot.availability);
    };
    let said = failure.detail.to_string();
    assert!(said.contains("sign-in needed"), "{said}");
    assert!(said.contains("last values"), "{said}");
    assert!(said.contains("old"), "{said}");
    assert_eq!(run_lines(&f.runs).len(), 1, "a refusal runs nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_the_context_kills_a_running_plugin() {
    let f = fixture("sleep 60\n", Settings::default()).await;
    f.turn_on().await;
    f.until_runs(1).await;
    let pid: i32 = run_lines(&f.runs)[0]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "it is running");

    let only_a = serde_json::to_string(&f.a.kubeconfig_named("a", Some("demo"))).unwrap();
    std::fs::write(&f.file, only_a).unwrap();
    f.core.handle.reload_kubeconfig().await.unwrap();

    tokio::time::timeout(WAIT, async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plugin should have been killed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_it_off_kills_a_running_plugin() {
    let f = fixture("sleep 60\n", Settings::default()).await;
    f.turn_on().await;
    f.until_runs(1).await;
    let pid: i32 = run_lines(&f.runs)[0]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    f.core.handle.send(Command::SetKeepWatching {
        cluster: "b".to_owned(),
        definition: None,
    });
    tokio::time::timeout(WAIT, async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the plugin should have been killed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provisional_context_runs_no_plugin() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.turn_on().await;
    f.until_feed().await;
    let runs = run_lines(&f.runs).len();

    std::fs::write(&f.earlier, "not: [valid").unwrap();
    f.core.handle.reload_kubeconfig().await.unwrap();
    f.until_status(KeepWatching::Paused).await;
    settle().await;
    assert_eq!(run_lines(&f.runs).len(), runs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_thirty_third_cluster_is_refused() {
    let watched = (0..MAX_WATCHED_CLUSTERS)
        .map(|at| WatchedCluster {
            context: format!("other-{at}"),
            server: "https://other.example".to_owned(),
        })
        .collect();
    let f = fixture(
        TOKEN,
        Settings {
            watched_clusters: watched,
            ..Settings::default()
        },
    )
    .await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert!(view.refusal.as_deref().unwrap().contains("at most 32"));

    let refused = f.offer().await.unwrap_err();
    assert!(refused.contains("at most 32"), "{refused}");
    let events = f.core.handle.events();
    f.core.handle.send(Command::SetKeepWatching {
        cluster: "b".to_owned(),
        definition: view.definition,
    });
    let said = tokio::time::timeout(WAIT, async {
        loop {
            if let Ok(Event::Error(text)) = events.recv().await
                && text.contains("at most 32")
            {
                return text;
            }
        }
    })
    .await
    .expect("the refusal should be reported");
    assert!(said.contains("Turn one off"), "{said}");
    assert_eq!(f.status().await, KeepWatching::Off);
    assert!(run_lines(&f.runs).is_empty());
}

/// A token already past its expiry: every request runs the plugin again.
const EXPIRED: &str = "printf '{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
                       \"kind\":\"ExecCredential\",\"status\":{\"token\":\"t\",\
                       \"expirationTimestamp\":\"2020-01-01T00:00:00Z\"}}'\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consent_does_not_survive_a_restart() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.turn_on().await;
    f.until_feed().await;
    let runs = run_lines(&f.runs).len();
    let config_dir = f.core.dir.path().join("config");
    let saved = settings::load_settings(&config_dir).unwrap();
    assert_eq!(saved.watched_clusters.len(), 1, "the choice is kept");

    let Core { handle, dir: _kept } = f.core;
    drop(handle);
    let next = tempfile::tempdir().unwrap();
    let core =
        Core::start_on_context(next, vec![f.earlier.clone(), f.file.clone()], saved, "a").await;
    settle().await;
    assert_eq!(
        core.handle.keep_watching().await["b"].status,
        KeepWatching::NeedsConfirming
    );
    assert_eq!(
        run_lines(&f.runs).len(),
        runs,
        "nothing ran after a restart"
    );
    assert!(core.handle.telemetry("b").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_definition_alone_is_not_consent() {
    let f = fixture(TOKEN, Settings::default()).await;
    let definition = f.core.handle.keep_watching().await["b"].definition;
    f.core.handle.send(Command::SetKeepWatching {
        cluster: "b".to_owned(),
        definition,
    });
    settle().await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.status, KeepWatching::NeedsConfirming);
    assert_eq!(view.sign_in, BackgroundSignIn::Off, "off by default");
    assert!(
        run_lines(&f.runs).is_empty(),
        "no plugin without the toggle"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_the_sign_in_off_stops_the_watch_and_undoing_an_edit_keeps_it_off() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.turn_on().await;
    f.until_feed().await;
    f.core
        .handle
        .send(Command::RevokeBackgroundSignIn("b".to_owned()));
    f.until_status(KeepWatching::NeedsConfirming).await;
    settle().await;
    assert!(f.core.handle.telemetry("b").is_none(), "its watch stopped");
    let runs = run_lines(&f.runs).len();

    let offer = f.offer().await.unwrap();
    f.core
        .handle
        .send(Command::ApproveKeepWatching(offer.approve()));
    f.until_status(KeepWatching::Watching).await;
    f.until_runs(runs + 1).await;
    let runs = run_lines(&f.runs).len();
    f.rewrite(exec_user(&f.script, &["get-token", "--moved"]))
        .await;
    f.until_status(KeepWatching::NeedsConfirming).await;
    f.rewrite(exec_user(&f.script, &["get-token"])).await;
    settle().await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.sign_in, BackgroundSignIn::Off);
    assert_eq!(run_lines(&f.runs).len(), runs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_auth_provider_has_its_toggle_disabled() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.rewrite(serde_json::json!({
        "auth-provider": {"name": "oidc", "config": {}}
    }))
    .await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.sign_in, BackgroundSignIn::Unavailable);
    assert_eq!(view.shown_sign_in, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_namespace_change_voids_consent() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.turn_on().await;
    f.until_feed().await;
    let runs = run_lines(&f.runs).len();

    let mut edited: serde_json::Value =
        serde_json::from_str(&config(&f.a, &f.b, exec_user(&f.script, &["get-token"]))).unwrap();
    edited["contexts"][1]["context"]["namespace"] = "elsewhere".into();
    std::fs::write(&f.file, edited.to_string()).unwrap();
    f.core.handle.reload_kubeconfig().await.unwrap();

    f.until_status(KeepWatching::NeedsConfirming).await;
    settle().await;
    assert_eq!(run_lines(&f.runs).len(), runs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_503_from_version_does_not_run_the_plugin_again() {
    let f = fixture(EXPIRED, Settings::default()).await;
    f.b.inject(Fault::status(503));
    f.turn_on().await;
    f.until_status(KeepWatching::SignInNeeded).await;
    settle().await;
    // One to connect, and one for the token already due on the single probe.
    assert_eq!(run_lines(&f.runs).len(), 2, "the probe is sent once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_refreshed_token_stops_the_watch() {
    // Two good runs connect; from the third on the token can't be a header.
    let body = "n=$(cat \"$0.n\" 2>/dev/null || echo 0)\necho $((n+1)) > \"$0.n\"\n\
                if [ \"$n\" -ge 2 ]; then token='bad\\\\nvalue'; else token=t; fi\n\
                printf '{\"apiVersion\":\"client.authentication.k8s.io/v1\",\
                \"kind\":\"ExecCredential\",\"status\":{\"token\":\"'\"$token\"'\",\
                \"expirationTimestamp\":\"2020-01-01T00:00:00Z\"}}'\n";
    let f = fixture(body, Settings::default()).await;
    f.turn_on().await;
    f.until_status(KeepWatching::SignInNeeded).await;
    settle().await;
    let runs = run_lines(&f.runs).len();
    settle().await;
    settle().await;
    assert_eq!(run_lines(&f.runs).len(), runs, "no further launches");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_holds_when_the_settings_cannot_be_written() {
    let f = fixture("sleep 60\n", Settings::default()).await;
    f.turn_on().await;
    f.until_runs(1).await;
    let pid: i32 = run_lines(&f.runs)[0]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let config_dir = f.core.dir.path().join("config");
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    f.core.handle.send(Command::SetKeepWatching {
        cluster: "b".to_owned(),
        definition: None,
    });
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        while unsafe { libc::kill(pid, 0) } == 0 || f.status().await != KeepWatching::Off {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    stopped.expect("Stop should hold without a settings write");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_not_yet_loaded_launches_nothing() {
    let f = fixture(EXPIRED, Settings::default()).await;
    f.turn_on().await;
    f.until_feed().await;
    f.b.wait_for_watches(&Gvr::core("pods"), 1, WAIT)
        .await
        .unwrap();

    // On disk only: this core has no file watcher, so nothing reloads it.
    let edited = config(&f.a, &f.b, exec_user(&f.script, &["get-token", "--moved"]));
    std::fs::write(&f.file, edited).unwrap();
    settle().await;
    let runs = run_lines(&f.runs).len();

    // The watch comes back after the feed's backoff, and its request needs a new token.
    f.b.send_watch(&Gvr::core("pods"), WatchStep::End);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(run_lines(&f.runs).len(), runs, "the old definition ran");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_works_once() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.keep().await;
    let approval = f.offer().await.unwrap().approve();
    f.core
        .handle
        .send(Command::ApproveKeepWatching(approval.clone()));
    f.until_status(KeepWatching::Watching).await;
    f.until_runs(1).await;
    // Used again while its offer would still be there, if approving did not use it up.
    let events = f.core.handle.events();
    f.core
        .handle
        .send(Command::ApproveKeepWatching(approval.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(Event::Error(text)) = events.recv().await
                && text.contains("turn its background sign-in on again")
            {
                return;
            }
        }
    })
    .await
    .expect("a used approval is refused");

    f.core
        .handle
        .send(Command::RevokeBackgroundSignIn("b".to_owned()));
    f.until_status(KeepWatching::NeedsConfirming).await;
    let runs = run_lines(&f.runs).len();
    f.core.handle.send(Command::ApproveKeepWatching(approval));
    settle().await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.sign_in, BackgroundSignIn::Off);
    assert_eq!(run_lines(&f.runs).len(), runs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_offer_gives_nothing_to_approve_once_the_refusal_clears() {
    let f = fixture(TOKEN, Settings::default()).await;
    f.keep().await;
    std::fs::write(&f.earlier, "not: [valid").unwrap();
    f.core.handle.reload_kubeconfig().await.unwrap();
    assert!(f.offer().await.is_err());

    std::fs::write(&f.earlier, "apiVersion: v1\nkind: Config\n").unwrap();
    f.core.handle.reload_kubeconfig().await.unwrap();
    settle().await;
    let view = f.core.handle.keep_watching().await["b"].clone();
    assert_eq!(view.sign_in, BackgroundSignIn::Off);
    assert!(run_lines(&f.runs).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quitting_kills_a_running_plugin_and_starts_nothing() {
    let f = fixture("sleep 60\n", Settings::default()).await;
    f.turn_on().await;
    f.until_runs(1).await;
    let pid: i32 = run_lines(&f.runs)[0]
        .split(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap();

    tokio::time::timeout(Duration::from_secs(2), f.core.handle.stop_keep_watching())
        .await
        .expect("the wait is bounded");
    tokio::time::timeout(Duration::from_secs(2), async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the plugin should be gone");
    settle().await;
    assert_eq!(run_lines(&f.runs).len(), 1, "nothing started after");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_background_plugin_gets_no_display_or_browser() {
    let body = format!("echo \"[$DISPLAY][$WAYLAND_DISPLAY][$BROWSER]\" > \"$0.env\"\n{TOKEN}");
    let f = fixture(&body, Settings::default()).await;
    let mut user = exec_user(&f.script, &["get-token"]);
    user["exec"]["env"] = serde_json::json!([
        {"name": "DISPLAY", "value": ":0"},
        {"name": "WAYLAND_DISPLAY", "value": "wayland-0"},
        {"name": "BROWSER", "value": "firefox"},
    ]);
    f.rewrite(user).await;
    f.turn_on().await;
    f.until_runs(1).await;
    f.until_feed().await;
    let seen = std::fs::read_to_string(format!("{}.env", f.script.display())).unwrap();
    assert_eq!(seen.trim(), "[][][false]");
}

#[test]
fn the_setting_is_typed_and_capped_on_write() {
    let dir = tempfile::tempdir().unwrap();
    let entry = |at: usize| WatchedCluster {
        context: format!("c{at}"),
        server: "https://a.example".to_owned(),
    };
    for at in 0..MAX_WATCHED_CLUSTERS {
        settings::set_watched_cluster(dir.path(), &format!("c{at}"), Some(entry(at))).unwrap();
    }
    let refused = settings::set_watched_cluster(dir.path(), "extra", Some(entry(99)));
    assert!(refused.unwrap_err().to_string().contains("at most 32"));
    let kept = settings::set_watched_cluster(dir.path(), "c0", Some(entry(0))).unwrap();
    assert_eq!(kept.watched_clusters.len(), MAX_WATCHED_CLUSTERS);
    let removed = settings::set_watched_cluster(dir.path(), "c0", None).unwrap();
    assert_eq!(removed.watched_clusters.len(), MAX_WATCHED_CLUSTERS - 1);
}
