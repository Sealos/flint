//! The words of the keep-watching toggle (ADR-0012): the sidebar's badge, the menu entry and the
//! confirmation that names the command a cluster's sign-in will run in the background.

use flint_core::keep_watching::KeepWatching;

use super::confirm::DialogText;

/// The sidebar's word for `status`. Health badges are not part of it.
pub(super) fn badge(status: KeepWatching) -> Option<&'static str> {
    match status {
        KeepWatching::Watching => Some("watching"),
        KeepWatching::SignInNeeded | KeepWatching::NeedsConfirming => Some("sign-in needed"),
        KeepWatching::Off | KeepWatching::Paused => None,
    }
}

/// The toggle's label for `status`.
pub(super) fn toggle_label(status: KeepWatching) -> &'static str {
    match status {
        KeepWatching::Off => "Keep Watching",
        KeepWatching::NeedsConfirming => "Confirm Keep Watching…",
        KeepWatching::Watching | KeepWatching::SignInNeeded | KeepWatching::Paused => {
            "Stop Watching"
        }
    }
}

/// What the toggle's tooltip says about `status`.
pub(super) fn toggle_tip(status: KeepWatching) -> &'static str {
    match status {
        KeepWatching::Off => "Gather overview data and alerts while this cluster is not selected.",
        KeepWatching::Watching => "Watching in the background. Click to stop.",
        KeepWatching::SignInNeeded => {
            "Its sign-in failed, so nothing runs. Select the cluster to sign in, or click to stop."
        }
        KeepWatching::NeedsConfirming => {
            "Its user, command or server changed, so nothing runs until you confirm it again."
        }
        KeepWatching::Paused => {
            "Waiting: a kubeconfig file defining this cluster can't be read. Click to stop."
        }
    }
}

/// Whether the toggle turns watching on, rather than off.
pub(super) fn turns_on(status: KeepWatching) -> bool {
    matches!(status, KeepWatching::Off | KeepWatching::NeedsConfirming)
}

/// The confirmation before a cluster's exec plugin may run in the background, naming the command.
pub(super) fn plugin_dialog(context: &str, server: &str, command: &str) -> DialogText {
    let context = flint_core::display::single_line(context);
    let server = flint_core::redact::server_url(server);
    let command = flint_core::display::single_line(command);
    DialogText {
        title: "Run this sign-in in the background?".into(),
        text: format!(
            "To keep watching this cluster, Flint will run its sign-in command without asking, \
             at launch and whenever its token is due, while you work elsewhere. It cannot prompt \
             you. If it fails, Flint stops and shows that a sign-in is needed. If the command or \
             this context changes, Flint asks again.\n\n\
             Command: {command}\nContext: {context}\nServer: {server}"
        ),
        ok: "Keep Watching".into(),
        cancel: "Cancel".into(),
        id: "keep-watching-description",
        default_cancel: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidebar_only_says_watching_or_sign_in_needed() {
        assert_eq!(badge(KeepWatching::Watching), Some("watching"));
        assert_eq!(badge(KeepWatching::SignInNeeded), Some("sign-in needed"));
        assert_eq!(badge(KeepWatching::NeedsConfirming), Some("sign-in needed"));
        assert_eq!(badge(KeepWatching::Off), None);
        assert_eq!(badge(KeepWatching::Paused), None);
    }

    #[test]
    fn a_changed_definition_asks_again() {
        assert!(turns_on(KeepWatching::NeedsConfirming));
        assert_eq!(
            toggle_label(KeepWatching::NeedsConfirming),
            "Confirm Keep Watching…"
        );
        assert!(!turns_on(KeepWatching::SignInNeeded));
    }

    #[test]
    fn the_confirmation_names_the_command_and_defaults_to_cancel() {
        let dialog = plugin_dialog("prod", "https://a.example", "/usr/bin/aws eks get-token");
        assert!(dialog.text.contains("Command: /usr/bin/aws eks get-token"));
        assert!(dialog.text.contains("Context: prod"));
        assert!(dialog.default_cancel);
    }
}
