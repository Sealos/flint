//! Keeping a cluster watched in the background, against fake API servers and fake exec plugins:
//! a plugin runs only with consent bound to its definition, without a prompt, stops at its first
//! failure and dies with its watch, and a background cluster sends no write.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flint_core::handle::{Command, Event};
use flint_core::keep_watching::{KeepWatching, MAX_WATCHED_CLUSTERS, WatchedCluster};
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

    async fn turn_on(&self) {
        let definition = self.core.handle.keep_watching().await["b"].definition;
        self.core.handle.send(Command::SetKeepWatching {
            cluster: "b".to_owned(),
            definition,
        });
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
    let saved = std::fs::read_to_string(f.core.dir.path().join("config/settings.toml")).unwrap();
    assert!(!saved.contains("plugin_consent"), "{saved}");

    f.turn_on().await;
    f.until_status(KeepWatching::Watching).await;
    f.until_runs(runs + 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_confirmation_is_refused() {
    let f = fixture(TOKEN, Settings::default()).await;
    let shown = f.core.handle.keep_watching().await["b"].definition;
    f.rewrite(exec_user(&f.script, &["get-token", "--moved"]))
        .await;
    f.core.handle.send(Command::SetKeepWatching {
        cluster: "b".to_owned(),
        definition: shown,
    });
    settle().await;
    assert_eq!(f.status().await, KeepWatching::Off);
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
            plugin_consent: None,
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

    let events = f.core.handle.events();
    f.turn_on().await;
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

#[test]
fn the_setting_is_typed_and_capped_on_write() {
    let dir = tempfile::tempdir().unwrap();
    let entry = |at: usize| WatchedCluster {
        context: format!("c{at}"),
        server: "https://a.example".to_owned(),
        plugin_consent: None,
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
