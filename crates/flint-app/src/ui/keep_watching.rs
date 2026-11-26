//! The words of the keep-watching controls (ADR-0012): the sidebar's badge, the menu entry, and
//! the toggle that lets a cluster's sign-in command run in the background.

use flint_core::keep_watching::{BackgroundSignIn, KeepWatching};

/// What the background sign-in toggle says it does.
pub(super) const SIGN_IN_LABEL: &str =
    "Let Flint run this cluster's sign-in command in the background";

/// Why a build with the automation channel can't turn the toggle on, if this is one. Any local
/// process can press keys through that channel, so a key on the focused toggle proves no person.
pub(super) const SIGN_IN_BLOCKED: Option<&str> = if cfg!(feature = "automation") {
    Some("Background sign-in can't be turned on in a build with the automation channel.")
} else {
    None
};

/// The sidebar's word for `status`. Health badges are not part of it.
pub(super) fn badge(status: KeepWatching) -> Option<&'static str> {
    match status {
        KeepWatching::Watching => Some("watching"),
        KeepWatching::SignInNeeded | KeepWatching::NeedsConfirming => Some("sign-in needed"),
        KeepWatching::Off | KeepWatching::Paused => None,
    }
}

/// The keep-watching control's label.
pub(super) fn toggle_label(status: KeepWatching, sign_in: BackgroundSignIn) -> &'static str {
    match status {
        KeepWatching::Off => "Keep Watching",
        KeepWatching::NeedsConfirming if turns_on(status, sign_in) => "Confirm Keep Watching…",
        KeepWatching::NeedsConfirming
        | KeepWatching::Watching
        | KeepWatching::SignInNeeded
        | KeepWatching::Paused => "Stop Watching",
    }
}

/// What the control's tooltip says about `status`.
pub(super) fn toggle_tip(status: KeepWatching) -> &'static str {
    match status {
        KeepWatching::Off => "Gather overview data and alerts while this cluster is not selected.",
        KeepWatching::Watching => "Watching in the background. Click to stop.",
        KeepWatching::SignInNeeded => {
            "Its sign-in failed, so nothing runs. Select the cluster to sign in, or click to stop."
        }
        KeepWatching::NeedsConfirming => {
            "Nothing runs until you allow its sign-in, which a change or a restart turns off."
        }
        KeepWatching::Paused => {
            "Waiting: a kubeconfig file defining this cluster can't be read. Click to stop."
        }
    }
}

/// Whether the control turns watching on, rather than off. A cluster waiting for its sign-in
/// toggle is already on: only that toggle can let it run.
pub(super) fn turns_on(status: KeepWatching, sign_in: BackgroundSignIn) -> bool {
    match status {
        KeepWatching::Off => true,
        KeepWatching::NeedsConfirming => {
            !matches!(sign_in, BackgroundSignIn::Off | BackgroundSignIn::On)
        }
        _ => false,
    }
}

/// Whether the sign-in toggle can be used now. Turning it off always can.
pub(super) fn sign_in_enabled(
    status: KeepWatching,
    sign_in: BackgroundSignIn,
    refused: bool,
) -> bool {
    match sign_in {
        BackgroundSignIn::On => true,
        BackgroundSignIn::Off => status != KeepWatching::Off && !refused,
        BackgroundSignIn::NotNeeded | BackgroundSignIn::Unavailable => false,
    }
}

/// What the sign-in toggle's tooltip says.
pub(super) fn sign_in_tip(status: KeepWatching, sign_in: BackgroundSignIn) -> &'static str {
    match sign_in {
        BackgroundSignIn::Unavailable => {
            "Not available: this cluster's sign-in can't be stopped once it runs, or can't be read."
        }
        _ if status == KeepWatching::Off => "Turn on Keep Watching first.",
        _ => {
            "Runs without a terminal or input until you quit Flint. Flint cannot stop a command \
             from opening a browser by itself. A change to this context turns it off."
        }
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
    fn a_cluster_waiting_for_its_sign_in_toggle_can_be_stopped() {
        let waiting = KeepWatching::NeedsConfirming;
        assert!(!turns_on(waiting, BackgroundSignIn::Off));
        assert_eq!(
            toggle_label(waiting, BackgroundSignIn::Off),
            "Stop Watching"
        );
        assert!(turns_on(waiting, BackgroundSignIn::NotNeeded));
        assert_eq!(
            toggle_label(waiting, BackgroundSignIn::NotNeeded),
            "Confirm Keep Watching…"
        );
        assert!(!turns_on(KeepWatching::SignInNeeded, BackgroundSignIn::On));
    }

    #[test]
    fn the_sign_in_toggle_is_disabled_when_it_cannot_be_turned_on() {
        let waiting = KeepWatching::NeedsConfirming;
        assert!(sign_in_enabled(waiting, BackgroundSignIn::Off, false));
        assert!(!sign_in_enabled(waiting, BackgroundSignIn::Off, true));
        assert!(!sign_in_enabled(
            KeepWatching::Off,
            BackgroundSignIn::Off,
            false
        ));
        assert!(!sign_in_enabled(
            waiting,
            BackgroundSignIn::Unavailable,
            false
        ));
        assert!(sign_in_enabled(
            KeepWatching::Paused,
            BackgroundSignIn::On,
            true
        ));
    }

    // Consent comes from the toggle alone: no registered action, and so no palette command, key
    // binding or automation `dispatch`, reaches what grants it.
    #[test]
    fn no_action_palette_or_automation_path_can_grant_the_sign_in() {
        let elsewhere = [
            include_str!("registry.rs"),
            include_str!("palette.rs"),
            include_str!("workspace_actions.rs"),
            include_str!("workspace_keys.rs"),
            include_str!("row_menu.rs"),
            #[cfg(feature = "automation")]
            include_str!("automation/commands.rs"),
            #[cfg(feature = "automation")]
            include_str!("automation/protocol.rs"),
            #[cfg(feature = "automation")]
            include_str!("automation/mod.rs"),
        ];
        for grant in [
            "turn_background_sign_in",
            "offer_keep_watching",
            "ApproveKeepWatching",
            ".approve()",
            // A set member's one sign-in: its Sign in click alone.
            "sign_in_set_member",
            "sign_in_member",
            "MemberSignIn",
        ] {
            for source in elsewhere {
                let code = source.split("#[cfg(test)]").next().unwrap_or(source);
                assert!(!code.contains(grant), "{grant} is reachable");
            }
        }
        for spec in super::super::registry::actions() {
            assert!(!spec.name.contains("SignIn"), "{}", spec.name);
        }
        let callers = include_str!("workspace.rs")
            .matches(".turn_background_sign_in(")
            .count();
        assert_eq!(callers, 1, "only the toggle's handler");
    }
}
