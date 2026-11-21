//! Whether the copy a detail pane shows can be presented as current, from its kind's sync state.
//!
//! This only decides what the pane says and when it fetches again. It is never asked before a
//! write: those go through the core's gate and read the live object.

use flint_core::SyncState;
use jiff::Timestamp;
use jiff::tz::TimeZone;

/// The kind's sync state as the pane has followed it.
pub(super) struct Freshness {
    state: SyncState,
    /// The kind has been current since the pane opened, so `Idle` or `Loading` now means its
    /// watch stopped or restarted rather than that it has not reported yet.
    seen_current: bool,
    /// The kind came back, and no fetch begun since then has succeeded.
    unconfirmed: bool,
}

impl Freshness {
    pub(super) fn new(state: SyncState) -> Self {
        Freshness {
            seen_current: state.is_current(),
            state,
            unconfirmed: false,
        }
    }

    /// Whether what the pane shows may be out of date.
    pub(super) fn is_stale(&self) -> bool {
        self.unconfirmed
            || match self.state {
                SyncState::Synced => false,
                SyncState::Idle | SyncState::Loading => self.seen_current,
                SyncState::Reconnecting { .. }
                | SyncState::RetryingRefusal { .. }
                | SyncState::Failed { .. }
                | SyncState::Unavailable => true,
            }
    }

    pub(super) fn state(&self) -> &SyncState {
        &self.state
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
        if !newer_wanted && self.state.is_current() {
            self.unconfirmed = false;
        }
    }

    /// The notice's first line, in the words the list uses for its rows.
    pub(super) fn headline(&self) -> String {
        const STALE: &str = "This object may be out of date";
        match &self.state {
            SyncState::Reconnecting { error: Some(e) }
            | SyncState::RetryingRefusal { error: e } => {
                format!("Reconnecting. {STALE}: {e}")
            }
            SyncState::Reconnecting { error: None } => format!("Reconnecting. {STALE}."),
            SyncState::Failed { error } => {
                format!("The list failed and is retried. {STALE}: {error}")
            }
            SyncState::Synced => format!("Connection restored. {STALE} until it is fetched."),
            SyncState::Idle | SyncState::Loading | SyncState::Unavailable => {
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
    use super::*;

    fn reconnecting() -> SyncState {
        SyncState::Reconnecting { error: None }
    }

    #[test]
    fn every_state_that_is_not_current_is_stale() {
        for state in [
            reconnecting(),
            SyncState::RetryingRefusal { error: "x".into() },
            SyncState::Failed { error: "x".into() },
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
    fn the_time_names_the_day_unless_it_is_today() {
        let at: Timestamp = "2026-01-02T03:04:05Z".parse().unwrap();
        let later: Timestamp = "2026-01-03T03:04:05Z".parse().unwrap();
        assert_eq!(clock(at, &TimeZone::UTC, at), "03:04:05");
        assert_eq!(clock(at, &TimeZone::UTC, later), "2026-01-02 03:04:05");
    }
}
