//! One sign-in of a running-set member, asked for from the set view.
//!
//! A member whose credential runs a program waits for a sign-in in the background unless it has
//! keep-watching consent (ADR-0012). The person can still sign it in once, now: the click is the
//! consent for that one start of its program, bound to the definition, server and program the
//! set view showed. It is held in memory only until the member's next attempt starts, at most
//! [`SIGN_IN_WINDOW`], and never saved. The connection it makes runs nothing more in the
//! background ([`CredentialAccess::signed_in_once`]): once its token is due, or the server
//! refuses it, the member waits for a sign-in again. It turns no background sign-in on.

// module-tests: keep_watching_tests.rs (and flint-e2e's member_sign_in.rs, on the fake API)

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::ClusterManager;
use crate::connection::member::{MemberEvent, MemberState, ServerIdentity};
use crate::connection::policy::{CredentialAccess, Role};
use crate::handle::{AdmittedMember, Event};
use crate::keep_watching::{PluginConsent, SignIn};

/// How long a sign-in asked for waits for the member's attempt to start. Attempts start within
/// seconds; one that starts later runs nothing, and the member asks for a sign-in again.
const SIGN_IN_WINDOW: Duration = Duration::from_secs(60);

/// A sign-in the person asked for, until the member's next attempt takes it.
pub(super) struct PendingSignIn {
    /// Sealed over the plugin definition the view showed, and the program it ran then.
    consent: Arc<PluginConsent>,
    /// The context's whole definition when asked.
    fingerprint: Option<u64>,
    server: ServerIdentity,
    asked: Instant,
}

impl ClusterManager {
    /// Signs member `member` in once, now, if it waits for a sign-in and is still exactly what
    /// the view showed: the member admitted to the open set, on the definition `definition`
    /// ([`KeepWatchView::definition`](crate::keep_watching::KeepWatchView)) and, for an exec
    /// plugin, the plugin definition `shown` ([`KeepWatchView::shown_sign_in`]
    /// (crate::keep_watching::KeepWatchView)). Anything else is refused and runs nothing.
    pub(super) fn sign_in_member(
        &mut self,
        member: &AdmittedMember,
        definition: Option<u64>,
        shown: Option<u64>,
    ) -> Result<(), String> {
        let id = member.context();
        let changed = || format!("\"{id}\" changed. Check its sign-in and try again.");
        if let Some(why) = self.member_changed(member) {
            return Err(why);
        }
        let Some(running) = self.members.get(id) else {
            return Err(format!("\"{id}\" is not in the open set."));
        };
        if *running.server() != ServerIdentity::new(member.server())
            || !self.member_on_its_server(id)
        {
            return Err(changed());
        }
        let waiting = matches!(running.state(), MemberState::SignInNeeded { .. });
        let paused = running.paused().is_some();
        let Some(slot) = self.cluster(id) else {
            return Err(format!("cluster \"{id}\" no longer exists"));
        };
        if paused
            || self.paused(id)
            || self.unsettled(id)
            || !self
                .contexts
                .shown(id, slot.info.server.as_deref())
                .confirmed
        {
            return Err(format!(
                "\"{id}\" can't sign in while a kubeconfig file defining it is unreadable."
            ));
        }
        if slot.fingerprint.is_none() || slot.fingerprint != definition {
            return Err(changed());
        }
        if self.disconnects.holds(id) {
            return Err(format!(
                "\"{id}\" is disconnected. Select it to connect first."
            ));
        }
        if self.active.as_deref() == Some(id) {
            return Err(format!(
                "\"{id}\" is selected: it signs in as the selected cluster."
            ));
        }
        if !waiting {
            return Err(format!("\"{id}\" is not waiting for a sign-in."));
        }
        let fingerprint = slot.fingerprint;
        match &*self.sign_in(id) {
            // Nothing runs: the attempt only tries the credential again.
            SignIn::Static => {}
            // Its consent covers this run, under the background sign-in's own rules.
            SignIn::Plugin(_) if self.member_consent(id).is_some() => {}
            SignIn::Plugin(_) => {
                let consent = self.seal_shown_plugin(id, shown.ok_or_else(changed)?)?;
                self.signing_in.insert(
                    id.to_owned(),
                    PendingSignIn {
                        consent: Arc::new(consent),
                        fingerprint,
                        server: ServerIdentity::new(member.server()),
                        asked: Instant::now(),
                    },
                );
            }
            SignIn::Provider => {
                return Err(format!(
                    "\"{id}\" signs in through an auth provider, which Flint runs only for the \
                     selected cluster. Select it to sign in."
                ));
            }
            SignIn::Unknown => return Err(format!("\"{id}\" has a user Flint can't read.")),
        }
        self.keep_watching_acted_on(id);
        let now = Instant::now();
        self.apply_member(id, MemberEvent::ConnectionLost, now);
        self.pump_members(now);
        Ok(())
    }

    /// Takes the sign-in asked for `id`, as the access of its attempt starting now in `role`, if
    /// it still holds: asked within [`SIGN_IN_WINDOW`], on the same definition and server,
    /// confirmed, and for the program the click saw. A selected member signs in as the selected
    /// cluster and uses none. Anything else leaves the attempt without it, so the plugin is
    /// refused and the member waits for a sign-in again.
    pub(super) fn take_member_sign_in(&mut self, id: &str, role: Role) -> Option<CredentialAccess> {
        let pending = self.signing_in.remove(id)?;
        if role != Role::Background {
            return None;
        }
        let slot = self.cluster(id)?;
        let in_time = pending.asked.elapsed() < SIGN_IN_WINDOW;
        let holds = slot.fingerprint == pending.fingerprint
            && self.member_on_its_server(id)
            && self
                .members
                .get(id)
                .is_some_and(|member| *member.server() == pending.server)
            && !self.paused(id)
            && !self.unsettled(id)
            && self
                .contexts
                .shown(id, slot.info.server.as_deref())
                .confirmed
            && matches!(
                &*self.sign_in(id),
                SignIn::Plugin(definition) if pending.consent.holds(definition)
            );
        if !in_time || !holds {
            let why = if holds {
                format!("\"{id}\" did not start its sign-in in time. Sign in again.")
            } else {
                format!("\"{id}\" changed before its sign-in started. Sign in again.")
            };
            self.emit(Event::Error(why));
            return None;
        }
        // The launch check reads the files again right before the program starts, as a
        // background sign-in's does.
        let check = self.launch_check(id, pending.consent);
        Some(CredentialAccess::signed_in_once(role, check))
    }

    /// Forgets a sign-in asked for `id` and not yet started.
    pub(super) fn drop_member_sign_in(&mut self, id: &str) {
        self.signing_in.remove(id);
    }

    /// Forgets every sign-in asked for and not yet started: what they were asked on is gone.
    pub(super) fn drop_member_sign_ins(&mut self) {
        self.signing_in.clear();
    }
}
