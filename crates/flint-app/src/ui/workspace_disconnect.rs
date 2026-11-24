//! Disconnect: the row menu's and the palette's way to leave a cluster. The core stops what it
//! runs for the cluster and ends its log streams; the shells are ended here, after asking.

use flint_core::ConnectionState;
use flint_core::keep_watching::KeepWatching;
use gpui_kit::*;

use super::super::confirm::{self, DialogText};
use super::super::dock_model::LiveSessions;
use super::super::registry::{ActionError, DisconnectCluster};
use super::Workspace;

/// What a shell's tab says once its cluster was disconnected.
const SHELL_ENDED: &str = "Disconnected from this cluster. Select it to connect again.";

/// The confirmation for ending `live` sessions, at least one a shell, by disconnecting `name`.
fn confirm_dialog(name: &str, live: LiveSessions) -> DialogText {
    let (verb, output) = match live.total() {
        1 => ("ends", "Its"),
        _ => ("end", "Their"),
    };
    DialogText {
        title: format!("Disconnect {name}?"),
        text: format!(
            "{} on this cluster {verb} now. {output} output stays in the dock, and the commands \
             do not come back. {name} stays disconnected until you select it again.",
            live.described(),
        ),
        ok: "Disconnect".into(),
        cancel: "Cancel".into(),
        id: "disconnect-cluster-description",
        // Ending a shell takes the button: Enter cancels.
        default_cancel: true,
    }
}

/// What the workspace says once `name` is disconnected.
fn done_note(name: &str, live: LiveSessions) -> String {
    if live.is_empty() {
        format!("Disconnected {name}. Select it to connect again.")
    } else {
        format!(
            "Disconnected {name} and ended {}. The output stays in the dock. Select it to \
             connect again.",
            live.described()
        )
    }
}

impl Workspace {
    /// Whether Flint runs anything for `cluster` that a disconnect would stop.
    pub(in crate::ui) fn can_disconnect(&self, cluster: &str, cx: &App) -> bool {
        let model = self.model.read(cx);
        let Some(summary) = self.cluster_summary(cluster, cx) else {
            return false;
        };
        !matches!(summary.state, ConnectionState::Disconnected)
            || model.active.as_deref() == Some(cluster)
            || (self.sets.has_member(Some(cluster)) && !self.sets.member_disconnected(cluster))
            || model
                .keep_watching(cluster)
                .is_some_and(|view| view.status != KeepWatching::Off)
            || !self.dock.read(cx).live_sessions_on(cluster, cx).is_empty()
    }

    pub(super) fn check_disconnect(&self, cluster: &str, cx: &App) -> Result<(), ActionError> {
        if self.sets.is_opening() {
            return Err(ActionError::rejected::<DisconnectCluster>(
                "a cluster set is opening",
            ));
        }
        if !self.can_disconnect(cluster, cx) {
            return Err(ActionError::rejected::<DisconnectCluster>(format!(
                "{cluster} is not connected"
            )));
        }
        Ok(())
    }

    /// Disconnects at once, unless that would end a live shell: then it asks first.
    pub(super) fn ask_disconnect(
        &mut self,
        action: &DisconnectCluster,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(e) = self.check_disconnect(&action.cluster, cx) {
            tracing::warn!("{e}");
            return;
        }
        let cluster = action.cluster.clone();
        let live = self.dock.read(cx).live_sessions_on(&cluster, cx);
        if live.shells == 0 {
            self.disconnect(&cluster, cx);
            return;
        }
        let name = self.cluster_names(cx).exact(&cluster);
        let view = cx.weak_entity();
        confirm::open(window, cx, confirm_dialog(&name, live), move |_, cx| {
            let _ = view.update(cx, |ws, cx| ws.disconnect(&cluster, cx));
        });
    }

    fn disconnect(&mut self, cluster: &str, cx: &mut Context<Self>) {
        let name = self.cluster_names(cx).exact(cluster);
        // Counted before the shells end. Log tabs are ended by the core.
        let live = self.dock.read(cx).live_sessions_on(cluster, cx);
        self.dock
            .update(cx, |dock, cx| dock.end_shells_on(cluster, SHELL_ENDED, cx));
        // Before the core answers, so losing the selection does not close an open set.
        self.sets.mark_disconnected(cluster);
        self.model
            .update(cx, |model, _| model.disconnect(cluster.to_owned()));
        let note = done_note(&name, live);
        if self.sets.member_disconnected(cluster) {
            self.discard_member(cluster, note, cx);
        } else {
            self.refuse(note, cx);
        }
    }
}

#[cfg(test)]
#[path = "workspace_disconnect_tests.rs"]
mod tests;
