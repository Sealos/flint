//! The fleet action's plan and results (#705, UP-100): who acts, who is left out and why, the one
//! confirmation naming the members, and each member's result, a partial failure included. The
//! whole flow against two local API servers is in `workspace_sets_tests.rs`.

use flint_core::KindId;
use flint_core::fleet::{
    FleetAction, FleetCancel, FleetObject, FleetResult, FleetSkip, FleetStatus,
};
use flint_core::protection::WriteRefusal;
use flint_core::write::WriteError;
use gpui_kit::{AppContext as _, TestAppContext};

use super::*;
use crate::ui::fleet_view::FleetRunView;

fn object() -> FleetObject {
    FleetObject {
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: "web".to_owned(),
    }
}

fn acts(cluster: &str, change: &str) -> PlanLine {
    PlanLine {
        cluster: cluster.to_owned(),
        shown: cluster.to_owned(),
        verdict: Verdict::Acts {
            change: vec![change.to_owned()],
            warnings: Vec::new(),
        },
    }
}

fn left_out(cluster: &str, why: &str) -> PlanLine {
    PlanLine {
        cluster: cluster.to_owned(),
        shown: cluster.to_owned(),
        verdict: Verdict::Refused(why.to_owned()),
    }
}

fn order(clusters: &[&str]) -> Vec<String> {
    clusters.iter().map(|c| (*c).to_owned()).collect()
}

// covers UP-100: a member left out before the preview is refused with its reason, and one the
// preview did not answer for is refused too: nothing is sent for what was not reviewed.
#[test]
fn members_left_out_or_not_reviewed_are_refused() {
    let refused = vec![(
        "prod".to_owned(),
        "prod is protected (read-only in Flint) until you unlock it".to_owned(),
    )];
    let lines = plan_lines(&order(&["prod", "dev"]), &refused, None, |c| {
        format!("{c} (shown)")
    });
    assert_eq!(
        lines,
        [
            PlanLine {
                cluster: "prod".into(),
                shown: "prod (shown)".into(),
                verdict: Verdict::Refused(
                    "prod is protected (read-only in Flint) until you unlock it".into()
                ),
            },
            PlanLine {
                cluster: "dev".into(),
                shown: "dev (shown)".into(),
                verdict: Verdict::Refused("it was not reviewed".into()),
            },
        ]
    );
    assert!(lines.iter().all(|l| !l.acts()));
}

// covers UP-100: the one confirmation lists every member, says why one is left out, and keeps
// the members it runs on and the object on show.
#[test]
fn the_confirmation_names_the_members_it_runs_on() {
    let lines = [
        acts("alpha", "Replicas: 2 to 5."),
        left_out(
            "prod",
            "prod is protected (read-only in Flint) until you unlock it",
        ),
        acts("beta", "Replicas: 3 to 5."),
    ];
    let (dialog, pinned) = plan_dialog(&FleetAction::Scale { to: 5 }, &object(), &lines);
    assert_eq!(dialog.title, "Scale web to 5 on 2 of 3 clusters?");
    assert_eq!(dialog.ok, "Scale on 2 Clusters");
    assert_eq!(dialog.cancel, "Cancel");
    let text: Vec<&str> = dialog.text.lines().collect();
    assert!(text.contains(&"alpha: Replicas: 2 to 5."), "{text:?}");
    assert!(
        text.contains(
            &"prod: left out: prod is protected (read-only in Flint) until you unlock it"
        ),
        "{text:?}"
    );
    assert_eq!(pinned, 3);
    assert_eq!(
        text[text.len() - pinned..],
        [
            "Runs on: alpha, beta",
            "Namespace: demo",
            "Object: deployments.apps web",
        ]
    );
}

#[test]
fn one_member_is_named_in_the_singular() {
    let lines = [acts("alpha", "Replaces its pods.")];
    let (dialog, _) = plan_dialog(&FleetAction::RolloutRestart, &object(), &lines);
    assert_eq!(dialog.title, "Restart web on 1 of 1 cluster?");
    assert_eq!(dialog.ok, "Restart on 1 Cluster");
}

// covers UP-100: with every member refused there is nothing to confirm: only why, and Close.
#[test]
fn with_no_member_to_act_on_there_is_nothing_to_confirm() {
    let lines = [left_out("prod", "locked"), left_out("dev", "not connected")];
    let (dialog, pinned) = plan_dialog(&FleetAction::RolloutRestart, &object(), &lines);
    assert_eq!(dialog.title, "Nothing to run for web");
    assert_eq!(dialog.ok, "Close");
    assert!(dialog.cancel.is_empty());
    assert_eq!(pinned, 0);
    assert!(
        dialog.text.contains("prod: left out: locked"),
        "{}",
        dialog.text
    );
    assert!(!dialog.text.contains("Runs on"), "{}", dialog.text);
}

fn result(cluster: &str, status: FleetStatus) -> FleetResult {
    FleetResult {
        cluster: cluster.to_owned(),
        status,
    }
}

fn texts(lines: &[ResultLine], finished: bool) -> Vec<String> {
    let now = current(lines, finished);
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}: {}", l.shown, outcome_text(&l.outcome, now == Some(i))))
        .collect()
}

// covers UP-100: a partial failure says which member was done, which failed and why, which was
// not tried, and which was left out from the start.
#[test]
fn a_partial_failure_reports_each_member() {
    let plan = [
        acts("a", "x"),
        left_out("locked", "locked is protected"),
        acts("b", "x"),
        acts("c", "x"),
    ];
    let mut lines = result_lines(&plan);
    assert_eq!(
        texts(&lines, false),
        [
            "a: running…",
            "locked: left out: locked is protected",
            "b: waiting",
            "c: waiting",
        ]
    );
    record(&mut lines, &result("a", FleetStatus::Done));
    record(
        &mut lines,
        &result("locked", FleetStatus::Skipped(FleetSkip::Dropped)),
    );
    assert_eq!(texts(&lines, false)[2], "b: running…");
    record(
        &mut lines,
        &result("b", FleetStatus::Failed(WriteError::Conflict)),
    );
    record(
        &mut lines,
        &result("c", FleetStatus::Skipped(FleetSkip::AfterFailure)),
    );
    assert_eq!(
        texts(&lines, true),
        [
            "a: done",
            "locked: left out: locked is protected",
            "b: failed: the object changed since it was opened; reload it and review again",
            "c: not tried: an earlier cluster failed",
        ]
    );
    assert_eq!(
        summary(&lines),
        "Finished: 1 done, 1 failed, 1 not tried, 1 left out."
    );
}

#[test]
fn a_refused_turn_and_a_stopped_run_say_so() {
    let mut lines = result_lines(&[acts("a", "x"), acts("b", "x")]);
    let refusal = WriteError::Refused(WriteRefusal::Protected {
        cluster: "a".to_owned(),
    });
    record(&mut lines, &result("a", FleetStatus::Failed(refusal)));
    record(
        &mut lines,
        &result("b", FleetStatus::Skipped(FleetSkip::Cancelled)),
    );
    let said = texts(&lines, true);
    assert!(said[0].starts_with("a: failed: "), "{said:?}");
    assert_eq!(said[1], "b: not tried: the run was stopped");
    // A result for a cluster the plan does not have changes nothing.
    let before = lines.clone();
    record(&mut lines, &result("elsewhere", FleetStatus::Done));
    assert_eq!(lines, before);
}

// covers UP-101: an apply's or a delete's failed dry runs are shown first, and the run cannot
// start until they are left out; restart and scale leave them out on their own.
#[test]
fn failed_dry_runs_are_shown_before_the_plan() {
    let lines = [
        left_out(
            "prod",
            "the edit changes the object's name, which cannot be changed here",
        ),
        acts("dev", "Creates it."),
    ];
    assert_eq!(
        failed_dialog(&FleetAction::Delete, &object(), &lines, 0),
        None
    );
    let dialog = failed_dialog(&FleetAction::Delete, &object(), &lines, 1).unwrap();
    assert_eq!(
        dialog.title,
        "The dry run of deleting web failed on 1 of 2 clusters"
    );
    assert!(
        dialog.text.contains("Nothing runs until"),
        "{}",
        dialog.text
    );
    assert!(dialog.text.contains("prod: failed: the edit changes"));
    assert!(dialog.text.contains("dev: Creates it."));
    assert_eq!(dialog.ok, "Leave Them Out");
    assert!(FleetChoice::ApplyYaml.drops_by_hand());
    assert!(FleetChoice::Delete.drops_by_hand());
    assert!(!FleetChoice::RolloutRestart.drops_by_hand());
    assert!(!FleetChoice::Scale.drops_by_hand());
}

// covers UP-101: an apply's and a delete's confirmation and results say what they do.
#[test]
fn apply_and_delete_are_worded_as_such() {
    let lines = [acts("dev", "Changes it: 1 line added, 2 lines removed.")];
    let apply = FleetAction::ApplyYaml {
        manifest: flint_core::fleet::FleetManifest::new("kind: Deployment"),
    };
    let (dialog, _) = plan_dialog(&apply, &object(), &lines);
    assert_eq!(dialog.title, "Apply YAML to web on 1 of 1 cluster?");
    assert_eq!(dialog.ok, "Apply on 1 Cluster");
    let (dialog, _) = plan_dialog(&FleetAction::Delete, &object(), &lines);
    assert_eq!(dialog.title, "Delete web on 1 of 1 cluster?");
    assert_eq!(dialog.ok, "Delete on 1 Cluster");
    assert!(dialog.default_cancel, "Enter cancels a delete");
    let (dialog, _) = plan_dialog(&apply, &object(), &lines);
    assert!(!dialog.default_cancel);
    assert_eq!(
        run_dialog(&FleetAction::Delete, &object(), 2).title,
        "Deleting web on 2 clusters"
    );
    assert_eq!(running_write(&apply), RunningWrite::YamlEdit);
    assert_eq!(
        running_write(&FleetAction::Delete),
        RunningWrite::Action(ResourceAction::Delete)
    );
}

#[test]
fn an_apply_says_how_much_it_changes() {
    assert_eq!(diff_lines("a: 1\n", "a: 1\n"), ["No change."]);
    assert_eq!(
        diff_lines("a: 1\nb: 2\n", "a: 3\n"),
        ["Changes it: 1 line added, 2 lines removed."]
    );
}

#[test]
fn a_run_that_did_not_start_says_why() {
    let mut lines = result_lines(&[acts("a", "x")]);
    record(
        &mut lines,
        &result("a", FleetStatus::Skipped(FleetSkip::NotStarted)),
    );
    assert_eq!(
        texts(&lines, true),
        ["a: not tried: a failed cluster was not left out"]
    );
}

// covers UP-100: the results show each member as it ends, and closing them stops the run before
// its next member.
#[gpui_kit::test]
async fn closing_the_results_stops_the_run(cx: &mut TestAppContext) {
    let cancel = FleetCancel::new();
    let lines = result_lines(&[acts("a", "x"), acts("b", "x")]);
    let view = cx.new(|_| FleetRunView::new(lines, cancel.clone()));
    view.update(cx, |view, cx| {
        view.heard(&result("a", FleetStatus::Done), cx)
    });
    view.read_with(cx, |view, _| {
        assert_eq!(view.texts(), ["a: done", "b: running…"]);
    });
    assert!(!cancel.is_cancelled());
    drop(view);
    // Released at the end of the next update.
    cx.update(|_| {});
    assert!(cancel.is_cancelled());
}
