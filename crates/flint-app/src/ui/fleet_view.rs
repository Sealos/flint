//! Fleet actions from a cluster set's row (#705, #450, UP-100, UP-101): restart, scale, apply
//! YAML to or delete the selected object on every member of the running set.
//!
//! The flow: the action (for Scale, a replica count; for Apply YAML, the manifest, opened from
//! the selected object without what one cluster's copy carries), then the core's preview
//! ([`flint_core::CoreHandle::preview_fleet`]), which reviews and server-dry-runs it on each
//! member through the write gate, bound to that member. One confirmation shows a line per
//! member, acting or left out and why, and names the members it runs on. The run
//! ([`flint_core::CoreHandle::run_fleet`]) goes one member at a time, asks the gate again before
//! each request, never retries, and stops at the first failure; its dialog shows each member's
//! result as it ends.
//!
//! Nothing here decides access. A member is left out before the preview when its row could not
//! be acted on (it left its admitted server, is paused, not connected or protected), and after
//! it when its review or dry run was refused: a left-out member gets no request. For apply and
//! delete a refused member is not left out on its own: the failures are shown first, and nothing
//! runs until they are left out (the core refuses to start a preview that still has one). Locked
//! members stay locked; there is no unlock here. The confirmation holds only while every acting member
//! keeps the connection it was planned on, and each member's object is claimed while the run
//! goes, as a single-cluster write claims it.

// module-tests: fleet_tests.rs

use std::cell::RefCell;
use std::rc::Rc;

use flint_core::fleet::{
    FleetAction, FleetCancel, FleetManifest, FleetObject, FleetPlan, FleetPreview, FleetResult,
};
use flint_core::handle::AdmittedMember;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Editor, EditorState, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::confirm::{self, ConfirmState, DialogText};
use super::detail::protected_reason;
use super::fleet_plan::{
    FleetChoice, Outcome, PlanLine, ResultLine, TargetRow, current, outcome_text, plan_dialog,
    plan_lines, record, result_lines, run_dialog, running_write, summary, target_rows,
    targets_dialog, targets_state,
};
use super::registry::{self, Bind, EditFleetManifestAgain, OnView, ToggleFleetTarget};
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
    match choice.resource_action() {
        Some(action) => {
            let deletable = action == ResourceAction::Delete
                && ws.kind_deletable(&object.cluster, &object.kind, cx);
            if !offered_for(&object.kind, deletable, None).contains(&action) {
                return Err(format!("{} has no such action", object.kind_label));
            }
        }
        // Its manifest opens from the selected object, so that object must be readable, and
        // still the one shown.
        None if detail.read(cx).is_gone() || detail.read(cx).edit_moved() => {
            return Err("the selected object changed; open it again".to_owned());
        }
        None if detail.read(cx).is_secret() => {
            return Err(
                "Secret values are hidden, so a Secret cannot be applied across the set".to_owned(),
            );
        }
        None if !detail.read(cx).has_loaded_object() => {
            return Err("the object is not loaded".to_owned());
        }
        None => {}
    }
    Ok(())
}

/// A refusal of `choice`, as the error line says it.
fn refusal(choice: FleetChoice, why: &str) -> String {
    let why = why.trim_end();
    // A reason cut short ends in "…", which ends the sentence too.
    let end = if why.ends_with(['.', '!', '?', '…']) {
        ""
    } else {
        "."
    };
    format!("{}: {why}{end}", choice.cant())
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
#[derive(Clone)]
struct Planned {
    /// Every member, in the set's order.
    order: Vec<String>,
    acting: Vec<AdmittedMember>,
    guards: Vec<ConnectionGuard>,
    refused: Vec<(String, String)>,
    /// The selected row's member and the uid it shows: that member's review is bound to it.
    shown: Option<(String, String)>,
}

impl Planned {
    /// A guard that holds while every acting member keeps its connection.
    fn guard(&self) -> ConnectionGuard {
        ConnectionGuard::all(self.guards.clone())
    }
}

fn plan_members(ws: &Workspace, cx: &App) -> Planned {
    let names = ws.cluster_names(cx);
    let shown = ws.detail_view().and_then(|detail| {
        let object = detail.read(cx).object();
        (!object.uid.is_empty()).then(|| (object.cluster.to_string(), object.uid.to_string()))
    });
    let mut planned = Planned {
        order: Vec::new(),
        acting: Vec::new(),
        guards: Vec::new(),
        refused: Vec::new(),
        shown,
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
        ws.refuse(refusal(choice, why), cx)
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
        let (text, _) = plan_dialog(&FleetAction::RolloutRestart, &object, &lines);
        confirm::open(window, cx, text, |_, _| {});
        return;
    }
    match choice {
        FleetChoice::RolloutRestart => {
            start_preview(ws, planned, FleetAction::RolloutRestart, object, window, cx)
        }
        FleetChoice::Delete => start_preview(ws, planned, FleetAction::Delete, object, window, cx),
        FleetChoice::Scale => ask_replicas(planned, object, window, cx),
        FleetChoice::ApplyYaml => {
            let manifest = ws.detail_view().and_then(|d| d.read(cx).manifest_yaml());
            match manifest {
                Some(Ok(yaml)) => ask_manifest(planned, object, yaml, window, cx),
                Some(Err(why)) => refuse(ws, &why, cx),
                None => refuse(ws, "the object is not loaded", cx),
            }
        }
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

/// The manifest Apply YAML sends to every member, as typed, and why the last Review could not
/// start.
pub(in crate::ui) struct ManifestField {
    pub(in crate::ui) editor: Entity<EditorState>,
    pub(in crate::ui) error: Option<String>,
}

impl Render for ManifestField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_1()
            .child(
                div()
                    .id("fleet-manifest")
                    .h(px(320.))
                    .border_1()
                    .border_color(theme.border)
                    .rounded(theme.radius)
                    .child(Editor::new(&self.editor).bordered(false).h_full())
                    .test_support(),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("fleet-manifest-error")
                        .aria_label(error.clone())
                        .text_xs()
                        .text_color(theme.danger)
                        .child(error)
                        .test_support(),
                )
            })
    }
}

fn manifest_dialog(object: &FleetObject, members: usize) -> DialogText {
    let name = confirm::title_name(&flint_core::display::single_line(&object.name));
    DialogText {
        title: format!("Apply YAML to {name} across the set"),
        text: format!(
            "One manifest for each of the {members} members it can run on: it replaces the \
             object where it exists and creates it where it does not. Review dry-runs it on \
             each member; the next step shows each member's change to confirm.\n\n{}",
            object_lines(object).join("\n")
        ),
        ok: "Review".into(),
        cancel: "Cancel".into(),
        id: "fleet-manifest-description",
        default_cancel: false,
    }
}

fn ask_manifest(
    planned: Planned,
    object: FleetObject,
    yaml: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let editor = cx.new(|cx| {
        let mut state = EditorState::new(window, cx)
            .language("yaml")
            .line_number(true)
            .soft_wrap(false);
        state.set_value(yaml, window, cx);
        state
    });
    let field = cx.new(|_| ManifestField {
        editor: editor.clone(),
        error: None,
    });
    let text = manifest_dialog(&object, planned.acting.len());
    let pinned = object_lines(&object).len();
    let guard = planned.guard();
    let this = cx.weak_entity();
    let content = field.clone().into();
    let planned = Rc::new(RefCell::new(Some(planned)));
    let on_ok = move |window: &mut Window, cx: &mut App| {
        let typed = field.read(cx).editor.read(cx).value().to_string();
        if typed.trim().is_empty() {
            field.update(cx, |f, cx| {
                f.error = Some("Type the manifest to apply.".to_owned());
                cx.notify();
            });
            return false;
        }
        let Some(planned) = planned.borrow_mut().take() else {
            return true;
        };
        let object = object.clone();
        let action = FleetAction::ApplyYaml {
            manifest: FleetManifest::new(typed),
        };
        let _ = this.update(cx, |ws, cx| {
            start_preview(ws, planned, action, object, window, cx)
        });
        true
    };
    confirm::open_pinned_guarded(window, cx, text, pinned, Some(content), guard, on_ok);
    editor.update(cx, |editor, cx| editor.focus(window, cx));
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
    let choice = FleetChoice::from(&action);
    let plan = FleetPlan::new(action, object.clone(), planned.acting.clone()).and_then(|plan| {
        // The selected row's member is held to the object it shows, by uid. A member left out
        // before the preview is not reviewed at all.
        match &planned.shown {
            Some((cluster, uid)) if planned.acting.iter().any(|m| m.context() == cluster) => {
                plan.with_shown_uid(cluster, uid)
            }
            _ => Ok(plan),
        }
    });
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => {
            ws.refuse(refusal(choice, &error.to_string()), cx);
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
    if FleetChoice::from(preview.action()).drops_by_hand() {
        choose_targets(ws, planned, object, preview, window, cx);
        return;
    }
    // Left in, a refused member would keep the run from starting.
    preview.drop_refused();
    confirm_plan(ws, planned, object, preview, window, cx);
}

/// An apply's or a delete's target list: each member with its dry run, to keep in the run or
/// leave out, the failed ones left out already. Its confirmation goes on to the plan for the
/// members chosen; nothing runs from here.
fn choose_targets(
    ws: &mut Workspace,
    planned: Planned,
    object: FleetObject,
    preview: FleetPreview,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let names = ws.cluster_names(cx);
    let lines = plan_lines(&planned.order, &planned.refused, Some(&preview), |c| {
        names.exact(c)
    });
    let this = cx.weak_entity();
    // An apply can go back to its manifest, as typed, to fix what a dry run refused.
    let on_edit: Option<EditAgain> = match preview.action() {
        FleetAction::ApplyYaml { manifest } => {
            let draft = manifest.as_str().to_owned();
            let (planned, object, this) = (planned.clone(), object.clone(), this.clone());
            Some(Rc::new(move |window: &mut Window, cx: &mut App| {
                confirm::close_newest(window, cx);
                let (planned, object, draft) = (planned.clone(), object.clone(), draft.clone());
                let this = this.clone();
                window.defer(cx, move |window, cx| {
                    let _ =
                        this.update(cx, |_, cx| ask_manifest(planned, object, draft, window, cx));
                });
            }))
        }
        _ => None,
    };
    let targets = cx.new(|cx| FleetTargets::new(target_rows(&lines), on_edit, cx));
    let text = targets_dialog(preview.action(), &object);
    let guard = planned.guard();
    let read = targets.clone();
    let state = move |cx: &App| {
        let (note, blocked) = targets_state(&read.read(cx).rows);
        ConfirmState { note, blocked }
    };
    let content = targets.clone().into();
    let held = Rc::new(RefCell::new(Some((planned, object, preview))));
    let on_ok = move |window: &mut Window, cx: &mut App| {
        let Some((planned, object, mut preview)) = held.borrow_mut().take() else {
            return true;
        };
        // Every failed member stays out; every member not chosen is left out too.
        preview.drop_refused();
        for row in &targets.read(cx).rows {
            preview.set_dropped(&row.cluster, !row.chosen);
        }
        let this = this.clone();
        // Once this list has closed: the plan is a dialog of its own.
        window.defer(cx, move |window, cx| {
            let _ = this.update(cx, |ws, cx| {
                confirm_plan(ws, planned, object, preview, window, cx)
            });
        });
        true
    };
    confirm::open_gated_guarded(window, cx, text, content, guard, state, on_ok);
}

/// Goes back from a target list to the manifest editor.
type EditAgain = Rc<dyn Fn(&mut Window, &mut App)>;

/// The target list of an apply or a delete: a checkbox per member. Each checkbox performs a
/// registered action from this view's focus, as the drain options do.
pub(in crate::ui) struct FleetTargets {
    focus: FocusHandle,
    pub(in crate::ui) rows: Vec<TargetRow>,
    on_edit: Option<EditAgain>,
}

impl FleetTargets {
    fn new(rows: Vec<TargetRow>, on_edit: Option<EditAgain>, cx: &mut Context<Self>) -> Self {
        FleetTargets {
            focus: cx.focus_handle(),
            rows,
            on_edit,
        }
    }

    /// Every action of the list and its handler, for [`Bind`].
    pub(in crate::ui) fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<ToggleFleetTarget>(|this, action, _, cx| {
            // A member that cannot run stays out, however it is asked.
            if let Some(row) = this
                .rows
                .iter_mut()
                .find(|r| r.cluster == action.cluster && r.choosable)
            {
                row.chosen = !row.chosen;
                cx.notify();
            }
        })
        .bind::<EditFleetManifestAgain>(|this, _, window, cx| {
            if let Some(on_edit) = this.on_edit.clone() {
                on_edit(window, cx);
            }
        })
    }
}

impl Render for FleetTargets {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let element = v_flex()
            .id("fleet-targets")
            .track_focus(&self.focus)
            .mt_2()
            .gap_1()
            .children(self.rows.iter().enumerate().map(|(i, row)| {
                v_flex()
                    .child(
                        Checkbox::new(("fleet-target", i))
                            .label(row.shown.clone())
                            .checked(row.chosen)
                            .disabled(!row.choosable)
                            .on_click(registry::on_click_in(
                                self.focus.clone(),
                                ToggleFleetTarget {
                                    cluster: row.cluster.clone(),
                                },
                            )),
                    )
                    .child(
                        div()
                            .id(("fleet-target-says", i))
                            .aria_label(row.says.clone())
                            .pl_6()
                            .text_xs()
                            .text_color(muted)
                            .child(row.says.clone())
                            .test_support(),
                    )
            }))
            .when(self.on_edit.is_some(), |el| {
                el.child(
                    h_flex().pt_1().child(
                        Button::new("fleet-edit-again")
                            .small()
                            .label("Edit Manifest Again")
                            .on_click(registry::on_click_in(
                                self.focus.clone(),
                                EditFleetManifestAgain,
                            )),
                    ),
                )
            });
        Self::bind_actions(OnView::new(element, cx)).into_element()
    }
}

/// The one confirmation of a preview whose refused members are all left out.
fn confirm_plan(
    ws: &mut Workspace,
    planned: Planned,
    object: FleetObject,
    preview: FleetPreview,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let guard = planned.guard();
    // The question before this one could have been answered after the connection changed.
    if !guard.holds(cx) {
        ws.refuse_held(connection_changed_text(), cx);
        return;
    }
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
    let action = preview.action().clone();
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
        if let Err(why) = claim(key, running_write(&action), cx) {
            for key in &claimed {
                release(key, cx);
            }
            let choice = FleetChoice::from(&action);
            let _ = this.update(cx, |ws, cx| ws.refuse(refusal(choice, &why), cx));
            return;
        }
        claimed.push(key.clone());
    }
    let cancel = FleetCancel::new();
    let view = cx.new(|_| FleetRunView::new(result_lines(lines), cancel.clone()));
    let text = run_dialog(&action, object, keys.len());
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
