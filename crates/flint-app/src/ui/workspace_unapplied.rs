//! Asking before an unapplied YAML edit is lost (#394). Whatever would drop the detail pane asks
//! here first, with Keep editing and Discard. Neither answer writes: applying stays on the
//! reviewed path in `detail_edit`.

// module-tests: workspace_unapplied_tests.rs

use std::rc::Rc;

use flint_core::display::single_line;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;

use super::{Workspace, connection_stopped};
use crate::ui::confirm::{self, DialogText};
use crate::ui::detail::{DetailView, ObjectRef};

/// The question while the edit's confirmed write is running.
const APPLYING: &str = "This edit is being applied, and discarding will not stop it. If the \
     server rejects it, your text will be lost.";

/// A pane kept after its list closed, because it holds an edit.
pub(super) struct HeldEdit {
    detail: Entity<DetailView>,
    /// Redraws the workspace when the edit ends, to close the pane.
    _notify: Subscription,
}

/// The question for an edit of `object`.
pub(super) fn dialog(object: &ObjectRef) -> DialogText {
    let name = match &object.namespace {
        Some(namespace) => format!("{}/{}", single_line(namespace), single_line(&object.name)),
        None => single_line(&object.name),
    };
    DialogText {
        title: "Discard unapplied edit?".into(),
        text: format!(
            "Your edit of {} {name} on {} has not been applied. Discarding it cannot be undone.",
            single_line(&object.kind_label),
            single_line(&object.cluster),
        ),
        ok: "Discard".into(),
        cancel: "Keep editing".into(),
        id: "unapplied-edit-description",
        // Losing typed text takes the button: Enter keeps it.
        default_cancel: true,
    }
}

impl Workspace {
    /// The detail pane, if leaving it would lose an unapplied edit.
    pub(in crate::ui) fn unapplied_edit(&self, cx: &App) -> Option<Entity<DetailView>> {
        self.detail
            .clone()
            .filter(|detail| detail.read(cx).has_unapplied_edit(cx))
    }

    /// Asks before leaving an unapplied edit: `true` if the caller must stop. Discard drops the
    /// edit and runs `then` once the dialog has closed; Keep editing changes nothing.
    pub(in crate::ui) fn asks_before_leaving(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> bool {
        let Some(detail) = self.unapplied_edit(cx) else {
            return false;
        };
        if window.has_active_dialog(cx) {
            return true;
        }
        self.list_in_front = false;
        detail.update(cx, |detail, cx| detail.reveal_edit(window, cx));
        let mut text = dialog(detail.read(cx).object());
        if detail.read(cx).is_applying_edit() {
            text.text = APPLYING.to_owned();
        }
        let workspace = cx.weak_entity();
        let then = Rc::new(then);
        confirm::open(window, cx, text, move |window, cx| {
            let (detail, then) = (detail.clone(), then.clone());
            let _ = workspace.update(cx, |ws, cx| {
                // The pane may have been replaced while the question was open. What was asked
                // for still goes on, and asks again about any other edit it would lose.
                if ws.detail.as_ref() == Some(&detail) {
                    detail.update(cx, |detail, cx| detail.discard_edit(window, cx));
                }
                cx.defer_in(window, move |ws, window, cx| then(ws, window, cx));
            });
        });
        cx.notify();
        true
    }

    /// Whether `cluster` is the one the open edit was made on.
    pub(in crate::ui) fn edit_is_on(&self, cluster: &str, cx: &App) -> bool {
        self.unapplied_edit(cx)
            .is_some_and(|detail| detail.read(cx).object().cluster == cluster)
    }

    /// Drops the detail pane with the list, unless that would lose an edit: then it stays, bound
    /// to what it was opened on, until the edit ends.
    pub(in crate::ui) fn drop_pane_unless_edited(&mut self, cx: &mut Context<Self>) {
        if let Some(detail) = self.unapplied_edit(cx) {
            if self.held_edit.as_ref().map(|held| &held.detail) != Some(&detail) {
                let notify = cx.observe(&detail, |_, _, cx| cx.notify());
                self.held_edit = Some(HeldEdit {
                    detail,
                    _notify: notify,
                });
            }
            return;
        }
        self.held_edit = None;
        self.detail = None;
        self._detail_subscriptions.clear();
    }

    /// Closes a pane that was kept only for its edit, once that edit has ended.
    pub(in crate::ui) fn close_finished_held_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(held) = &self.held_edit else {
            return;
        };
        if self.detail.as_ref() != Some(&held.detail) {
            self.held_edit = None;
        } else if !held.detail.read(cx).has_open_edit() {
            self.held_edit = None;
            self.detail = None;
            self._detail_subscriptions.clear();
            cx.defer_in(window, |ws, window, cx| ws.reclaim_focus(window, cx));
        }
    }

    /// The pane to draw on its own: its edit is open and nothing else would show it.
    pub(in crate::ui) fn edit_shown_alone(&self, cx: &App) -> Option<Entity<DetailView>> {
        let detail = self
            .detail
            .clone()
            .filter(|detail| detail.read(cx).has_open_edit())?;
        let stopped = self
            .model
            .read(cx)
            .active_cluster()
            .is_none_or(|c| connection_stopped(&c.state));
        (self.list.is_none() || stopped).then_some(detail)
    }

    /// Closes the detail pane (its close button, Esc), asking first.
    pub(in crate::ui) fn close_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.asks_before_leaving(window, cx, |ws, window, cx| ws.close_detail(window, cx)) {
            return;
        }
        let cleared = if let Some(run) = &self.sets.running {
            let selected = run.list.read(cx).selected().is_some();
            run.list.update(cx, |list, cx| list.clear_selection(cx));
            selected
        } else if let Some(list) = &self.list {
            let selected = list.read(cx).has_selection();
            list.update(cx, |list, cx| list.clear_selection(cx));
            selected
        } else {
            false
        };
        // An edit kept across a reconnect has no row selected to clear.
        if !cleared {
            self.detail = None;
            self._detail_subscriptions.clear();
            self.reclaim_focus(window, cx);
            cx.notify();
        }
    }
}
