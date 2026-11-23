use std::cell::RefCell;

use super::*;

fn step(
    cluster: &str,
    dropped: bool,
    outcome: Result<(), WriteError>,
) -> Step<(String, Result<(), WriteError>)> {
    Step {
        cluster: cluster.to_owned(),
        dropped,
        work: (cluster.to_owned(), outcome),
    }
}

/// Runs `steps`, and returns the report and the clusters whose work started.
async fn driven(
    steps: Vec<Step<(String, Result<(), WriteError>)>>,
    cancel: FleetCancel,
    cancel_after: Option<&str>,
) -> (FleetReport, Vec<String>) {
    let started = RefCell::new(Vec::new());
    let report = drive(
        steps,
        cancel.clone(),
        |_| {},
        |(cluster, outcome)| {
            started.borrow_mut().push(cluster.clone());
            if cancel_after == Some(cluster.as_str()) {
                cancel.cancel();
            }
            async move { outcome }
        },
    )
    .await;
    (report, started.into_inner())
}

fn statuses(report: &FleetReport) -> Vec<FleetStatus> {
    report.results.iter().map(|r| r.status.clone()).collect()
}

#[tokio::test]
async fn the_first_failure_stops_the_run() {
    let error = WriteError::Conflict;
    let (report, started) = driven(
        vec![
            step("a", false, Ok(())),
            step("b", false, Err(error.clone())),
            step("c", false, Ok(())),
        ],
        FleetCancel::new(),
        None,
    )
    .await;
    assert_eq!(started, ["a", "b"]);
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Failed(error),
            FleetStatus::Skipped(FleetSkip::AfterFailure),
        ]
    );
}

#[tokio::test]
async fn a_cancel_takes_effect_before_the_next_cluster() {
    let (report, started) = driven(
        vec![
            step("a", false, Ok(())),
            step("b", false, Ok(())),
            step("c", false, Ok(())),
        ],
        FleetCancel::new(),
        Some("a"),
    )
    .await;
    assert_eq!(started, ["a"]);
    assert_eq!(
        statuses(&report),
        [
            FleetStatus::Done,
            FleetStatus::Skipped(FleetSkip::Cancelled),
            FleetStatus::Skipped(FleetSkip::Cancelled),
        ]
    );
}

#[tokio::test]
async fn a_dropped_cluster_is_never_started_and_does_not_stop_the_run() {
    let (report, started) = driven(
        vec![
            step("a", true, Err(WriteError::Conflict)),
            step("b", false, Ok(())),
        ],
        FleetCancel::new(),
        None,
    )
    .await;
    assert_eq!(started, ["b"]);
    assert_eq!(
        statuses(&report),
        [FleetStatus::Skipped(FleetSkip::Dropped), FleetStatus::Done]
    );
}

#[test]
fn a_plan_needs_targets() {
    let object = FleetObject {
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: "web".to_owned(),
    };
    assert_eq!(
        FleetPlan::new(FleetAction::RolloutRestart, object, Vec::new()).unwrap_err(),
        FleetPlanError::NoTargets
    );
}

#[test]
fn a_plan_needs_a_plain_name_and_namespace() {
    let plan = |namespace: &str, name: &str| {
        let object = FleetObject {
            kind: KindId::new("apps", "deployments"),
            namespace: Some(namespace.to_owned()),
            name: name.to_owned(),
        };
        FleetPlan::new(FleetAction::RolloutRestart, object, Vec::new()).unwrap_err()
    };
    for name in [
        "web#x", "web?x=1", "web/x", "web%23", "..", "-web", "Web", "",
    ] {
        assert_eq!(plan("demo", name), FleetPlanError::InvalidObject, "{name}");
    }
    for namespace in ["demo#x", "demo?x=1", "demo/x", "demo.x", ""] {
        assert_eq!(
            plan(namespace, "web"),
            FleetPlanError::InvalidObject,
            "{namespace}"
        );
    }
    assert_eq!(plan("demo", "web.v2-1"), FleetPlanError::NoTargets);
}

#[test]
fn each_action_is_its_write_proposal() {
    assert_eq!(
        FleetAction::RolloutRestart.proposal(),
        WriteProposal::RolloutRestart
    );
    assert_eq!(
        FleetAction::Scale { to: 3 }.proposal(),
        WriteProposal::Scale { to: 3 }
    );
}
