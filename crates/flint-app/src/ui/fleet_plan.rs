//! What a fleet action on a cluster set says (#705, #450, UP-100, UP-101): the per-member plan
//! its one confirmation shows, and the per-member results as the run goes.
//!
//! Only wording and bookkeeping: what runs, and where, is decided by the core's fleet engine
//! ([`flint_core::fleet`]) and the write gate it asks for every member. A member this module
//! calls refused is never sent, and one it calls acting is still refused at its turn if the gate
//! says so.

use flint_core::display::single_line;
use flint_core::fleet::{
    FleetAction, FleetObject, FleetPreview, FleetResult, FleetSkip, FleetStatus,
};
use flint_core::write::{ResourceChange, ReviewedEdit, WriteTarget};
use gpui_kit::Action;

use super::confirm::{self, DialogText};
use super::registry::{ApplyAcrossSet, DeleteAcrossSet, RestartAcrossSet, ScaleAcrossSet};
use super::resource_actions::{ResourceAction, RunningWrite, error_text};
use super::write_confirm::{change_lines, warning_text};

/// Which fleet action was asked for, before a replica count or a manifest is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum FleetChoice {
    RolloutRestart,
    Scale,
    ApplyYaml,
    Delete,
}

impl FleetChoice {
    /// The single-cluster action it matches, which says which kinds offer it. Apply YAML has
    /// none: it is an edit.
    pub(in crate::ui) fn resource_action(self) -> Option<ResourceAction> {
        match self {
            FleetChoice::RolloutRestart => Some(ResourceAction::RolloutRestart),
            FleetChoice::Scale => Some(ResourceAction::Scale),
            FleetChoice::Delete => Some(ResourceAction::Delete),
            FleetChoice::ApplyYaml => None,
        }
    }

    /// How a refusal starts.
    pub(in crate::ui) fn cant(self) -> &'static str {
        match self.resource_action() {
            Some(action) => action.cant(),
            None => "Can't apply YAML across the set",
        }
    }

    /// The row menu's entry. Each opens a dialog first.
    pub(in crate::ui) fn label(self) -> &'static str {
        match self {
            FleetChoice::RolloutRestart => "Rollout Restart Across Set…",
            FleetChoice::Scale => "Scale Across Set…",
            FleetChoice::ApplyYaml => "Apply YAML Across Set…",
            FleetChoice::Delete => "Delete Across Set…",
        }
    }

    /// The registered action.
    pub(in crate::ui) fn boxed(self) -> Box<dyn Action> {
        match self {
            FleetChoice::RolloutRestart => Box::new(RestartAcrossSet),
            FleetChoice::Scale => Box::new(ScaleAcrossSet),
            FleetChoice::ApplyYaml => Box::new(ApplyAcrossSet),
            FleetChoice::Delete => Box::new(DeleteAcrossSet),
        }
    }

    /// Whether a member whose review or dry run failed must be left out by hand before the run
    /// can start, rather than being left out on its own. Apply and delete: what each member
    /// gets differs, so a failure is shown first.
    pub(in crate::ui) fn drops_by_hand(self) -> bool {
        matches!(self, FleetChoice::ApplyYaml | FleetChoice::Delete)
    }
}

impl From<&FleetAction> for FleetChoice {
    fn from(action: &FleetAction) -> Self {
        match action {
            FleetAction::Scale { .. } => FleetChoice::Scale,
            FleetAction::ApplyYaml { .. } => FleetChoice::ApplyYaml,
            FleetAction::Delete => FleetChoice::Delete,
            _ => FleetChoice::RolloutRestart,
        }
    }
}

/// What claims each member's object while `action` runs there.
pub(in crate::ui) fn running_write(action: &FleetAction) -> RunningWrite {
    match FleetChoice::from(action).resource_action() {
        Some(resource) => RunningWrite::Action(resource),
        None => RunningWrite::YamlEdit,
    }
}

/// What an apply does on one member, from its server dry run: creates the object, changes it
/// (how many lines), or leaves it as it is.
pub(in crate::ui) fn edit_lines(edit: &ReviewedEdit) -> Vec<String> {
    if edit.creates() {
        return vec!["Creates it.".to_owned()];
    }
    diff_lines(edit.before(), edit.after())
}

/// How `after` differs from `before`, in lines added and removed.
fn diff_lines(before: &str, after: &str) -> Vec<String> {
    let diff = similar::TextDiff::configure()
        .timeout(std::time::Duration::from_secs(1))
        .diff_lines(before, after);
    let count = |tag| diff.iter_all_changes().filter(|c| c.tag() == tag).count();
    let added = count(similar::ChangeTag::Insert);
    let removed = count(similar::ChangeTag::Delete);
    if added == 0 && removed == 0 {
        return vec!["No change.".to_owned()];
    }
    vec![format!(
        "Changes it: {} added, {} removed.",
        line_count(added),
        line_count(removed)
    )]
}

/// "1 line", "3 lines".
fn line_count(n: usize) -> String {
    match n {
        1 => "1 line".to_owned(),
        n => format!("{n} lines"),
    }
}

/// What a resource action does on one member.
fn action_lines(change: &ResourceChange) -> Vec<String> {
    match change {
        ResourceChange::Delete => vec!["Deletes it. This cannot be undone.".to_owned()],
        change => change_lines(change),
    }
}

/// What the plan says about one member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) enum Verdict {
    /// It will be asked for, in its turn: what changes there, and the warnings from its live
    /// object.
    Acts {
        change: Vec<String>,
        warnings: Vec<String>,
    },
    /// It is left out and gets no request, and why.
    Refused(String),
}

/// One member's line of the plan, in run order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) struct PlanLine {
    pub cluster: String,
    /// As the cluster is called on screen.
    pub shown: String,
    pub verdict: Verdict,
}

impl PlanLine {
    pub(in crate::ui) fn acts(&self) -> bool {
        matches!(self.verdict, Verdict::Acts { .. })
    }
}

/// The plan for every member of the set, in the set's order. `refused` are the members left
/// out before the preview, with why; every other member is read from `preview`, whose own
/// refusals are dropped from its run already. A member in neither is refused: nothing is sent
/// for what was not reviewed.
pub(in crate::ui) fn plan_lines(
    order: &[String],
    refused: &[(String, String)],
    preview: Option<&FleetPreview>,
    shown: impl Fn(&str) -> String,
) -> Vec<PlanLine> {
    order
        .iter()
        .map(|cluster| {
            let entry = preview.and_then(|p| p.entries().iter().find(|e| e.cluster() == cluster));
            let verdict = match (refused.iter().find(|(c, _)| c == cluster), entry) {
                (Some((_, why)), _) => Verdict::Refused(why.clone()),
                (None, None) => Verdict::Refused("it was not reviewed".to_owned()),
                (None, Some(entry)) => match entry.refusal() {
                    Some(error) => Verdict::Refused(error_text(error)),
                    None if !entry.will_run() => Verdict::Refused("left out".to_owned()),
                    None => {
                        let target = member_target(preview, cluster, entry.server());
                        let change = match (entry.change(), entry.edit()) {
                            (Some(change), _) => action_lines(change),
                            (None, Some(edit)) => edit_lines(edit),
                            (None, None) => Vec::new(),
                        };
                        Verdict::Acts {
                            change,
                            warnings: entry
                                .warnings()
                                .iter()
                                .map(|w| warning_text(&target, w))
                                .collect(),
                        }
                    }
                },
            };
            PlanLine {
                cluster: cluster.clone(),
                shown: shown(cluster),
                verdict,
            }
        })
        .collect()
}

/// The object on one member, as a warning about it names it.
fn member_target(preview: Option<&FleetPreview>, cluster: &str, server: &str) -> WriteTarget {
    let object = preview.map(FleetPreview::object);
    WriteTarget {
        cluster: cluster.to_owned(),
        server: Some(server.to_owned()),
        kind: object.map(|o| o.kind.clone()).unwrap_or_default(),
        namespace: object.and_then(|o| o.namespace.clone()),
        name: object.map(|o| o.name.clone()).unwrap_or_default(),
        uid: None,
        resource_version: None,
    }
}

/// "1 cluster", "3 clusters".
fn clusters(n: usize) -> String {
    match n {
        1 => "1 cluster".to_owned(),
        n => format!("{n} clusters"),
    }
}

/// The verb of a title ("Restart", "Scale web to 5").
fn doing(action: &FleetAction, name: &str) -> String {
    match action {
        FleetAction::Scale { to } => format!("Scale {name} to {to}"),
        FleetAction::ApplyYaml { .. } => format!("Apply YAML to {name}"),
        FleetAction::Delete => format!("Delete {name}"),
        _ => format!("Restart {name}"),
    }
}

/// The verb of the button that starts the run.
fn verb(action: &FleetAction) -> &'static str {
    match action {
        FleetAction::Scale { .. } => "Scale",
        FleetAction::ApplyYaml { .. } => "Apply",
        FleetAction::Delete => "Delete",
        _ => "Restart",
    }
}

/// The lines a fleet confirmation always keeps on show: which members act, and the object.
fn pinned_lines(object: &FleetObject, acting: &[&PlanLine]) -> Vec<String> {
    let names: Vec<&str> = acting.iter().map(|l| l.shown.as_str()).collect();
    let mut lines = vec![format!("Runs on: {}", single_line(&names.join(", ")))];
    if let Some(namespace) = &object.namespace {
        lines.push(format!("Namespace: {}", single_line(namespace)));
    }
    lines.push(format!(
        "Object: {}",
        single_line(&format!("{} {}", object.kind, object.name))
    ));
    lines
}

/// The one confirmation of a fleet action: a line per member, acting or refused and why, then
/// the members it runs on and the object, pinned. With no member to act on it only says why,
/// and has nothing to confirm. Returns the text and how many of its last lines are pinned.
pub(in crate::ui) fn plan_dialog(
    action: &FleetAction,
    object: &FleetObject,
    lines: &[PlanLine],
) -> (DialogText, usize) {
    let acting: Vec<&PlanLine> = lines.iter().filter(|l| l.acts()).collect();
    let name = confirm::title_name(&single_line(&object.name));
    let mut text = vec![
        "Runs on one cluster at a time, in this order, and stops at the first failure.".to_owned(),
        String::new(),
    ];
    for line in lines {
        match &line.verdict {
            Verdict::Acts { change, warnings } => {
                text.push(
                    format!("{}: {}", line.shown, change.join(" "))
                        .trim_end()
                        .to_owned(),
                );
                text.extend(warnings.iter().map(|w| format!("  {w}")));
            }
            Verdict::Refused(why) => text.push(format!("{}: left out: {why}", line.shown)),
        }
    }
    if acting.is_empty() {
        let dialog = DialogText {
            title: format!("Nothing to run for {name}"),
            text: format!(
                "No member of this set can be changed now.\n\n{}",
                text[2..].join("\n")
            ),
            ok: "Close".to_owned(),
            cancel: String::new(),
            id: "fleet-plan-description",
            default_cancel: false,
        };
        return (dialog, 0);
    }
    text.push(String::new());
    let pinned = pinned_lines(object, &acting);
    let count = pinned.len();
    text.extend(pinned);
    let verb = verb(action);
    let dialog = DialogText {
        title: format!(
            "{} on {} of {}?",
            doing(action, &name),
            acting.len(),
            clusters(lines.len())
        ),
        text: text.join("\n"),
        ok: format!("{verb} on {}", title_clusters(acting.len())),
        cancel: "Cancel".to_owned(),
        id: "fleet-plan-description",
        // A delete cannot be got back: as on one cluster, Enter cancels it.
        default_cancel: matches!(action, FleetAction::Delete),
    };
    (dialog, count)
}

/// The question before the plan when the review or dry run failed on `failed` members: the run
/// cannot start until they are left out. `None` when none failed.
pub(in crate::ui) fn failed_dialog(
    action: &FleetAction,
    object: &FleetObject,
    lines: &[PlanLine],
    failed: usize,
) -> Option<DialogText> {
    if failed == 0 {
        return None;
    }
    let name = confirm::title_name(&single_line(&object.name));
    let mut text = vec![
        "Nothing runs until the clusters it failed on are left out. A cluster left out is sent          nothing; the next step shows the plan for the rest to confirm."
            .to_owned(),
        String::new(),
    ];
    for line in lines {
        match &line.verdict {
            Verdict::Acts { change, .. } => text.push(
                format!("{}: {}", line.shown, change.join(" "))
                    .trim_end()
                    .to_owned(),
            ),
            Verdict::Refused(why) => text.push(format!("{}: failed: {why}", line.shown)),
        }
    }
    let what = match action {
        FleetAction::Delete => format!("deleting {name}"),
        _ => format!("applying YAML to {name}"),
    };
    Some(DialogText {
        title: format!(
            "The dry run of {what} failed on {failed} of {}",
            clusters(lines.len())
        ),
        text: text.join("\n"),
        ok: "Leave Them Out".to_owned(),
        cancel: "Cancel".to_owned(),
        id: "fleet-failed-description",
        default_cancel: false,
    })
}

/// "1 Cluster", "3 Clusters", for a button.
fn title_clusters(n: usize) -> String {
    match n {
        1 => "1 Cluster".to_owned(),
        n => format!("{n} Clusters"),
    }
}

/// What the results dialog says above its lines.
pub(in crate::ui) fn run_dialog(
    action: &FleetAction,
    object: &FleetObject,
    acting: usize,
) -> DialogText {
    let name = confirm::title_name(&single_line(&object.name));
    let on = clusters(acting);
    let title = match action {
        FleetAction::Scale { to } => format!("Scaling {name} to {to} on {on}"),
        FleetAction::ApplyYaml { .. } => format!("Applying YAML to {name} on {on}"),
        FleetAction::Delete => format!("Deleting {name} on {on}"),
        _ => format!("Restarting {name} on {on}"),
    };
    DialogText {
        title,
        text: "One cluster at a time; the first failure stops the rest. Closing this stops the \
               run before the next cluster."
            .to_owned(),
        ok: "Close".to_owned(),
        cancel: String::new(),
        id: "fleet-run-description",
        default_cancel: false,
    }
}

/// How one member's part of a run stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) enum Outcome {
    /// Its turn has not come.
    Waiting,
    Done,
    Failed(String),
    /// Left out by the plan, and why.
    Refused(String),
    /// Its turn never came, and why.
    NotTried(&'static str),
}

/// One member's line of the results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) struct ResultLine {
    pub cluster: String,
    pub shown: String,
    pub outcome: Outcome,
}

/// The results before the run has heard from any member.
pub(in crate::ui) fn result_lines(plan: &[PlanLine]) -> Vec<ResultLine> {
    plan.iter()
        .map(|line| ResultLine {
            cluster: line.cluster.clone(),
            shown: line.shown.clone(),
            outcome: match &line.verdict {
                Verdict::Acts { .. } => Outcome::Waiting,
                Verdict::Refused(why) => Outcome::Refused(why.clone()),
            },
        })
        .collect()
}

/// Records how `result`'s member ended. A member the plan left out keeps why.
pub(in crate::ui) fn record(lines: &mut [ResultLine], result: &FleetResult) {
    let Some(line) = lines.iter_mut().find(|l| l.cluster == result.cluster) else {
        return;
    };
    line.outcome = match &result.status {
        FleetStatus::Done => Outcome::Done,
        FleetStatus::Failed(error) => Outcome::Failed(error_text(error)),
        FleetStatus::Skipped(FleetSkip::AfterFailure) => {
            Outcome::NotTried("not tried: an earlier cluster failed")
        }
        FleetStatus::Skipped(FleetSkip::Cancelled) => {
            Outcome::NotTried("not tried: the run was stopped")
        }
        FleetStatus::Skipped(FleetSkip::NotStarted) => {
            Outcome::NotTried("not tried: a failed cluster was not left out")
        }
        FleetStatus::Skipped(FleetSkip::Dropped) => match &line.outcome {
            Outcome::Refused(_) => return,
            _ => Outcome::Refused("left out".to_owned()),
        },
        // A status this version does not know: never reported as done.
        _ => Outcome::Failed("Flint can't tell how this cluster ended".to_owned()),
    };
}

/// What a result line says after the member's name. `current` is the member whose turn it is.
pub(in crate::ui) fn outcome_text(outcome: &Outcome, current: bool) -> String {
    match outcome {
        Outcome::Waiting if current => "running…".to_owned(),
        Outcome::Waiting => "waiting".to_owned(),
        Outcome::Done => "done".to_owned(),
        Outcome::Failed(why) => format!("failed: {why}"),
        Outcome::Refused(why) => format!("left out: {why}"),
        Outcome::NotTried(why) => (*why).to_owned(),
    }
}

/// The member whose turn it is while the run goes: the first still waiting.
pub(in crate::ui) fn current(lines: &[ResultLine], finished: bool) -> Option<usize> {
    if finished {
        return None;
    }
    lines.iter().position(|l| l.outcome == Outcome::Waiting)
}

/// The count under the results once the run has ended ("2 done, 1 failed, 1 not tried").
pub(in crate::ui) fn summary(lines: &[ResultLine]) -> String {
    let count = |f: fn(&Outcome) -> bool| lines.iter().filter(|l| f(&l.outcome)).count();
    let parts = [
        (count(|o| *o == Outcome::Done), "done"),
        (count(|o| matches!(o, Outcome::Failed(_))), "failed"),
        (
            count(|o| matches!(o, Outcome::NotTried(_) | Outcome::Waiting)),
            "not tried",
        ),
        (count(|o| matches!(o, Outcome::Refused(_))), "left out"),
    ];
    let said: Vec<String> = parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    format!("Finished: {}.", said.join(", "))
}

#[cfg(test)]
#[path = "fleet_tests.rs"]
mod tests;
