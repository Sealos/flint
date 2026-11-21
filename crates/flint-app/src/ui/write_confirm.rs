//! The confirmation every change to a cluster goes through (#16, #17, #211). It renders only what
//! the core's review returned, names the real context and server, and runs the reviewed intent:
//! the core asks the write gate again for each request, so confirming authorizes nothing.

// The YAML editor (#16) and the resource actions (#17) open it.
#![allow(dead_code)]

use std::rc::Rc;

use flint_core::CoreHandle;
use flint_core::display::ClusterNames;
use flint_core::write::{
    WriteAction, WriteError, WriteOutcome, WriteReview, WriteTarget, WriteWarning,
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
        // An action this version does not word: the careful default.
        _ => Wording {
            title: "Change",
            ok: "Confirm",
            deliberate: true,
        },
    }
}

/// What a warning says.
fn warning_text(warning: &WriteWarning) -> String {
    match warning {
        WriteWarning::LikelyReverted(managed) => {
            format!("Managed by {managed}. This change will likely be reverted on the next sync.")
        }
        // A warning this version does not word must still stop and be read.
        _ => "Flint has a warning about this change that this version cannot describe.".to_owned(),
    }
}

/// The dialog for a change to `target`: what it does, warnings first, then exactly which object
/// on which cluster. `shown` is what the cluster is called, with its real name if it has a
/// custom one.
fn dialog_text(
    target: &WriteTarget,
    wording: Wording,
    warnings: &[WriteWarning],
    shown: &str,
) -> DialogText {
    let mut lines: Vec<String> = warnings.iter().map(warning_text).collect();
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.push(format!("Context: {}", target.cluster));
    lines.push(format!(
        "Server: {}",
        target
            .server
            .as_deref()
            .map_or("no server".to_owned(), flint_core::redact::server_url)
    ));
    if let Some(namespace) = &target.namespace {
        lines.push(format!("Namespace: {namespace}"));
    }
    lines.push(format!("Object: {} {}", target.kind, target.name));
    DialogText {
        title: format!("{} {} on {shown}?", wording.title, target.name),
        text: lines.join("\n"),
        ok: wording.ok.to_owned(),
        cancel: "Cancel".to_owned(),
        id: "write-confirm-description",
        default_cancel: wording.deliberate,
    }
}

/// Asks to confirm `review`, then runs it on `core` and calls `on_done` with the result.
/// Cancelling runs nothing and does not call it.
pub fn open(
    window: &mut Window,
    cx: &mut App,
    core: CoreHandle,
    review: WriteReview,
    names: &ClusterNames,
    on_done: impl Fn(Result<WriteOutcome, WriteError>, &mut App) + 'static,
) {
    let shown = names.with_real(&review.target().cluster);
    let text = dialog_text(
        review.target(),
        wording(review.action()),
        review.warnings(),
        &shown,
    );
    let on_done = Rc::new(on_done);
    confirm::open(window, cx, text, move |_, cx| {
        let run = core.run_write(review.clone().into_intent());
        let on_done = on_done.clone();
        cx.spawn(async move |cx| {
            let result = run.await;
            cx.update(|cx| on_done(result, cx));
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use flint_core::KindId;
    use flint_core::gitops::{ManagedBy, Manager};
    use flint_core::write::{WriteTarget, WriteWarning};

    use super::{Wording, dialog_text, warning_text};

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

    // covers UP-53: an action that cannot be got back is confirmed only by its button.
    #[test]
    fn a_deliberate_action_is_not_confirmed_by_enter() {
        let deliberate = Wording {
            deliberate: true,
            ..APPLY
        };
        assert!(dialog_text(&target(), deliberate, &[], "prod").default_cancel);
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
            warning_text(&warning),
            "Managed by Flux: flux-system/apps. This change will likely be reverted on the next \
             sync."
        );
        let text = dialog_text(&target(), APPLY, &[warning], "prod").text;
        assert!(text.starts_with("Managed by Flux"), "{text}");
        assert!(text.contains("\n\nContext: "), "{text}");
    }
}
