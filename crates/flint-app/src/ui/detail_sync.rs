//! Whether the copy a detail pane shows can be presented as current, from its kind's sync state.
//!
//! This only decides what the pane says and when it fetches again. It is never asked before a
//! write: those go through the core's gate and read the live object.

use std::time::Duration;

use flint_core::SyncState;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;

use super::ObjectRef;

/// What a pane with no copy says when its object's name now belongs to another object.
pub(super) const REPLACED: &str =
    "This object was deleted and a new one has its name. Select the new one in the list.";

/// Whether `fetched` is a different object from the one the pane is for. A row of an API that
/// sends no uid is keyed by its name, which identifies nothing, and a fetched object without a
/// uid proves nothing either, so neither is compared.
pub(super) fn is_other_object(object: &ObjectRef, fetched: &Value) -> bool {
    let uid = &*object.uid;
    let keyed_by_name = uid.contains('/') || uid == object.name;
    !keyed_by_name
        && fetched
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            .is_some_and(|fetched| fetched != uid)
}

/// How long after automatic fetch number `retries` (from 0) the next may start.
pub(super) fn error_retry_delay(retries: u32) -> Duration {
    const FIRST_SECS: u64 = 2;
    const MAX_SECS: u64 = 60;
    Duration::from_secs(
        FIRST_SECS
            .saturating_mul(1u64 << retries.min(16))
            .min(MAX_SECS),
    )
}

/// The kind's sync state as the pane has followed it.
pub(super) struct Freshness {
    state: SyncState,
    /// The kind has been current since the pane opened, so `Idle` or `Loading` now means its
    /// watch stopped or restarted rather than that it has not reported yet.
    seen_current: bool,
    /// The kind came back, and no fetch begun since then has succeeded.
    unconfirmed: bool,
    /// A fetch has succeeded, so a `Loading` kind is not just a pane waiting for its first list:
    /// a namespace-limited list can publish rows while another of its namespaces still loads.
    holds_copy: bool,
}

impl Freshness {
    pub(super) fn new(state: SyncState) -> Self {
        Freshness {
            seen_current: state.is_current(),
            state,
            unconfirmed: false,
            holds_copy: false,
        }
    }

    /// Whether what the pane shows may be out of date.
    pub(super) fn is_stale(&self) -> bool {
        self.unconfirmed
            || match self.state {
                SyncState::Synced => false,
                SyncState::Idle => self.seen_current,
                SyncState::Loading => self.seen_current || self.holds_copy,
                SyncState::Reconnecting { .. }
                | SyncState::RetryingRefusal { .. }
                | SyncState::Failed { .. }
                | SyncState::Unavailable => true,
            }
    }

    pub(super) fn state(&self) -> &SyncState {
        &self.state
    }

    /// The connection is down, so a pane with no copy says so instead of loading or failing.
    pub(super) fn is_reconnecting(&self) -> bool {
        matches!(self.state, SyncState::Reconnecting { error: Some(_) })
    }

    /// Follows the kind to `state`. Returns whether the pane must fetch again: the kind became
    /// current while the pane was stale or held a fetch error (`holds_error`).
    pub(super) fn update(&mut self, state: SyncState, holds_error: bool) -> bool {
        let returned = state.is_current() && !self.state.is_current();
        let refetch = returned && (self.is_stale() || holds_error);
        self.seen_current |= state.is_current();
        self.state = state;
        self.unconfirmed |= refetch;
        refetch
    }

    /// A fetch succeeded. `newer_wanted`: another one is due, because this one may have begun
    /// before the kind came back, so it confirms nothing.
    pub(super) fn fetched(&mut self, newer_wanted: bool) {
        self.holds_copy = true;
        if !newer_wanted && self.state.is_current() {
            self.unconfirmed = false;
        }
    }

    /// The notice's first line, in the words the list uses for its rows.
    pub(super) fn headline(&self) -> String {
        self.headline_with(str::to_owned)
    }

    /// [`Self::headline`], with the server's error text passed through `show`.
    pub(super) fn headline_with(&self, show: impl Fn(&str) -> String) -> String {
        const STALE: &str = "This object may be out of date";
        match &self.state {
            SyncState::Reconnecting { error: Some(e) }
            | SyncState::RetryingRefusal { error: e, .. } => {
                format!("Reconnecting. {STALE}: {}", show(e))
            }
            SyncState::Reconnecting { error: None } => format!("Reconnecting. {STALE}."),
            SyncState::Failed { error, .. } => {
                format!("The list failed and is retried. {STALE}: {}", show(error))
            }
            SyncState::Synced => format!("Connection restored. {STALE} until it is fetched."),
            SyncState::Loading => format!("Still loading the list. {STALE}."),
            SyncState::Idle | SyncState::Unavailable => {
                format!("Not syncing. {STALE}.")
            }
        }
    }
}

/// The notice's second line. An absolute time, because nothing redraws the pane during an
/// outage and a relative one would stop counting.
pub(super) fn last_updated(at: Option<Timestamp>) -> String {
    match at {
        Some(at) => format!(
            "Last updated {}. Ages are as of then.",
            clock(at, &TimeZone::system(), Timestamp::now())
        ),
        None => "Last updated: unknown.".to_owned(),
    }
}

fn clock(at: Timestamp, tz: &TimeZone, now: Timestamp) -> String {
    let local = at.to_zoned(tz.clone());
    if local.date() == now.to_zoned(tz.clone()).date() {
        local.strftime("%H:%M:%S").to_string()
    } else {
        local.strftime("%Y-%m-%d %H:%M:%S").to_string()
    }
}

#[cfg(test)]
mod tests {
    use flint_core::RefusalCause;

    use super::*;

    fn reconnecting() -> SyncState {
        SyncState::Reconnecting { error: None }
    }

    #[test]
    fn every_state_that_is_not_current_is_stale() {
        for state in [
            reconnecting(),
            SyncState::RetryingRefusal {
                error: "x".into(),
                cause: Some(RefusalCause::Denied),
            },
            SyncState::Failed {
                error: "x".into(),
                cause: Some(RefusalCause::Denied),
            },
            SyncState::Unavailable,
        ] {
            assert!(Freshness::new(state.clone()).is_stale(), "{state:?}");
        }
        assert!(!Freshness::new(SyncState::Synced).is_stale());
    }

    #[test]
    fn idle_is_stale_only_after_the_kind_was_current() {
        assert!(!Freshness::new(SyncState::Idle).is_stale());
        let mut f = Freshness::new(SyncState::Synced);
        assert!(!f.update(SyncState::Idle, false));
        assert!(f.is_stale());
    }

    #[test]
    fn a_return_refetches_once_and_stays_stale_until_a_later_fetch() {
        let mut f = Freshness::new(SyncState::Synced);
        f.update(reconnecting(), false);
        assert!(f.update(SyncState::Synced, false));
        assert!(
            !f.update(SyncState::Synced, false),
            "only on the transition"
        );
        assert!(f.is_stale());
        f.fetched(true);
        assert!(f.is_stale(), "that fetch may predate the return");
        f.fetched(false);
        assert!(!f.is_stale());
    }

    #[test]
    fn a_fetch_during_the_outage_does_not_clear_it() {
        let mut f = Freshness::new(reconnecting());
        f.fetched(false);
        assert!(f.is_stale());
    }

    #[test]
    fn a_held_error_refetches_on_the_first_current_state() {
        let mut f = Freshness::new(SyncState::Idle);
        assert!(f.update(SyncState::Synced, true));
        let mut f = Freshness::new(SyncState::Idle);
        assert!(!f.update(SyncState::Synced, false));
        assert!(!f.is_stale());
    }

    #[test]
    fn loading_is_stale_once_a_fetched_copy_is_shown() {
        let mut f = Freshness::new(SyncState::Loading);
        assert!(!f.is_stale(), "nothing is shown yet");
        f.fetched(false);
        assert!(f.is_stale());
        assert!(f.headline().contains("loading"));
        assert!(f.update(SyncState::Synced, false), "fetches on the return");
        f.fetched(false);
        assert!(!f.is_stale());
    }

    #[test]
    fn only_a_connection_error_counts_as_reconnecting() {
        let down = SyncState::Reconnecting {
            error: Some("dropped".into()),
        };
        assert!(Freshness::new(down).is_reconnecting());
        for state in [reconnecting(), SyncState::Loading, SyncState::Synced] {
            assert!(!Freshness::new(state).is_reconnecting());
        }
    }

    #[test]
    fn only_the_server_error_goes_through_show() {
        let f = Freshness::new(SyncState::Failed {
            error: "secret".into(),
            cause: Some(RefusalCause::Denied),
        });
        let text = f.headline_with(|_| "hidden".to_owned());
        assert!(text.ends_with(": hidden"), "{text}");
    }

    #[test]
    fn error_retries_space_out_up_to_a_limit() {
        assert_eq!(error_retry_delay(0), Duration::from_secs(2));
        assert_eq!(error_retry_delay(1), Duration::from_secs(4));
        assert_eq!(error_retry_delay(5), Duration::from_secs(60));
        assert_eq!(error_retry_delay(u32::MAX), Duration::from_secs(60));
    }

    #[test]
    fn another_uid_is_another_object_unless_the_row_has_no_uid() {
        let object = |uid: &str| ObjectRef {
            cluster: "c".into(),
            kind: flint_core::KindId::pods(),
            kind_label: "Pods".into(),
            namespace: Some("demo".into()),
            name: "api".into(),
            uid: uid.into(),
        };
        let with_uid = serde_json::json!({"metadata": {"uid": "new"}});
        let without = serde_json::json!({"metadata": {}});
        assert!(is_other_object(&object("old"), &with_uid));
        assert!(!is_other_object(&object("new"), &with_uid));
        assert!(
            !is_other_object(&object("old"), &without),
            "nothing to compare"
        );
        assert!(!is_other_object(&object("api"), &with_uid));
        assert!(!is_other_object(&object("demo/api"), &with_uid));
    }

    #[test]
    fn the_time_names_the_day_unless_it_is_today() {
        let at: Timestamp = "2026-01-02T03:04:05Z".parse().unwrap();
        let later: Timestamp = "2026-01-03T03:04:05Z".parse().unwrap();
        assert_eq!(clock(at, &TimeZone::UTC, at), "03:04:05");
        assert_eq!(clock(at, &TimeZone::UTC, later), "2026-01-02 03:04:05");
    }
}
