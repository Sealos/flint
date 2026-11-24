//! Fleet actions against fake API servers: each context is its own server, so what reached a
//! cluster is what that server recorded. No cluster needed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use flint_core::fleet::{
    FleetAction, FleetCancel, FleetManifest, FleetObject, FleetPlan, FleetPreview, FleetReport,
    FleetSkip, FleetStatus,
};
use flint_core::handle::{AdmittedMember, Command, MemberStatus};
use flint_core::settings::ClusterPin;
use flint_core::write::{ResourceChange, WriteError, WriteWarning};
use flint_core::{KindId, Settings, WriteRefusal};
use flint_e2e::fake_api::{
    FakeApiServer, Fault, Gvr, Options, RequestHold, Verb, write_kubeconfigs,
};
use serde_json::{Value, json};

#[allow(dead_code)]
mod core_support;

use core_support::{Core, WAIT};

const NAME: &str = "web";
const NAMES: [&str; 3] = ["a", "b", "c"];
const FLUX_LABEL: &str = "kustomize.toolkit.fluxcd.io/name";

fn deployments() -> Gvr {
    Gvr::new("apps", "v1", "deployments")
}

fn deployment(uid: &str, replicas: u32) -> Value {
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": NAME, "namespace": "demo", "uid": uid, "resourceVersion": "100"},
        "spec": {"replicas": replicas, "template": {"metadata": {}}},
    })
}

fn object() -> FleetObject {
    FleetObject {
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: NAME.to_owned(),
    }
}

fn live(server: &FakeApiServer) -> Value {
    server
        .object(&deployments(), Some("demo"), NAME)
        .expect("the object exists")
}

fn replicas(server: &FakeApiServer) -> u64 {
    live(server)["spec"]["replicas"].as_u64().unwrap()
}

/// The writes that were stored, not dry runs.
fn real_writes(server: &FakeApiServer) -> usize {
    server.writes().iter().filter(|w| !w.is_dry_run()).count()
}

fn all_writes(server: &FakeApiServer) -> usize {
    server.writes().len()
}

/// The writes the server accepted and stored.
fn stored_writes(server: &FakeApiServer) -> usize {
    server
        .writes()
        .iter()
        .filter(|w| !w.is_dry_run() && w.status.is_some_and(|code| code < 300))
        .count()
}

/// Holds the next PATCH that `action` sends to `server`, after its recheck has read the object.
fn hold_patch(server: &FakeApiServer, action: &FleetAction) -> RequestHold {
    let (fault, hold) = Fault::hold();
    let fault = fault.on(&deployments()).verb(Verb::Other).times(1);
    server.inject(match action {
        FleetAction::Scale { .. } => fault.subresource("scale"),
        _ => fault,
    });
    hold
}

/// A controller's change to the image, which leaves the uid and the replica count alone.
fn change_image(server: &FakeApiServer) -> Value {
    let mut changed = live(server);
    changed["metadata"]["resourceVersion"] = json!("101");
    changed["spec"]["template"]["spec"] = json!({"containers": [{"name": "app", "image": "next"}]});
    server.set_object(&deployments(), changed);
    live(server)
}

const ACTIONS: [FleetAction; 2] = [FleetAction::RolloutRestart, FleetAction::Scale { to: 5 }];

fn write_config(path: &Path, servers: &[&FakeApiServer]) {
    let entries: Vec<_> = servers
        .iter()
        .zip(NAMES)
        .map(|(server, name)| (*server, name, None))
        .collect();
    write_kubeconfigs(path, &entries).unwrap();
}

struct Fleet {
    core: Core,
    path: PathBuf,
    servers: [FakeApiServer; 3],
    members: Vec<AdmittedMember>,
}

impl Fleet {
    /// Three connected members, each with the object, unlocked except for `locked`.
    async fn start(locked: &[&str]) -> Fleet {
        Self::start_with(locked, Options::default()).await
    }

    /// [`Self::start`], on servers that also create objects.
    async fn start_creating(locked: &[&str]) -> Fleet {
        let options = Options {
            accept_creates: true,
            ..Options::default()
        };
        Self::start_with(locked, options).await
    }

    async fn start_with(locked: &[&str], options: Options) -> Fleet {
        let servers = [
            FakeApiServer::start_with(options.clone()).await.unwrap(),
            FakeApiServer::start_with(options.clone()).await.unwrap(),
            FakeApiServer::start_with(options).await.unwrap(),
        ];
        for (index, server) in servers.iter().enumerate() {
            server.set_object(&deployments(), deployment(&format!("uid-{index}"), 2));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kubeconfig");
        write_config(&path, &[&servers[0], &servers[1], &servers[2]]);
        let core = Core::start_on_context(dir, vec![path.clone()], Settings::default(), "a").await;
        let pins = NAMES
            .iter()
            .zip(&servers)
            .map(|(name, server)| ClusterPin::new(*name, server.url()))
            .collect();
        let members = core.handle.admit_running_set(pins).await.unwrap();
        for (name, server) in NAMES.iter().zip(&servers) {
            if !locked.contains(name) {
                core.handle.unlock(name, &server.url());
            }
        }
        let fleet = Fleet {
            core,
            path,
            servers,
            members,
        };
        fleet.until_connected().await;
        fleet
    }

    /// Waits until every background member has connected.
    async fn until_connected(&self) {
        tokio::time::timeout(WAIT, async {
            loop {
                let reports = self.core.handle.running_set().await;
                let ready = NAMES.iter().all(|name| {
                    reports
                        .iter()
                        .any(|r| r.cluster == *name && r.status == MemberStatus::Live)
                });
                if ready {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("every member connects");
    }

    async fn preview(&self, action: FleetAction) -> FleetPreview {
        let plan = FleetPlan::new(action, object(), self.members.clone()).unwrap();
        self.core.handle.preview_fleet(plan).await
    }

    async fn run(&self, preview: FleetPreview) -> FleetReport {
        self.core
            .handle
            .run_fleet(preview, FleetCancel::new(), |_| {})
            .await
    }

    fn server(&self, name: &str) -> &FakeApiServer {
        let index = NAMES.iter().position(|n| *n == name).unwrap();
        &self.servers[index]
    }

    /// Points context `b` at `server` and connects it there, unlocked, so a write that ignored
    /// its admission would have a working client to the new server.
    async fn repoint_b(&self, server: &FakeApiServer) {
        write_config(&self.path, &[&self.servers[0], server, &self.servers[2]]);
        let handle = &self.core.handle;
        handle.reload_kubeconfig().await.unwrap();
        handle.unlock("b", &server.url());
        let before = server.arrivals().len();
        handle.send(Command::Activate("a".to_owned()));
        handle.send(Command::Activate("b".to_owned()));
        tokio::time::timeout(WAIT, async {
            while server.arrivals().len() == before || handle.discovery("b").await.is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("b connects to the new server");
    }
}

/// Whether the run was refused for the member's connection, not for some other reason.
fn is_connection_refusal(status: &FleetStatus) -> bool {
    matches!(
        status,
        FleetStatus::Failed(
            WriteError::ConnectionChanged
                | WriteError::Refused(WriteRefusal::Moved { .. } | WriteRefusal::Stale { .. })
        )
    )
}

fn statuses(report: &FleetReport) -> Vec<FleetStatus> {
    report.results.iter().map(|r| r.status.clone()).collect()
}

const AFTER_FAILURE: FleetStatus = FleetStatus::Skipped(FleetSkip::AfterFailure);
const NOT_STARTED: FleetStatus = FleetStatus::Skipped(FleetSkip::NotStarted);

// covers UP-100: the dry run stores nothing on any cluster, and the run changes each in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scale_is_dry_run_on_every_cluster_then_applied_on_each() {
    let fleet = Fleet::start(&[]).await;

    let preview = fleet.preview(FleetAction::Scale { to: 5 }).await;
    for entry in preview.entries() {
        assert_eq!(entry.refusal(), None, "{}", entry.cluster());
        let server = fleet.server(entry.cluster());
        assert_eq!(replicas(server), 2, "a dry run stores nothing");
        assert_eq!(real_writes(server), 0);
        assert!(server.writes().iter().any(|w| w.is_dry_run()));
    }

    let report = fleet.run(preview).await;
    assert_eq!(statuses(&report), [const { FleetStatus::Done }; 3]);
    for server in &fleet.servers {
        assert_eq!(replicas(server), 5);
        assert_eq!(real_writes(server), 1, "sent once");
    }
}

// covers UP-100: a rollout restart reaches each cluster's pod template.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rollout_restart_runs_on_every_cluster() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::RolloutRestart).await;
    for server in &fleet.servers {
        assert!(
            live(server)["spec"]["template"]["metadata"]["annotations"].is_null(),
            "a dry run stores nothing"
        );
    }
    let report = fleet.run(preview).await;
    assert!(statuses(&report).iter().all(|s| *s == FleetStatus::Done));
    for server in &fleet.servers {
        assert!(
            live(server)["spec"]["template"]["metadata"]["annotations"]
                ["kubectl.kubernetes.io/restartedAt"]
                .is_string()
        );
    }
}

// covers UP-100: the first failure stops the run, and no request reaches a later cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_failure_stops_the_run_and_nothing_reaches_a_skipped_cluster() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::Scale { to: 5 }).await;
    let (b, c) = (fleet.server("b"), fleet.server("c"));
    b.inject(
        Fault::status(500)
            .on(&deployments())
            .verb(Verb::Other)
            .subresource("scale")
            .times(1),
    );
    let arrivals_at_c = c.arrivals().len();
    let writes_at_b = all_writes(b);

    let report = fleet.run(preview).await;

    let results = statuses(&report);
    assert_eq!(results[0], FleetStatus::Done);
    assert!(matches!(results[1], FleetStatus::Failed(_)), "{results:?}");
    assert_eq!(results[2], AFTER_FAILURE);
    assert_eq!(replicas(fleet.server("a")), 5);
    assert_eq!(replicas(b), 2);
    assert_eq!(replicas(c), 2);
    assert_eq!(
        all_writes(b),
        writes_at_b + 1,
        "a failed write is not retried"
    );
    let to_object = c.arrivals()[arrivals_at_c..]
        .iter()
        .filter(|r| r.path.contains(NAME))
        .count();
    assert_eq!(
        to_object, 0,
        "no request for the object reached the skipped cluster"
    );
    assert_eq!(real_writes(c), 0);
}

// covers UP-100: a member re-pointed after its preview is refused at its turn, nothing reaches
// either server for it, and the run stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_repointed_mid_run_is_refused_and_stops_the_run() {
    let fleet = Fleet::start(&[]).await;
    let elsewhere = FakeApiServer::start().await.unwrap();
    elsewhere.set_object(&deployments(), deployment("uid-elsewhere", 2));
    let preview = fleet.preview(FleetAction::Scale { to: 5 }).await;

    fleet.repoint_b(&elsewhere).await;

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert_eq!(results[0], FleetStatus::Done);
    assert!(is_connection_refusal(&results[1]), "{results:?}");
    assert_eq!(results[2], AFTER_FAILURE);
    assert_eq!(all_writes(&elsewhere), 0, "nothing reached the new server");
    assert_eq!(replicas(&elsewhere), 2);
    assert_eq!(real_writes(fleet.server("b")), 0);
    assert_eq!(real_writes(fleet.server("c")), 0);
}

// covers UP-100: a member moved away and back shows the same URL, but is not the definition
// admitted, so it is refused too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_repointed_and_back_is_refused() {
    let fleet = Fleet::start(&[]).await;
    let elsewhere = FakeApiServer::start().await.unwrap();
    elsewhere.set_object(&deployments(), deployment("uid-elsewhere", 2));
    let preview = fleet.preview(FleetAction::Scale { to: 5 }).await;

    fleet.repoint_b(&elsewhere).await;
    let original = fleet.server("b");
    let arrivals = original.arrivals().len();
    fleet.repoint_b(original).await;
    assert!(original.arrivals().len() > arrivals, "b reconnected");

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert!(is_connection_refusal(&results[1]), "{results:?}");
    assert_eq!(results[2], AFTER_FAILURE);
    assert_eq!(real_writes(original), 0);
    assert_eq!(all_writes(&elsewhere), 0);
}

// covers UP-100: with a kubeconfig that can no longer be read, the definition is provisional, so
// the first member is refused and nothing is written anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_provisional_definition_is_refused_and_stops_the_run() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::Scale { to: 5 }).await;
    std::fs::write(&fleet.path, "not: [a kubeconfig").unwrap();

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert!(
        matches!(
            &results[0],
            FleetStatus::Failed(WriteError::KubeconfigUnreadable | WriteError::ConnectionChanged)
        ),
        "{results:?}"
    );
    assert_eq!(results[1..], [AFTER_FAILURE, AFTER_FAILURE]);
    assert!(fleet.servers.iter().all(|s| real_writes(s) == 0));
}

// covers UP-100, UP-54: a locked member is refused by the dry run and gets no request; left in,
// it keeps the run from starting anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_member_gets_no_write_and_keeps_the_run_from_starting() {
    let fleet = Fleet::start(&["b"]).await;
    let preview = fleet.preview(FleetAction::Scale { to: 5 }).await;
    assert!(matches!(
        preview.entries()[1].refusal(),
        Some(WriteError::Refused(WriteRefusal::Protected { .. }))
    ));
    assert_eq!(all_writes(fleet.server("b")), 0, "not even a dry run");
    assert!(!preview.is_ready());

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert_eq!(results[0], NOT_STARTED);
    assert!(matches!(
        &results[1],
        FleetStatus::Failed(WriteError::Refused(WriteRefusal::Protected { .. }))
    ));
    assert_eq!(results[2], NOT_STARTED);
    assert!(fleet.servers.iter().all(|s| real_writes(s) == 0));
    assert_eq!(all_writes(fleet.server("b")), 0);
}

// covers UP-100, UP-54: a cluster locked after its dry run is refused: the preview grants nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_member_locked_after_its_dry_run_is_refused() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::Scale { to: 5 }).await;
    assert!(preview.entries().iter().all(|e| e.refusal().is_none()));
    fleet.core.handle.lock("b");

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert_eq!(results[0], FleetStatus::Done);
    assert!(matches!(
        &results[1],
        FleetStatus::Failed(WriteError::Refused(WriteRefusal::Protected { .. }))
    ));
    assert_eq!(results[2], AFTER_FAILURE);
    assert_eq!(real_writes(fleet.server("b")), 0);
    assert_eq!(real_writes(fleet.server("c")), 0);
}

// covers UP-100: a spec changed after the dry run makes the review stale, even for a restart,
// whose patch tests nothing but the uid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_review_is_refused_and_stops_the_run() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::RolloutRestart).await;
    let b = fleet.server("b");
    let mut changed = live(b);
    changed["spec"]["template"]["metadata"]["labels"] = json!({"release": "next"});
    b.set_object(&deployments(), changed);

    let report = fleet.run(preview).await;
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Failed(WriteError::Conflict),
            AFTER_FAILURE,
        ]
    );
    assert_eq!(real_writes(b), 0);
    assert_eq!(real_writes(fleet.server("c")), 0);
}

// covers UP-100: a status update alone does not make a review stale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_status_update_does_not_make_a_review_stale() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::RolloutRestart).await;
    let b = fleet.server("b");
    let mut updated = live(b);
    updated["status"] = json!({"observedGeneration": 2});
    b.set_object(&deployments(), updated);

    let report = fleet.run(preview).await;
    assert!(statuses(&report).iter().all(|s| *s == FleetStatus::Done));
}

// covers UP-100, #211: warnings are read from each cluster's live object.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warnings_come_from_each_live_object() {
    let fleet = Fleet::start(&[]).await;
    let b = fleet.server("b");
    let mut managed = live(b);
    managed["metadata"]["labels"] = json!({
        FLUX_LABEL: "apps",
        "kustomize.toolkit.fluxcd.io/namespace": "flux-system",
    });
    b.set_object(&deployments(), managed);

    let preview = fleet.preview(FleetAction::RolloutRestart).await;
    assert!(preview.entries()[0].warnings().is_empty());
    assert!(matches!(
        preview.entries()[1].warnings(),
        [WriteWarning::LikelyReverted(m)] if m.name == "apps"
    ));
}

// covers UP-100: a dropped cluster gets no write, and a cancel before the run sends none at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_cluster_and_a_cancelled_run_send_nothing() {
    let fleet = Fleet::start(&[]).await;
    let mut cancelled = fleet.preview(FleetAction::Scale { to: 5 }).await;
    assert!(cancelled.set_dropped("b", true));

    let cancel = FleetCancel::new();
    cancel.cancel();
    let report = fleet.core.handle.run_fleet(cancelled, cancel, |_| {}).await;
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Skipped(FleetSkip::Cancelled),
            FleetStatus::Skipped(FleetSkip::Dropped),
            FleetStatus::Skipped(FleetSkip::Cancelled),
        ]
    );
    assert!(fleet.servers.iter().all(|s| real_writes(s) == 0));

    let mut preview = fleet.preview(FleetAction::Scale { to: 5 }).await;
    assert!(preview.set_dropped("b", true));
    let report = fleet.run(preview).await;
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Skipped(FleetSkip::Dropped),
            FleetStatus::Done,
        ]
    );
    assert_eq!(real_writes(fleet.server("b")), 0);
}

// covers UP-100: a change that lands after the last read refuses the write and stops the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_just_before_the_patch_stops_the_run() {
    for action in ACTIONS {
        let fleet = Fleet::start(&[]).await;
        let preview = fleet.preview(action.clone()).await;
        let (b, c) = (fleet.server("b"), fleet.server("c"));
        let hold = hold_patch(b, &action);
        let run = tokio::spawn(
            fleet
                .core
                .handle
                .run_fleet(preview, FleetCancel::new(), |_| {}),
        );
        tokio::time::timeout(WAIT, hold.reached()).await.unwrap();
        let changed = change_image(b);
        hold.release();

        let report = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
        assert_eq!(
            statuses(&report),
            [
                FleetStatus::Done,
                FleetStatus::Failed(WriteError::Conflict),
                AFTER_FAILURE,
            ],
            "{action:?}"
        );
        assert_eq!(live(b), changed, "{action:?}");
        assert_eq!(stored_writes(b), 0, "{action:?}");
        assert_eq!(real_writes(c), 0, "{action:?}");
    }
}

// covers UP-100: the same change during a preview's dry run is that cluster's refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_just_before_the_dry_run_is_refused() {
    for action in ACTIONS {
        let fleet = Fleet::start(&[]).await;
        let b = fleet.server("b");
        let hold = hold_patch(b, &action);
        let plan = FleetPlan::new(action.clone(), object(), fleet.members.clone()).unwrap();
        let preview = tokio::spawn(fleet.core.handle.preview_fleet(plan));
        tokio::time::timeout(WAIT, hold.reached()).await.unwrap();
        let changed = change_image(b);
        hold.release();

        let preview = tokio::time::timeout(WAIT, preview).await.unwrap().unwrap();
        assert_eq!(
            preview.entries()[1].refusal(),
            Some(&WriteError::Conflict),
            "{action:?}"
        );
        assert_eq!(live(b), changed, "{action:?}");
        assert_eq!(stored_writes(b), 0, "{action:?}");
    }
}

// covers UP-100: a name or namespace that is not one path segment is refused before any request,
// so a preview can never reach the object without its dry run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_that_is_not_a_path_segment_sends_nothing() {
    let fleet = Fleet::start(&[]).await;
    let object_requests = |server: &FakeApiServer| {
        server
            .arrivals()
            .iter()
            .filter(|r| r.path.contains("/deployments/"))
            .count()
    };
    let before: Vec<_> = fleet.servers.iter().map(object_requests).collect();
    let named = |namespace: &str, name: &str| FleetObject {
        kind: KindId::new("apps", "deployments"),
        namespace: Some(namespace.to_owned()),
        name: name.to_owned(),
    };
    let mut objects: Vec<_> = ["web#x", "web?x=1", "web/scale", "web%23x", "..", "Web", ""]
        .into_iter()
        .map(|name| named("demo", name))
        .collect();
    objects.extend(
        ["demo#x", "demo?x=1", "demo/x", "demo.x"]
            .into_iter()
            .map(|namespace| named(namespace, NAME)),
    );
    for action in ACTIONS {
        for object in &objects {
            let plan = FleetPlan::new(action.clone(), object.clone(), fleet.members.clone());
            let refused = plan.is_err();
            if let Ok(plan) = plan {
                fleet.core.handle.preview_fleet(plan).await;
            }
            for server in &fleet.servers {
                assert_eq!(replicas(server), 2, "{object:?} changed the object");
                assert_eq!(real_writes(server), 0, "{object:?} lost its dry run");
            }
            assert!(refused, "{object:?}");
        }
    }
    let after: Vec<_> = fleet.servers.iter().map(object_requests).collect();
    assert_eq!(after, before, "no request for a refused name");
}

// covers UP-100: every write of a preview is a dry run, and no cluster's object changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preview_only_sends_dry_runs() {
    for action in ACTIONS {
        let fleet = Fleet::start(&[]).await;
        let before: Vec<_> = fleet.servers.iter().map(live).collect();
        let preview = fleet.preview(action.clone()).await;
        assert!(preview.entries().iter().all(|e| e.refusal().is_none()));
        for (server, before) in fleet.servers.iter().zip(&before) {
            let writes = server.writes();
            assert_eq!(writes.len(), 1, "{action:?}");
            assert!(writes[0].query.contains("dryRun=All"), "{action:?}");
            assert_eq!(live(server), *before, "{action:?}");
        }
    }
}

// covers UP-100: the plan the app confirms leaves a locked member out, so it gets no request and
// the others still run, each heard as it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_locked_member_left_out_of_the_plan_lets_the_others_run() {
    let fleet = Fleet::start(&["b"]).await;
    let mut preview = fleet.preview(FleetAction::Scale { to: 5 }).await;
    let will_run: Vec<bool> = preview.entries().iter().map(|e| e.will_run()).collect();
    assert_eq!(will_run, [true, false, true]);
    assert_eq!(preview.drop_refused(), ["b"]);

    let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let report = {
        let heard = heard.clone();
        fleet
            .core
            .handle
            .run_fleet(preview, FleetCancel::new(), move |result| {
                heard.lock().unwrap().push(result.clone())
            })
            .await
    };
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Skipped(FleetSkip::Dropped),
            FleetStatus::Done,
        ]
    );
    assert_eq!(*heard.lock().unwrap(), report.results);
    assert_eq!(all_writes(fleet.server("b")), 0, "not even a dry run");
    assert_eq!(replicas(fleet.server("b")), 2);
    assert_eq!(replicas(fleet.server("a")), 5);
    assert_eq!(replicas(fleet.server("c")), 5);
}

// covers UP-100: with a locked member left out and a failure on the next, the result says which
// member was refused, which failed and which was not tried, and nothing reaches the last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_partial_failure_after_a_refusal_reports_each_member() {
    let fleet = Fleet::start(&["a"]).await;
    let mut preview = fleet.preview(FleetAction::RolloutRestart).await;
    assert_eq!(preview.drop_refused(), ["a"]);
    let b = fleet.server("b");
    b.inject(
        Fault::status(500)
            .on(&deployments())
            .verb(Verb::Other)
            .times(1),
    );

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert_eq!(results[0], FleetStatus::Skipped(FleetSkip::Dropped));
    assert!(matches!(results[1], FleetStatus::Failed(_)), "{results:?}");
    assert_eq!(results[2], AFTER_FAILURE);
    assert_eq!(all_writes(fleet.server("a")), 0);
    assert_eq!(stored_writes(b), 0);
    assert_eq!(real_writes(fleet.server("c")), 0);
}

/// A manifest for `web` with `replicas` and a label, as one cluster's copy would carry it: with
/// that cluster's uid, version and status, which the others must not get.
fn manifest(replicas: u32) -> FleetAction {
    let yaml = format!(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {NAME}\n  namespace: demo\n  \
         uid: uid-elsewhere\n  resourceVersion: '999'\n  labels:\n    release: next\nspec:\n  \
         replicas: {replicas}\n  template:\n    metadata: {{}}\nstatus:\n  replicas: 7\n"
    );
    FleetAction::ApplyYaml {
        manifest: FleetManifest::new(yaml),
    }
}

// covers UP-101: one manifest is dry-run on every cluster, replacing the object where it exists
// and creating it where it does not, then applied on each in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_apply_is_dry_run_per_cluster_then_creates_or_replaces_on_each() {
    let fleet = Fleet::start_creating(&[]).await;
    let c = fleet.server("c");
    assert!(c.remove_object(&deployments(), Some("demo"), NAME));

    let preview = fleet.preview(manifest(4)).await;
    let creates: Vec<Option<bool>> = preview
        .entries()
        .iter()
        .map(|e| e.edit().map(|edit| edit.creates()))
        .collect();
    assert_eq!(creates, [Some(false), Some(false), Some(true)]);
    let a = &preview.entries()[0];
    assert!(a.edit().unwrap().after().contains("release: next"));
    assert!(a.edit().unwrap().before().contains("uid-0"));
    assert!(preview.is_ready());
    for server in &fleet.servers {
        assert_eq!(real_writes(server), 0, "a dry run stores nothing");
        assert_eq!(all_writes(server), 1);
    }
    assert!(c.object(&deployments(), Some("demo"), NAME).is_none());

    let report = fleet.run(preview).await;
    assert_eq!(statuses(&report), [const { FleetStatus::Done }; 3]);
    for (index, server) in fleet.servers.iter().enumerate() {
        let object = live(server);
        assert_eq!(object["spec"]["replicas"], 4);
        assert_eq!(object["metadata"]["labels"]["release"], "next");
        assert_ne!(object["metadata"]["uid"], "uid-elsewhere");
        if index < 2 {
            assert_eq!(
                object["metadata"]["uid"],
                format!("uid-{index}"),
                "replaced"
            );
        }
        assert_eq!(real_writes(server), 1, "sent once");
    }
}

// covers UP-101: a cluster whose dry run failed keeps the run from starting until it is left
// out; left out, it gets nothing and the others run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_apply_dry_run_blocks_the_run_until_it_is_left_out() {
    let fleet = Fleet::start_creating(&[]).await;
    let b = fleet.server("b");
    b.inject(
        Fault::status(422)
            .on(&deployments())
            .verb(Verb::Other)
            .times(1),
    );
    let preview = fleet.preview(manifest(4)).await;
    assert!(matches!(
        preview.entries()[1].refusal(),
        Some(WriteError::Rejected(_))
    ));
    assert_eq!(preview.unresolved(), ["b"]);
    assert!(!preview.is_ready());

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert_eq!(results[0], NOT_STARTED);
    assert!(matches!(results[1], FleetStatus::Failed(_)), "{results:?}");
    assert_eq!(results[2], NOT_STARTED);
    assert!(fleet.servers.iter().all(|s| real_writes(s) == 0));

    b.inject(
        Fault::status(422)
            .on(&deployments())
            .verb(Verb::Other)
            .times(1),
    );
    let mut preview = fleet.preview(manifest(4)).await;
    assert_eq!(preview.drop_refused(), ["b"]);
    let writes_at_b = all_writes(b);
    let report = fleet.run(preview).await;
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Skipped(FleetSkip::Dropped),
            FleetStatus::Done,
        ]
    );
    assert_eq!(
        all_writes(b),
        writes_at_b,
        "nothing reached the cluster left out"
    );
    assert_eq!(replicas(b), 2);
    assert_eq!(replicas(fleet.server("c")), 4);
}

// covers UP-101, UP-54: a locked member is refused an apply and a delete before any request,
// even a dry run, and the others run once it is left out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_protected_member_is_refused_an_apply_and_a_delete() {
    for action in [manifest(4), FleetAction::Delete] {
        let fleet = Fleet::start_creating(&["b"]).await;
        let b = fleet.server("b");
        let mut preview = fleet.preview(action.clone()).await;
        assert!(
            matches!(
                preview.entries()[1].refusal(),
                Some(WriteError::Refused(WriteRefusal::Protected { .. }))
            ),
            "{action:?}"
        );
        assert_eq!(all_writes(b), 0, "not even a dry run: {action:?}");
        assert_eq!(preview.drop_refused(), ["b"]);
        let report = fleet.run(preview).await;
        assert_eq!(
            statuses(&report),
            [
                FleetStatus::Done,
                FleetStatus::Skipped(FleetSkip::Dropped),
                FleetStatus::Done,
            ],
            "{action:?}"
        );
        assert_eq!(all_writes(b), 0, "{action:?}");
        assert_eq!(replicas(b), 2, "{action:?}");
        assert_eq!(live(b)["metadata"]["uid"], "uid-1", "{action:?}");
    }
}

// covers UP-101: the first failure of an apply stops the run, and the clusters after it are not
// tried and get no request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_failed_apply_stops_the_run() {
    let fleet = Fleet::start_creating(&[]).await;
    let preview = fleet.preview(manifest(4)).await;
    let (b, c) = (fleet.server("b"), fleet.server("c"));
    b.inject(
        Fault::status(500)
            .on(&deployments())
            .verb(Verb::Other)
            .times(1),
    );
    let arrivals_at_c = c.arrivals().len();
    let writes_at_b = all_writes(b);

    let report = fleet.run(preview).await;
    let results = statuses(&report);
    assert_eq!(results[0], FleetStatus::Done);
    assert!(matches!(results[1], FleetStatus::Failed(_)), "{results:?}");
    assert_eq!(results[2], AFTER_FAILURE);
    assert_eq!(replicas(fleet.server("a")), 4);
    assert_eq!(replicas(b), 2);
    assert_eq!(all_writes(b), writes_at_b + 1, "not retried");
    assert_eq!(c.arrivals().len(), arrivals_at_c, "nothing reached c");
    assert_eq!(replicas(c), 2);
}

// covers UP-101: an object created on a cluster after its review (where the apply was to create
// it) is a conflict: the create is refused and stops the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_object_created_after_the_review_stops_the_apply() {
    let fleet = Fleet::start_creating(&[]).await;
    let a = fleet.server("a");
    assert!(a.remove_object(&deployments(), Some("demo"), NAME));
    let preview = fleet.preview(manifest(4)).await;
    assert!(preview.entries()[0].edit().unwrap().creates());
    a.set_object(&deployments(), deployment("uid-new", 3));

    let report = fleet.run(preview).await;
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Failed(WriteError::Conflict),
            AFTER_FAILURE,
            AFTER_FAILURE,
        ]
    );
    assert_eq!(replicas(a), 3, "the object someone else made stays");
    assert!(fleet.servers.iter().all(|s| stored_writes(s) == 0));
}

// covers UP-101: an object changed after its apply review is a conflict at its turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_object_changed_after_the_review_stops_the_apply() {
    let fleet = Fleet::start_creating(&[]).await;
    let preview = fleet.preview(manifest(4)).await;
    let b = fleet.server("b");
    let changed = change_image(b);

    let report = fleet.run(preview).await;
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Failed(WriteError::Conflict),
            AFTER_FAILURE,
        ]
    );
    assert_eq!(live(b), changed);
    assert_eq!(replicas(fleet.server("c")), 2);
}

// covers UP-101: a delete is dry-run on every cluster, bound to each one's uid, then deletes
// each in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_is_dry_run_per_cluster_then_deletes_each() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::Delete).await;
    for entry in preview.entries() {
        assert_eq!(entry.refusal(), None, "{}", entry.cluster());
        assert_eq!(entry.change(), Some(&ResourceChange::Delete));
    }
    for server in &fleet.servers {
        let writes = server.writes();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].is_dry_run(), "{:?}", writes[0].query);
        assert!(server.object(&deployments(), Some("demo"), NAME).is_some());
    }

    let report = fleet.run(preview).await;
    assert_eq!(statuses(&report), [const { FleetStatus::Done }; 3]);
    for server in &fleet.servers {
        assert!(server.object(&deployments(), Some("demo"), NAME).is_none());
        assert_eq!(real_writes(server), 1, "sent once");
    }
}

/// A change made to a member's object between its review and the run.
type Change = fn(&FakeApiServer);

// covers UP-101: a member whose object changed (its spec or only its status), went away, or was
// deleted and made again since the review stops the delete there, and nothing is deleted after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_stops_where_the_object_changed_or_vanished_since_the_review() {
    let changes: [(&str, Change); 4] = [
        ("spec", |server| {
            change_image(server);
        }),
        ("status", |server| {
            let mut updated = live(server);
            updated["metadata"]["resourceVersion"] = json!("102");
            updated["status"] = json!({"observedGeneration": 2});
            server.set_object(&deployments(), updated);
        }),
        ("vanished", |server| {
            assert!(server.remove_object(&deployments(), Some("demo"), NAME));
        }),
        ("replaced", |server| {
            server.set_object(&deployments(), deployment("uid-again", 2));
        }),
    ];
    for (what, change) in changes {
        let fleet = Fleet::start(&[]).await;
        let preview = fleet.preview(FleetAction::Delete).await;
        let (b, c) = (fleet.server("b"), fleet.server("c"));
        change(b);
        let report = fleet.run(preview).await;
        let results = statuses(&report);
        assert_eq!(results[0], FleetStatus::Done, "{what}");
        assert!(
            matches!(
                results[1],
                FleetStatus::Failed(WriteError::Conflict | WriteError::Gone)
            ),
            "{what}: {results:?}"
        );
        assert_eq!(results[2], AFTER_FAILURE, "{what}");
        assert_eq!(real_writes(b), 0, "{what}");
        assert_eq!(real_writes(c), 0, "{what}");
        assert!(
            c.object(&deployments(), Some("demo"), NAME).is_some(),
            "{what}"
        );
    }
}

// covers UP-101: a change that lands between a delete's last read and its request is refused by
// the server: the request names the uid and version reviewed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_just_before_the_delete_stops_the_run() {
    let fleet = Fleet::start(&[]).await;
    let preview = fleet.preview(FleetAction::Delete).await;
    let (b, c) = (fleet.server("b"), fleet.server("c"));
    let (fault, hold) = Fault::hold();
    b.inject(fault.on(&deployments()).verb(Verb::Other).times(1));
    let run = tokio::spawn(
        fleet
            .core
            .handle
            .run_fleet(preview, FleetCancel::new(), |_| {}),
    );
    tokio::time::timeout(WAIT, hold.reached()).await.unwrap();
    let changed = change_image(b);
    hold.release();

    let report = tokio::time::timeout(WAIT, run).await.unwrap().unwrap();
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Failed(WriteError::Conflict),
            AFTER_FAILURE,
        ]
    );
    assert_eq!(live(b), changed);
    assert_eq!(stored_writes(b), 0);
    assert_eq!(real_writes(c), 0);
}
