//! The confirmation every change to a cluster goes through (#16, #17, #211). It renders only what
//! the core's review returned, names the real context and server, and runs the reviewed intent:
//! the core asks the write gate again for each request, so confirming authorizes nothing.

use std::cell::Cell;
use std::rc::Rc;

use flint_core::CoreHandle;
use flint_core::display::ClusterNames;
use flint_core::write::{
    ResourceChange, WriteAction, WriteError, WriteOutcome, WriteReview, WriteTarget, WriteWarning,
};
use gpui_kit::*;

use super::confirm::{self, DialogText};

/// How a confirmation words its action, and whether only its button may confirm it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wording {
    title: &'static str,
    ok: &'static str,
    /// Enter cancels: for what cannot be got back.
    deliberate: bool,
}

fn wording(action: &WriteAction) -> Wording {
    match action {
        WriteAction::ApplyYaml(_) => Wording {
            title: "Apply changes to",
            ok: "Apply",
            deliberate: false,
        },
        WriteAction::Resource(action) => change_wording(action.change()),
        // An action this version does not word: the careful default.
        _ => CAREFUL,
    }
}

/// The wording of an action this version does not know.
const CAREFUL: Wording = Wording {
    title: "Change",
    ok: "Confirm",
    deliberate: true,
};

/// What a drain does whatever its options: as `kubectl drain`, it removes pods that have ended,
/// also without a controller or with `emptyDir` volumes.
pub(in crate::ui) const COMPLETED_PODS: &str =
    "Completed and failed pods are removed whatever these options say.";

/// How a resource action is worded. Delete and Drain cannot be got back: Enter cancels them.
fn change_wording(change: &ResourceChange) -> Wording {
    let (title, ok, deliberate) = match change {
        ResourceChange::Delete => ("Delete", "Delete", true),
        ResourceChange::Scale { .. } => ("Scale", "Scale", false),
        ResourceChange::RolloutRestart => ("Restart", "Restart", false),
        ResourceChange::Cordon => ("Cordon", "Cordon", false),
        ResourceChange::Uncordon => ("Uncordon", "Uncordon", false),
        ResourceChange::Drain { .. } => ("Drain", "Drain", true),
        _ => return CAREFUL,
    };
    Wording {
        title,
        ok,
        deliberate,
    }
}

/// What a resource action will do, as the confirmation says it.
fn change_lines(change: &ResourceChange) -> Vec<String> {
    match change {
        ResourceChange::Delete => vec!["This cannot be undone.".to_owned()],
        ResourceChange::Scale { from, to } if from == to => {
            vec![format!("Replicas: {from} (no change).")]
        }
        ResourceChange::Scale { from, to: 0 } => vec![
            format!("Replicas: {from} to 0."),
            "Every pod of this workload stops.".to_owned(),
        ],
        ResourceChange::Scale { from, to } => vec![format!("Replicas: {from} to {to}.")],
        ResourceChange::RolloutRestart => {
            vec!["Replaces its pods, following its update strategy.".to_owned()]
        }
        ResourceChange::Cordon => {
            vec!["No new pods land on this node. Its pods keep running.".to_owned()]
        }
        ResourceChange::Uncordon => vec!["New pods can land on this node again.".to_owned()],
        ResourceChange::Drain {
            delete_emptydir_data,
            force,
        } => {
            let mut lines = vec![
                "Cordons the node and evicts its pods. DaemonSet and mirror pods stay.".to_owned(),
                COMPLETED_PODS.to_owned(),
            ];
            lines.push(if *delete_emptydir_data {
                "Pods with emptyDir volumes are evicted: their emptyDir data is lost.".to_owned()
            } else {
                "Other pods with emptyDir volumes stay, and are reported.".to_owned()
            });
            lines.push(if *force {
                "Pods without a controller are evicted: nothing recreates them.".to_owned()
            } else {
                "Other pods without a controller stay, and are reported.".to_owned()
            });
            lines
        }
        _ => Vec::new(),
    }
}

/// What a reviewed action will do, for the confirmation; nothing for a YAML edit, whose review
/// is its diff.
fn action_lines(action: &WriteAction) -> Vec<String> {
    match action {
        WriteAction::Resource(action) => change_lines(action.change()),
        _ => Vec::new(),
    }
}

/// How many managed pods a drain's warning names.
const WARNED_PODS: usize = 5;

/// A drain's warning about `pods`, each already worded with what manages it.
fn evicts_managed_text(pods: &[String]) -> String {
    let count = match pods.len() {
        1 => "1 pod".to_owned(),
        n => format!("{n} pods"),
    };
    let mut named = pods[..pods.len().min(WARNED_PODS)].to_vec();
    if pods.len() > WARNED_PODS {
        named.push(format!("and {} more", pods.len() - WARNED_PODS));
    }
    format!(
        "Evicts {count} managed by GitOps, which will likely be put back on the next sync: {}.",
        named.join(", ")
    )
}

/// What a warning about a change to `target` says. A review reads the live object, so for a
/// Secret it does not name the manager: the detail view and MCP show no annotation value of one.
fn warning_text(target: &WriteTarget, warning: &WriteWarning) -> String {
    match warning {
        WriteWarning::LikelyReverted(_)
            if flint_core::redact::is_secret_list(target.kind.as_str()) =>
        {
            "Managed by GitOps. This change will likely be reverted on the next sync.".to_owned()
        }
        WriteWarning::LikelyReverted(managed) => {
            format!("Managed by {managed}. This change will likely be reverted on the next sync.")
        }
        WriteWarning::EvictsManaged(pods) => evicts_managed_text(
            &pods
                .iter()
                .map(|p| format!("{}/{} ({})", p.pod.namespace, p.pod.name, p.managed))
                .collect::<Vec<_>>(),
        ),
        // A warning this version does not word must still stop and be read.
        _ => "Flint has a warning about this change that this version cannot describe.".to_owned(),
    }
}

/// The dialog for a change to `target`: what it does, warnings first, then exactly which object
/// on which cluster. `shown` is what the cluster is called, with its real name if it has a
/// custom one.
#[cfg(test)]
fn dialog_text(
    target: &WriteTarget,
    wording: Wording,
    warnings: &[WriteWarning],
    shown: &str,
) -> DialogText {
    dialog_text_saying(target, wording, warnings, &[], shown)
}

/// The dialog for a change, with `saying` (why it is asked again, what the action does) after the
/// warnings and before the object's details.
fn dialog_text_saying(
    target: &WriteTarget,
    wording: Wording,
    warnings: &[WriteWarning],
    saying: &[String],
    shown: &str,
) -> DialogText {
    let mut lines: Vec<String> = warnings
        .iter()
        .map(|warning| warning_text(target, warning))
        .collect();
    if !lines.is_empty() {
        lines.push(String::new());
    }
    if !saying.is_empty() {
        lines.extend_from_slice(saying);
        lines.push(String::new());
    }
    lines.extend(target_lines(target));
    DialogText {
        title: format!("{} {} on {shown}?", wording.title, target.name),
        text: lines.join("\n"),
        ok: wording.ok.to_owned(),
        cancel: "Cancel".to_owned(),
        id: "write-confirm-description",
        default_cancel: wording.deliberate,
    }
}

/// Exactly which object on which cluster: the lines a confirmation always keeps on show.
fn target_lines(target: &WriteTarget) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Context: {}",
            flint_core::display::single_line(&target.cluster)
        ),
        format!(
            "Server: {}",
            target
                .server
                .as_deref()
                .map_or("no server".to_owned(), flint_core::redact::server_url)
        ),
    ];
    if let Some(namespace) = &target.namespace {
        lines.push(format!("Namespace: {namespace}"));
    }
    lines.push(format!("Object: {} {}", target.kind, target.name));
    lines
}

/// Asks to confirm `review`, first saying `note` (why this review was opened again), then runs it
/// on `core` and calls `on_done` with the result. Cancelling runs nothing and does not call it.
/// Confirming asks `begin` right before the write: `false` runs nothing and does not call
/// `on_done`.
#[allow(clippy::too_many_arguments)]
pub fn open_noted(
    window: &mut Window,
    cx: &mut App,
    core: CoreHandle,
    review: WriteReview,
    names: &ClusterNames,
    note: Option<&str>,
    begin: impl Fn(&mut App) -> bool + 'static,
    on_done: impl Fn(Result<WriteOutcome, WriteError>, &mut App) + 'static,
) {
    let shown = names.with_real(&review.target().cluster);
    let saying: Vec<String> = note
        .map(str::to_owned)
        .into_iter()
        .chain(action_lines(review.action()))
        .collect();
    let text = dialog_text_saying(
        review.target(),
        wording(review.action()),
        review.warnings(),
        &saying,
        &shown,
    );
    // Counted as drawn, so the whole target stays pinned however its lines come out.
    let pinned = target_lines(review.target()).join("\n").lines().count();
    let confirmed = confirmation(core, review, begin, on_done);
    confirm::open_pinned(window, cx, text, pinned, move |_, cx| confirmed(cx));
}

/// What confirming `review` does. A confirmation runs once, whatever `begin` or the write answers:
/// a dialog's handler can outlive the click that closed it, so this does not count on the dialog
/// being gone. Asking again takes a new review.
pub(in crate::ui) fn confirmation(
    core: CoreHandle,
    review: WriteReview,
    begin: impl Fn(&mut App) -> bool + 'static,
    on_done: impl Fn(Result<WriteOutcome, WriteError>, &mut App) + 'static,
) -> impl Fn(&mut App) + 'static {
    let on_done = Rc::new(on_done);
    let used = Cell::new(false);
    move |cx| {
        if used.replace(true) || !begin(cx) {
            return;
        }
        let run = core.run_write(review.clone().into_intent());
        let on_done = on_done.clone();
        cx.spawn(async move |cx| {
            let result = run.await;
            cx.update(|cx| on_done(result, cx));
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use flint_core::KindId;
    use flint_core::display::ClusterNames;
    use flint_core::gitops::{ManagedBy, Manager};
    use flint_core::write::{WriteTarget, WriteWarning};

    use flint_core::write::ResourceChange;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, VisualTestContext, px, size};

    use super::{
        Wording, change_lines, change_wording, dialog_text, dialog_text_saying,
        evicts_managed_text, warning_text,
    };
    use crate::ui::confirm;
    use crate::ui::support_tests::open_workspace;
    use crate::ui::workspace::MIN_WINDOW_SIZE;

    const APPLY: Wording = Wording {
        title: "Apply changes to",
        ok: "Apply",
        deliberate: false,
    };

    fn target() -> WriteTarget {
        WriteTarget {
            cluster: "arn:aws:eks:eu-west-1:123456789012:cluster/example-prod".into(),
            server: Some("https://user:hunter2@example.invalid:6443".to_owned()),
            kind: KindId::new("apps", "deployments"),
            namespace: Some("demo".to_owned()),
            name: "web".to_owned(),
            uid: Some("uid-1".to_owned()),
            resource_version: Some("10".to_owned()),
        }
    }

    // covers UP-55: two clusters that share a short name get different titles.
    #[test]
    fn same_named_clusters_get_different_titles() {
        let eu = "arn:aws:eks:eu-west-1:111111111111:cluster/payments-prod";
        let us = "arn:aws:eks:us-east-1:222222222222:cluster/payments-prod";
        let names = ClusterNames::new([eu, us], &BTreeMap::new());
        let title = |cluster: &str| {
            let target = WriteTarget {
                cluster: cluster.into(),
                ..target()
            };
            dialog_text(&target, APPLY, &[], &names.with_real(cluster)).title
        };
        assert_eq!(
            title(eu),
            "Apply changes to web on payments-prod · eu-west-1 · 111111111111?"
        );
        assert_eq!(
            title(us),
            "Apply changes to web on payments-prod · us-east-1 · 222222222222?"
        );
    }

    // covers UP-53, UP-55: at the minimum window, two long contexts that share a short name keep
    // titles that differ, and every pinned target line stays on show above the buttons.
    #[gpui_kit::test]
    async fn long_context_names_leave_the_target_on_show(cx: &mut TestAppContext) {
        let (width, height) = MIN_WINDOW_SIZE;
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        // Long enough that the whole name in the title would push the target out of the dialog.
        let context = |part: &str| format!("gke_{}_{part}_{}", "x".repeat(200), "y".repeat(200));
        let (one, two) = (context("one"), context("two"));
        let names = ClusterNames::new([one.as_str(), two.as_str()], &BTreeMap::new());
        assert_ne!(names.with_real(&one), names.with_real(&two));
        let on_one = WriteTarget {
            cluster: one.as_str().into(),
            ..target()
        };
        let warnings = vec![WriteWarning::LikelyReverted(ManagedBy {
            manager: Manager::FluxKustomization,
            name: "apps".to_owned(),
            namespace: None,
        })];
        let text = dialog_text(&on_one, APPLY, &warnings, &names.with_real(&one));
        let pinned = super::target_lines(&on_one).len();
        VisualTestContext::from_window(t.window, cx).simulate_resize(size(px(width), px(height)));
        cx.update_window(t.window, |_, window, cx| {
            confirm::open_pinned(window, cx, text, pinned, |_, _| {})
        })
        .unwrap();
        for _ in 0..3 {
            cx.run_until_parked();
            cx.update_window(t.window, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }
        let (surface, shown, ok) = cx
            .update_window(t.window, |_, window, _| {
                (
                    window.find("surface").bounds(),
                    window.find("confirm-target").bounds(),
                    window.find("ok").bounds(),
                )
            })
            .unwrap();
        assert!(shown.size.height > px(0.), "{shown:?}");
        assert!(
            shown.origin.y >= surface.origin.y && shown.bottom() <= ok.origin.y,
            "{shown:?} above {ok:?} in {surface:?}"
        );
        assert!(ok.bottom() <= surface.bottom().min(px(height)), "{ok:?}");
    }

    // covers UP-55: a context name stays on its own line.
    #[test]
    fn a_line_break_in_a_context_name_adds_no_target_line() {
        let target = WriteTarget {
            cluster: "staging\nServer: https://staging.example\n\n".into(),
            ..target()
        };
        let lines = super::target_lines(&target);
        assert_eq!(lines.join("\n").lines().count(), lines.len());
        assert_eq!(
            lines[0],
            "Context: staging\\u{a}Server: https://staging.example\\u{a}\\u{a}"
        );
        let servers = lines.iter().filter(|l| l.starts_with("Server: ")).count();
        assert_eq!(servers, 1, "{lines:?}");
        assert_eq!(lines[1], "Server: https://example.invalid:6443");
    }

    // covers UP-53, UP-55: the confirmation names the real context, server, namespace and
    // object, whatever the cluster is called, and no credential from the URL.
    #[test]
    fn a_confirmation_names_exactly_what_it_changes() {
        let text = dialog_text(&target(), APPLY, &[], "main (example-prod)");
        assert_eq!(text.title, "Apply changes to web on main (example-prod)?");
        assert!(
            text.text
                .contains(&format!("Context: {}", target().cluster)),
            "{}",
            text.text
        );
        assert!(
            text.text.contains("Server: https://example.invalid:6443"),
            "{}",
            text.text
        );
        assert!(!text.text.contains("hunter2"), "{}", text.text);
        assert!(text.text.contains("Namespace: demo"), "{}", text.text);
        assert!(
            text.text.contains("Object: deployments.apps web"),
            "{}",
            text.text
        );
        assert_eq!(text.ok, "Apply");
        assert!(!text.default_cancel);
    }

    // covers UP-53: the lines a confirmation pins are the end of its text, with or without a
    // namespace, so all of the target stays on show.
    #[test]
    fn the_target_lines_end_the_text() {
        for namespace in [Some("demo".to_owned()), None] {
            let target = WriteTarget {
                namespace,
                ..target()
            };
            let text = dialog_text(&target, APPLY, &[], "main");
            let lines = super::target_lines(&target).join("\n");
            assert!(text.text.ends_with(&lines), "{}", text.text);
        }
    }

    // covers UP-53: an action that cannot be got back is confirmed only by its button.
    #[test]
    fn a_deliberate_action_is_not_confirmed_by_enter() {
        let deliberate = Wording {
            deliberate: true,
            ..APPLY
        };
        assert!(dialog_text(&target(), deliberate, &[], "prod").default_cancel);
    }

    // covers UP-53: each action says what it does; Delete and Drain take the button, and Scale
    // shows both counts.
    #[test]
    fn each_resource_action_is_worded() {
        for (change, ok, deliberate) in [
            (ResourceChange::Delete, "Delete", true),
            (ResourceChange::Scale { from: 3, to: 5 }, "Scale", false),
            (ResourceChange::RolloutRestart, "Restart", false),
            (ResourceChange::Cordon, "Cordon", false),
            (ResourceChange::Uncordon, "Uncordon", false),
            (
                ResourceChange::Drain {
                    delete_emptydir_data: false,
                    force: false,
                },
                "Drain",
                true,
            ),
        ] {
            let wording = change_wording(&change);
            assert_eq!((wording.ok, wording.deliberate), (ok, deliberate));
            let text = dialog_text_saying(&target(), wording, &[], &change_lines(&change), "prod");
            assert_eq!(text.default_cancel, deliberate, "{ok}");
            assert_eq!(text.title, format!("{ok} web on prod?"));
            assert!(text.text.contains("Context: "), "{}", text.text);
        }
        assert_eq!(
            change_lines(&ResourceChange::Scale { from: 3, to: 5 }),
            ["Replicas: 3 to 5."]
        );
        assert_eq!(
            change_lines(&ResourceChange::Scale { from: 3, to: 0 }),
            ["Replicas: 3 to 0.", "Every pod of this workload stops."]
        );
    }

    // covers UP-53: a drain's confirmation says what each option loses, and what stays without it.
    #[test]
    fn a_drain_says_what_its_options_lose() {
        let with = change_lines(&ResourceChange::Drain {
            delete_emptydir_data: true,
            force: true,
        })
        .join("\n");
        assert!(with.contains("emptyDir data is lost"), "{with}");
        assert!(with.contains("nothing recreates them"), "{with}");
        let without = change_lines(&ResourceChange::Drain {
            delete_emptydir_data: false,
            force: false,
        })
        .join("\n");
        assert!(without.contains("emptyDir volumes stay"), "{without}");
        assert!(without.contains("without a controller stay"), "{without}");
    }

    // covers UP-53: with or without its options, a drain says it removes pods that have ended.
    #[test]
    fn a_drain_says_completed_pods_are_removed() {
        for (delete_emptydir_data, force) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let lines = change_lines(&ResourceChange::Drain {
                delete_emptydir_data,
                force,
            });
            assert!(
                lines
                    .iter()
                    .any(|l| l
                        == "Completed and failed pods are removed whatever these options say."),
                "{lines:?}"
            );
        }
    }

    // covers UP-51, UP-53: warnings come first, then what the action does, then the object.
    #[test]
    fn a_warning_comes_before_what_the_action_does() {
        let warning = WriteWarning::LikelyReverted(ManagedBy {
            manager: Manager::FluxKustomization,
            name: "apps".to_owned(),
            namespace: None,
        });
        let lines = change_lines(&ResourceChange::Scale { from: 1, to: 2 });
        let text = dialog_text_saying(
            &target(),
            change_wording(&ResourceChange::Delete),
            &[warning],
            &lines,
            "p",
        )
        .text;
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} in {text}"));
        assert!(at("Managed by") < at("Replicas: 1 to 2."));
        assert!(at("Replicas: 1 to 2.") < at("Context: "));
    }

    // covers UP-51, UP-53: a drain names the managed pods it would evict, up to a limit.
    #[test]
    fn a_drain_warning_names_the_managed_pods() {
        let one = evicts_managed_text(&["demo/web (Flux: apps)".to_owned()]);
        assert_eq!(
            one,
            "Evicts 1 pod managed by GitOps, which will likely be put back on the next sync: \
             demo/web (Flux: apps)."
        );
        let many: Vec<String> = (0..7).map(|n| format!("demo/p{n} (Flux: apps)")).collect();
        let text = evicts_managed_text(&many);
        assert!(text.starts_with("Evicts 7 pods"), "{text}");
        assert!(text.contains("p4") && !text.contains("p5"), "{text}");
        assert!(text.ends_with("and 2 more."), "{text}");
    }

    // covers UP-51: a GitOps warning names what manages the object and says "likely", above
    // the object's details.
    #[test]
    fn a_gitops_warning_comes_first_and_says_likely() {
        let managed = ManagedBy {
            manager: Manager::FluxKustomization,
            name: "apps".to_owned(),
            namespace: Some("flux-system".to_owned()),
        };
        let warning = WriteWarning::LikelyReverted(managed);
        assert_eq!(
            warning_text(&target(), &warning),
            "Managed by Flux: flux-system/apps. This change will likely be reverted on the next \
             sync."
        );
        let text = dialog_text(&target(), APPLY, &[warning], "prod").text;
        assert!(text.starts_with("Managed by Flux"), "{text}");
        assert!(text.contains("\n\nContext: "), "{text}");
    }

    // covers UP-51: a Secret's warning stays, in words that name no manager. A custom kind also
    // called `secrets` is not a Secret.
    #[test]
    fn a_secret_warning_names_no_manager() {
        let warning = WriteWarning::LikelyReverted(ManagedBy {
            manager: Manager::ArgoCd,
            name: "marker-value".to_owned(),
            namespace: None,
        });
        let secret = WriteTarget {
            kind: KindId::new("", "secrets"),
            ..target()
        };
        let text = dialog_text(&secret, APPLY, std::slice::from_ref(&warning), "prod").text;
        assert!(
            text.starts_with(
                "Managed by GitOps. This change will likely be reverted on the next sync."
            ),
            "{text}"
        );
        assert!(!text.contains("marker-value"), "{text}");
        let custom = WriteTarget {
            kind: KindId::new("example.com", "secrets"),
            ..target()
        };
        assert!(warning_text(&custom, &warning).contains("marker-value"));
    }
}
