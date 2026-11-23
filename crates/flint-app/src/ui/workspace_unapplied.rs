//! Asking before an unapplied YAML edit is lost (#394). Whatever would drop the detail pane asks
//! here first, with Keep editing and Discard. Neither answer writes: applying stays on the
//! reviewed path in `detail_edit`.

// module-tests: workspace_unapplied_tests.rs

use std::rc::Rc;

use flint_core::display::single_line;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;

use super::Workspace;
use crate::ui::confirm::{self, DialogText};
use crate::ui::detail::{DetailView, ObjectRef};

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
        let text = dialog(detail.read(cx).object());
        let workspace = cx.weak_entity();
        let then = Rc::new(then);
        confirm::open(window, cx, text, move |window, cx| {
            let (detail, then) = (detail.clone(), then.clone());
            let _ = workspace.update(cx, |ws, cx| {
                // The pane may have been replaced while the question was open.
                if ws.detail.as_ref() != Some(&detail) {
                    return;
                }
                detail.update(cx, |detail, cx| detail.discard_edit(window, cx));
                cx.defer_in(window, move |ws, window, cx| then(ws, window, cx));
            });
        });
        cx.notify();
        true
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

    /// Takes the pane out of the way of a list that restarts on `cluster`, when it holds an
    /// unapplied edit of that cluster. The edit stays bound to what it was opened on, so the
    /// write gate refuses it if the context now points elsewhere.
    pub(in crate::ui) fn hold_edit_across_restart(
        &mut self,
        cluster: Option<&str>,
        cx: &App,
    ) -> Option<(Entity<DetailView>, Vec<Subscription>)> {
        let detail = self.unapplied_edit(cx)?;
        if Some(detail.read(cx).object().cluster.as_str()) != cluster {
            return None;
        }
        self.detail = None;
        Some((detail, std::mem::take(&mut self._detail_subscriptions)))
    }

    pub(in crate::ui) fn restore_held_edit(
        &mut self,
        held: Option<(Entity<DetailView>, Vec<Subscription>)>,
    ) {
        if let Some((detail, subscriptions)) = held {
            self.detail = Some(detail);
            self._detail_subscriptions = subscriptions;
        }
    }
}
