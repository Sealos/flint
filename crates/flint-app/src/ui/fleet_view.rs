//! Fleet actions from a cluster set's row (#705, UP-100): restart or scale the selected workload
//! on every member of the running set.
//!
//! The flow: the action (and for Scale, a replica count), then the core's preview
//! ([`flint_core::CoreHandle::preview_fleet`]), which reviews and server-dry-runs it on each
//! member through the write gate, bound to that member. One confirmation shows a line per
//! member, acting or left out and why, and names the members it runs on. The run
//! ([`flint_core::CoreHandle::run_fleet`]) goes one member at a time, asks the gate again before
//! each request, never retries, and stops at the first failure; its dialog shows each member's
//! result as it ends.
//!
//! Nothing here decides access. A member is left out before the preview when its row could not
//! be acted on (it left its admitted server, is paused, not connected or protected), and after
//! it when its review or dry run was refused: a left-out member gets no request. Locked members
//! stay locked; there is no unlock here. The confirmation holds only while every acting member
//! keeps the connection it was planned on, and each member's object is claimed while the run
//! goes, as a single-cluster write claims it.

// module-tests: fleet_tests.rs

use std::cell::RefCell;
use std::rc::Rc;

use flint_core::fleet::{
    FleetAction, FleetCancel, FleetObject, FleetPlan, FleetPreview, FleetResult,
};
use flint_core::action_kinds;
use flint_core::handle::AdmittedMember;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::input::InputState;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::confirm::{self, DialogText};
use super::detail::{Opening, protected_reason, refused};
use super::fleet_plan::{
    FleetChoice, Outcome, PlanLine, ResultLine, current, outcome_text, plan_dialog, plan_lines,
    record, result_lines, run_dialog, summary,
};
use super::resource_actions::{
    ResourceAction, ScaleField, WriteKey, claim, offered_for, parse_replicas, release,
};
use super::row_menu::Availability;
use super::workspace::Workspace;
use super::write_binding::{ConnectionGuard, connection_changed_text};

/// Whether `choice` can run on the selected row now, and why not: what the row menu shows, the
/// guards check, and the handler checks again.
pub(in crate::ui) fn available(ws: &Workspace, choice: FleetChoice, cx: &App) -> Availability {
    let Some(members) = ws.set_members_to_act_on(cx) else {
        return Err("no cluster set is open".to_owned());
    };
    let Some(detail) = ws.detail_view() else {
        return Err("no object is selected".to_owned());
    };
    let object = detail.read(cx).object();
    if !members.iter().any(|(m, _)| m.context() == object.cluster) {
        return Err("the selected row is not from the open cluster set".to_owned());
    }
    // A set's members may define a custom resource differently: built-in kinds only.
    let scalable = action_kinds::scales_built_in(&object.kind);
    if !offered_for(&object.kind, scalable, false, None).contains(&choice.resource_action()) {
        return Err(format!("{} has no such action", object.kind_label));
    }
    Ok(())
}

/// The selected row's object, as every member is asked about it.
fn fleet_object(ws: &Workspace, cx: &App) -> Option<FleetObject> {
    let detail = ws.detail_view()?;
    let object = detail.read(cx).object();
    Some(FleetObject {
        kind: object.kind.clone(),
        namespace: object.namespace.clone(),
        name: object.name.clone(),
    })
}

/// The set's members split before any request: the ones to preview, each with a guard on the
/// connection it has now, and the ones left out, with why.
struct Planned {
    /// Every member, in the set's order.
    order: Vec<String>,
    acting: Vec<AdmittedMember>,
    guards: Vec<ConnectionGuard>,
    refused: Vec<(String, String)>,
}

impl Planned {
    /// A guard that holds while every acting member keeps its connection.
    fn guard(&self) -> ConnectionGuard {
        ConnectionGuard::all(self.guards.clone())
    }
}

fn plan_members(ws: &Workspace, cx: &App) -> Planned {
    let names = ws.cluster_names(cx);
    let mut planned = Planned {
        order: Vec::new(),
        acting: Vec::new(),
        guards: Vec::new(),
        refused: Vec::new(),
    };
    for (member, why) in ws.set_members_to_act_on(cx).unwrap_or_default() {
        let cluster = member.context().to_owned();
        planned.order.push(cluster.clone());
        let why = why
            .or_else(|| ws.sessions_blocked(&cluster, cx))
            .or_else(|| ws.writes_paused_why(&cluster, cx))
            .or_else(|| {
                ws.is_protected(&cluster, cx)
                    .then(|| protected_reason(&names.exact(&cluster)))
            });
        match (why, ws.connection_guard(&cluster, cx)) {
            (Some(why), _) => planned.refused.push((cluster, why)),
            (None, None) => {
                let why = format!("{} has no settled connection", names.exact(&cluster));
                planned.refused.push((cluster, why));
            }
            (None, Some(guard)) => {
                planned.acting.push(member);
                planned.guards.push(guard);
            }
        }
    }
    planned
}

/// Runs `choice` on the selected row across the open set: the replica count first for Scale,
/// then the preview and its confirmation.
pub(in crate::ui) fn request(
    ws: &mut Workspace,
    choice: FleetChoice,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let refuse = |ws: &mut Workspace, why: &str, cx: &mut Context<Workspace>| {
        ws.refuse(refused(Opening::Write(choice.resource_action()), why), cx)
    };
    if let Err(why) = available(ws, choice, cx) {
        refuse(ws, &why, cx);
        return;
    }
    let Some(object) = fleet_object(ws, cx) else {
        return;
    };
    let planned = plan_members(ws, cx);
    if planned.acting.is_empty() {
        let names = ws.cluster_names(cx);
        let lines = plan_lines(&planned.order, &planned.refused, None, |c| names.exact(c));
        // Says only why: there is nothing to confirm.
        let (text, _) = plan_dialog(FleetAction::RolloutRestart, &object, &lines);
        confirm::open(window, cx, text, |_, _| {});
        return;
    }
    match choice {
        FleetChoice::RolloutRestart => {
            start_preview(ws, planned, FleetAction::RolloutRestart, object, window, cx)
        }
        FleetChoice::Scale => ask_replicas(planned, object, window, cx),
    }
}

/// Which object a dialog before the preview is about.
fn object_lines(object: &FleetObject) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(namespace) = &object.namespace {
        lines.push(format!(
            "Namespace: {}",
            flint_core::display::single_line(namespace)
        ));
    }
    lines.push(format!(
        "Object: {}",
        flint_core::display::single_line(&format!("{} {}", object.kind, object.name))
    ));
    lines
}

fn scale_dialog(object: &FleetObject, members: usize) -> DialogText {
    let name = confirm::title_name(&flint_core::display::single_line(&object.name));
    DialogText {
        title: format!("Scale {name} across the set"),
        text: format!(
            "The replica count for each of the {members} members it can run on. The next step \
             shows each member's change to confirm.\n\n{}",
            object_lines(object).join("\n")
        ),
        ok: "Continue".into(),
        cancel: "Cancel".into(),
        id: "fleet-scale-description",
        default_cancel: false,
    }
}

fn ask_replicas(
    planned: Planned,
    object: FleetObject,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let input = cx.new(|cx| InputState::new(window, cx).placeholder("Replicas"));
    let field = cx.new(|_| ScaleField {
        input: input.clone(),
        error: None,
    });
    let text = scale_dialog(&object, planned.acting.len());
    let pinned = object_lines(&object).len();
    let guard = planned.guard();
    let this = cx.weak_entity();
    let content = field.clone().into();
    let planned = Rc::new(RefCell::new(Some(planned)));
    let on_ok = move |window: &mut Window, cx: &mut App| {
        let typed = field.read(cx).input.read(cx).value().to_string();
        let to = match parse_replicas(&typed) {
            Ok(to) => to,
            Err(error) => {
                field.update(cx, |f, cx| {
                    f.error = Some(error);
                    cx.notify();
                });
                return false;
            }
        };
        let Some(planned) = planned.borrow_mut().take() else {
            return true;
        };
        let object = object.clone();
        let action = FleetAction::Scale { to };
        let _ = this.update(cx, |ws, cx| {
            start_preview(ws, planned, action, object, window, cx)
        });
        true
    };
    confirm::open_pinned_guarded(window, cx, text, pinned, Some(content), guard, on_ok);
    input.update(cx, |input, cx| input.focus(window, cx));
}

/// Has the core review and dry-run `action` on every acting member; the confirmation opens with
/// the result.
fn start_preview(
    ws: &mut Workspace,
    planned: Planned,
    action: FleetAction,
    object: FleetObject,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let plan = match FleetPlan::new(action, object.clone(), planned.acting.clone()) {
        Ok(plan) => plan,
        Err(error) => {
            let why = error.to_string();
            ws.refuse(refused(Opening::Write(resource_action(action)), &why), cx);
            return;
        }
    };
    let preview = ws.core(cx).preview_fleet(plan);
    let handle = window.window_handle();
    let task = cx.spawn(async move |this, cx| {
        let preview = preview.await;
        let _ = cx.update_window(handle, |_, window, cx| {
            let _ = this.update(cx, |ws, cx| {
                previewed(ws, planned, object, preview, window, cx)
            });
        });
    });
    ws.set_write_review(task);
}

fn resource_action(action: FleetAction) -> ResourceAction {
    match action {
        FleetAction::Scale { .. } => ResourceAction::Scale,
        _ => ResourceAction::RolloutRestart,
    }
}

fn previewed(
    ws: &mut Workspace,
    planned: Planned,
    object: FleetObject,
    mut preview: FleetPreview,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let guard = planned.guard();
    // A preview that comes back from another connection is not offered, whatever it says.
    if !guard.holds(cx) {
        ws.refuse_held(connection_changed_text(), cx);
        return;
    }
    // Left in, a refused member would fail at its turn and stop the others.
    preview.drop_refused();
    let names = ws.cluster_names(cx);
    let lines = plan_lines(&planned.order, &planned.refused, Some(&preview), |c| {
        names.exact(c)
    });
    let (text, pinned) = plan_dialog(preview.action(), &object, &lines);
    if !lines.iter().any(PlanLine::acts) {
        confirm::open(window, cx, text, |_, _| {});
        return;
    }
    let core = ws.core(cx).clone();
    let this = cx.weak_entity();
    let preview = Rc::new(RefCell::new(Some(preview)));
    let on_ok = move |window: &mut Window, cx: &mut App| {
        // Runs once, whatever asks twice.
        if let Some(preview) = preview.borrow_mut().take() {
            start_run(&this, &core, preview, &object, &lines, window, cx);
        }
        true
    };
    confirm::open_pinned_guarded(window, cx, text, pinned, None, guard, on_ok);
}

/// Claims each acting member's object, opens the results and starts the run. Nothing runs if
/// any of those objects has a write running already.
fn start_run(
    this: &WeakEntity<Workspace>,
    core: &flint_core::CoreHandle,
    preview: FleetPreview,
    object: &FleetObject,
    lines: &[PlanLine],
    window: &mut Window,
    cx: &mut App,
) {
    let action = preview.action();
    let keys: Vec<WriteKey> = lines
        .iter()
        .filter(|l| l.acts())
        .map(|l| {
            WriteKey::new(
                &l.cluster,
                &object.kind,
                object.namespace.as_deref(),
                &object.name,
            )
        })
        .collect();
    let mut claimed: Vec<WriteKey> = Vec::new();
    for key in &keys {
        if let Err(why) = claim(key, resource_action(action), cx) {
            for key in &claimed {
                release(key, cx);
            }
            let _ = this.update(cx, |ws, cx| {
                ws.refuse(refused(Opening::Write(resource_action(action)), &why), cx)
            });
            return;
        }
        claimed.push(key.clone());
    }
    let cancel = FleetCancel::new();
    let view = cx.new(|_| FleetRunView::new(result_lines(lines), cancel.clone()));
    let text = run_dialog(action, object, keys.len());
    confirm::open_with(window, cx, text, Some(view.clone().into()), |_, _| true);
    let (sender, receiver) = async_channel::unbounded::<FleetResult>();
    let run = core.run_fleet(preview, cancel, move |result| {
        let _ = sender.try_send(result.clone());
    });
    let view = view.downgrade();
    cx.spawn(async move |cx| {
        let run = cx.background_spawn(run);
        while let Ok(result) = receiver.recv().await {
            let _ = view.update(cx, |view, cx| view.heard(&result, cx));
        }
        run.await;
        cx.update(|cx| {
            // Whatever became of the window: a closed one must not keep the objects claimed.
            for key in &claimed {
                release(key, cx);
            }
            let _ = view.update(cx, |view, cx| view.finish(cx));
        });
    })
    .detach();
}

/// The results of a running fleet action, a line per member, under its dialog's text.
pub(in crate::ui) struct FleetRunView {
    lines: Vec<ResultLine>,
    finished: bool,
    cancel: FleetCancel,
}

impl FleetRunView {
    pub(in crate::ui) fn new(lines: Vec<ResultLine>, cancel: FleetCancel) -> Self {
        Self {
            lines,
            finished: false,
            cancel,
        }
    }

    pub(in crate::ui) fn heard(&mut self, result: &FleetResult, cx: &mut Context<Self>) {
        record(&mut self.lines, result);
        cx.notify();
    }

    pub(in crate::ui) fn finish(&mut self, cx: &mut Context<Self>) {
        self.finished = true;
        cx.notify();
    }

    /// Each line as drawn, for tests.
    #[cfg(test)]
    pub(in crate::ui) fn texts(&self) -> Vec<String> {
        let current = current(&self.lines, self.finished);
        self.lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                format!(
                    "{}: {}",
                    l.shown,
                    outcome_text(&l.outcome, current == Some(i))
                )
            })
            .collect()
    }
}

/// Closing the results stops the run before its next member. The member whose turn it is runs
/// to its end: a write is never cut off halfway.
impl Drop for FleetRunView {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Render for FleetRunView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let current = current(&self.lines, self.finished);
        v_flex()
            .id("fleet-results")
            .gap_1()
            .text_sm()
            .children(self.lines.iter().enumerate().map(|(i, line)| {
                let text = outcome_text(&line.outcome, current == Some(i));
                let color = match line.outcome {
                    Outcome::Done => theme.success,
                    Outcome::Failed(_) => theme.danger,
                    _ => theme.muted_foreground,
                };
                let label = format!("{}: {text}", line.shown);
                h_flex()
                    .id(("fleet-result", i))
                    .gap_2()
                    .aria_label(label)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(line.shown.clone()),
                    )
                    .child(div().text_color(color).child(text))
                    .test_support()
            }))
            .when(self.finished, |el| {
                let said = summary(&self.lines);
                el.child(
                    div()
                        .id("fleet-summary")
                        .pt_1()
                        .aria_label(said.clone())
                        .child(said)
                        .test_support(),
                )
            })
    }
}
