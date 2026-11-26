//! Why a member of an open cluster set cannot show rows, and what the set view offers for it.
//!
//! A signed-out member is said so in one place only: its chip beside the list, or, when no
//! member can show rows, one block with a row per member (its name, its cause and its action).

use flint_core::handle::{MemberReport, MemberStatus};
use flint_core::settings::ClusterPin;
use flint_core::{ClusterId, ClusterSummary};

use super::registry::SelectCluster;

/// Why a member's rows cannot be shown or acted on now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemberCause {
    /// Its kubeconfig entry is gone.
    Removed,
    /// A kubeconfig file defining it can't be read, and it was never confirmed.
    Provisional,
    /// It points at another server than the one admitted.
    Moved,
    /// A kubeconfig file defining it can't be read: nothing new starts.
    Paused,
    /// Its credential needs a sign-in, which only selecting it retries.
    SignIn,
    /// Its connection failed and is retried after a backoff.
    Failed,
    /// The person disconnected it: its rows are gone until the set is opened again.
    Disconnected,
    /// It left the server it was admitted on: its rows are gone until the set is opened again.
    Changed,
}

/// What a row of the empty-state block offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemberAction {
    SignIn,
    Retry,
}

impl MemberAction {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::SignIn => "Sign in",
            Self::Retry => "Retry",
        }
    }
}

impl MemberCause {
    /// Why a row of `shown` cannot be opened or acted on, for a refusal. `None` for a cause that
    /// only keeps rows off the screen: a failed connection is retried by itself.
    pub(super) fn refusal(self, shown: &str) -> Option<String> {
        Some(match self {
            Self::Removed => format!("{shown} is no longer in kubeconfig"),
            Self::Provisional => {
                format!("{shown} is provisional: a kubeconfig file defining it can't be read")
            }
            Self::Moved => format!("{shown} now points at another server"),
            Self::Paused => paused_text(shown),
            Self::SignIn => format!("{shown} needs sign-in: select it in the sidebar"),
            Self::Failed => return None,
            Self::Disconnected => format!("{shown} was disconnected. Open the set again."),
            Self::Changed => {
                format!("{shown} changed since this set opened. Open the set again.")
            }
        })
    }

    /// The cause as one row of the empty-state block says it.
    pub(super) fn line(self) -> &'static str {
        match self {
            Self::Removed => "No longer in kubeconfig",
            Self::Provisional => "Provisional: a kubeconfig file defining it can't be read",
            Self::Moved => "Now points at another server",
            Self::Paused => "Paused: a kubeconfig file defining it can't be read",
            Self::SignIn => "Sign-in needed",
            Self::Failed => "Connection failed",
            Self::Disconnected => "Disconnected: open the set again",
            Self::Changed => "Changed since the set opened: open the set again",
        }
    }

    pub(super) fn action(self) -> Option<MemberAction> {
        match self {
            Self::SignIn => Some(MemberAction::SignIn),
            Self::Failed => Some(MemberAction::Retry),
            Self::Removed
            | Self::Provisional
            | Self::Moved
            | Self::Paused
            | Self::Disconnected
            | Self::Changed => None,
        }
    }
}

pub(super) fn paused_text(shown: &str) -> String {
    format!("{shown} is paused: a kubeconfig file defining it can't be read")
}

/// The cause that closes a row of `pin`'s cluster, if any. Protection is not one: a protected
/// cluster's rows open read-only and the write path refuses their actions.
pub(super) fn refusal_cause(
    pin: &ClusterPin,
    summary: Option<&ClusterSummary>,
    report: Option<&MemberReport>,
) -> Option<MemberCause> {
    let Some(summary) = summary else {
        return Some(MemberCause::Removed);
    };
    if !summary.confirmed {
        return Some(MemberCause::Provisional);
    }
    let on_pin = summary
        .info
        .server
        .as_deref()
        .is_some_and(|server| pin.matches(&pin.context, server));
    if !on_pin {
        return Some(MemberCause::Moved);
    }
    if summary.paused || report.is_some_and(|r| r.paused) {
        return Some(MemberCause::Paused);
    }
    match &report?.status {
        MemberStatus::SignInNeeded { .. } => Some(MemberCause::SignIn),
        _ => None,
    }
}

/// Failed when `report` says the member's connection did: it shows no rows, though a row it
/// held could still be opened.
pub(super) fn failed_cause(report: Option<&MemberReport>) -> Option<MemberCause> {
    report
        .is_some_and(|r| matches!(r.status, MemberStatus::Failed { .. }))
        .then_some(MemberCause::Failed)
}

/// One member of the empty-state block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MemberLine {
    pub(super) cluster: ClusterId,
    /// The member's full name on screen.
    pub(super) shown: String,
    pub(super) cause: MemberCause,
}

/// A member with its full name on screen, and the cause that keeps it from showing rows.
pub(super) type MemberCauses = Vec<(ClusterId, String, Option<MemberCause>)>;

/// The empty-state block's rows: one per member, when none can show rows (`members` holds a
/// cause for every member). Otherwise `None`, and the list stays with its member chips.
pub(super) fn empty_state(members: MemberCauses) -> Option<Vec<MemberLine>> {
    let lines: Option<Vec<MemberLine>> = members
        .into_iter()
        .map(|(cluster, shown, cause)| {
            Some(MemberLine {
                cluster,
                shown,
                cause: cause?,
            })
        })
        .collect();
    lines.filter(|lines| !lines.is_empty())
}

/// The action of every "Sign in" in the set view: the chip of a signed-out member and a row of
/// the empty-state block, each wrapped in `registry::on_click`. For now it selects the member in
/// the sidebar, which is where a sign-in command runs; the one-shot sign-in replaces this body.
pub(super) fn sign_in_member(cluster: &str) -> SelectCluster {
    SelectCluster {
        cluster: cluster.to_owned(),
    }
}

/// What Retry does for a failed member: selects it, which connects it and shows its own retry.
pub(super) fn retry_member(cluster: &str) -> SelectCluster {
    SelectCluster {
        cluster: cluster.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(causes: &[Option<MemberCause>]) -> Option<Vec<MemberLine>> {
        empty_state(
            causes
                .iter()
                .enumerate()
                .map(|(i, cause)| (format!("c{i}"), format!("cluster-{i}"), *cause))
                .collect(),
        )
    }

    fn actions(lines: Option<Vec<MemberLine>>) -> Vec<Option<MemberAction>> {
        lines
            .expect("the block")
            .into_iter()
            .map(|l| l.cause.action())
            .collect()
    }

    #[test]
    fn signed_out_members_get_one_row_each_with_a_sign_in() {
        let lines = block(&[Some(MemberCause::SignIn), Some(MemberCause::SignIn)]).unwrap();
        let shown: Vec<&str> = lines.iter().map(|l| l.shown.as_str()).collect();
        assert_eq!(shown, ["cluster-0", "cluster-1"]);
        assert!(lines.iter().all(|l| l.cause.line() == "Sign-in needed"));
        assert!(
            lines
                .iter()
                .all(|l| l.cause.action() == Some(MemberAction::SignIn))
        );
        assert_eq!(MemberAction::SignIn.label(), "Sign in");
    }

    #[test]
    fn paused_members_say_why_and_offer_nothing() {
        let found = actions(block(&[
            Some(MemberCause::Paused),
            Some(MemberCause::Provisional),
        ]));
        assert_eq!(found, [None, None]);
        assert!(MemberCause::Paused.line().starts_with("Paused"));
    }

    #[test]
    fn failed_members_offer_retry() {
        let found = actions(block(&[
            Some(MemberCause::Failed),
            Some(MemberCause::Failed),
        ]));
        assert_eq!(found, [Some(MemberAction::Retry); 2]);
        assert_eq!(MemberAction::Retry.label(), "Retry");
    }

    #[test]
    fn mixed_causes_each_keep_their_own_row_and_action() {
        let found = actions(block(&[
            Some(MemberCause::SignIn),
            Some(MemberCause::Paused),
            Some(MemberCause::Failed),
        ]));
        assert_eq!(
            found,
            [Some(MemberAction::SignIn), None, Some(MemberAction::Retry)]
        );
    }

    #[test]
    fn a_member_that_can_show_rows_keeps_the_list() {
        assert_eq!(block(&[Some(MemberCause::SignIn), None]), None);
        assert_eq!(block(&[]), None);
    }

    #[test]
    fn only_a_failed_connection_is_no_reason_to_refuse_a_row() {
        assert_eq!(MemberCause::Failed.refusal("a"), None);
        assert!(
            MemberCause::SignIn
                .refusal("a")
                .unwrap()
                .contains("sidebar")
        );
    }
}
