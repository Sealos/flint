//! One sign-in of a cluster, asked for from the set view (a running-set member) or from its
//! fleet home card (a cluster kept watching).
//!
//! A cluster whose credential runs a program waits for a sign-in in the background unless it has
//! keep-watching consent (ADR-0012). The person can still sign it in once, now: the click is the
//! consent for that one start of its program, bound to the definition, server and program the
//! view showed. It is held in memory only until the program starts, at most [`SIGN_IN_WINDOW`]
//! after the click, and never saved. The connection it makes runs nothing more in the background
//! ([`CredentialAccess::signed_in_once`]): once its token is due, or the server refuses it, the
//! cluster waits for a sign-in again. It turns no background sign-in on.

// module-tests: keep_watching_tests.rs (and watched_sign_in_tests.rs; flint-e2e's
// member_sign_in.rs, on the fake API)

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{ClusterId, ClusterManager, Internal};
use crate::cluster::SendCheck;
use crate::connection::member::{MemberEvent, MemberState, ServerIdentity};
use crate::connection::policy::{CredentialAccess, Role};
use crate::handle::ConnectionState;
use crate::handle::{AdmittedMember, Event};
use crate::keep_watching::{KeepWatching, PluginConsent, SignIn};

/// How long a sign-in asked for waits for its program to start. Attempts start within seconds;
/// one that starts later runs nothing, and the cluster asks for a sign-in again.
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
    /// the view showed: the member admitted to the open set, on the definition `definition` and,
    /// for an exec plugin, the plugin definition `shown`, both as its
    /// [`KeepWatchView`](crate::keep_watching::KeepWatchView) names them. Anything else is
    /// refused and runs nothing.
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
        if let Some(why) = self.sign_in_held(id, definition, paused) {
            return Err(why);
        }
        if !waiting {
            return Err(format!("\"{id}\" is not waiting for a sign-in."));
        }
        let fingerprint = self.cluster(id).and_then(|slot| slot.fingerprint);
        match &*self.sign_in(id) {
            // Nothing runs: the attempt only tries the credential again.
            SignIn::Static => {}
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
        let now = Instant::now();
        self.apply_member(id, MemberEvent::ConnectionLost, now);
        self.pump_members(now);
        Ok(())
    }

    /// Why `id` can't sign in once now, if so: held under an old proxy setting, paused (also
    /// when `paused` says so), not confirmed, on another definition than `definition`,
    /// disconnected, or selected. Both entry points ask this.
    fn sign_in_held(&self, id: &str, definition: Option<u64>, paused: bool) -> Option<String> {
        let Some(slot) = self.cluster(id) else {
            return Some(format!("cluster \"{id}\" no longer exists"));
        };
        // Loaded under a proxy setting no longer in use: nothing starts on it until a load
        // replaces it.
        if slot.proxy_generation != crate::connection::proxy::generation() {
            return Some(format!(
                "\"{id}\" is reconnecting under the new proxy setting. Try again once it has."
            ));
        }
        if paused
            || self.paused(id)
            || self.unsettled(id)
            || !self
                .contexts
                .shown(id, slot.info.server.as_deref())
                .confirmed
        {
            return Some(format!(
                "\"{id}\" can't sign in while a kubeconfig file defining it is unreadable."
            ));
        }
        if slot.fingerprint.is_none() || slot.fingerprint != definition {
            return Some(format!(
                "\"{id}\" changed. Check its sign-in and try again."
            ));
        }
        if self.disconnects.holds(id) {
            return Some(format!(
                "\"{id}\" is disconnected. Select it to connect first."
            ));
        }
        if self.active.as_deref() == Some(id) {
            return Some(format!(
                "\"{id}\" is selected: it signs in as the selected cluster."
            ));
        }
        None
    }

    /// Signs cluster `id`, kept watching, in once, now, from its fleet home card, if it waits for
    /// a sign-in and is still exactly what the card showed: at `server`, on the definition
    /// `definition` and the plugin definition `shown`, as its
    /// [`KeepWatchView`](crate::keep_watching::KeepWatchView) names them. Its kept connection
    /// starts at once, with access for that one start of the plugin
    /// ([`Self::start_kept_once`]). `revoked` is the count of `id`'s ended grants at the click:
    /// a stop or disconnect since refuses it. Anything else is refused and runs nothing. A
    /// member of the open set signs in from the set view ([`Self::sign_in_member`]).
    pub(super) fn sign_in_watched(
        &mut self,
        id: &str,
        server: &str,
        definition: Option<u64>,
        shown: Option<u64>,
        revoked: u64,
    ) -> Result<(), String> {
        let changed = || format!("\"{id}\" changed. Check its sign-in and try again.");
        let Some(slot) = self.cluster(id) else {
            return Err(format!("cluster \"{id}\" no longer exists"));
        };
        if slot.info.server.as_deref() != Some(server) {
            return Err(changed());
        }
        let grant_server = slot.info.grant_server();
        if let Some(why) = self.sign_in_held(id, definition, false) {
            return Err(why);
        }
        if self.members.contains_key(id) {
            return Err(format!(
                "\"{id}\" is in the open set. Sign it in from the set view."
            ));
        }
        // Kept watching for the route it has now: another route is turned on again, not signed
        // in.
        let entry = self.settings.watched_cluster(id).map(|entry| &entry.server);
        if entry.is_none() || entry != grant_server.as_ref() {
            return Err(changed());
        }
        // Past the cap, a file edited by hand starts only the first ones.
        let capped = self
            .settings
            .watched_clusters
            .iter()
            .take(crate::keep_watching::MAX_WATCHED_CLUSTERS)
            .any(|entry| entry.context == id);
        if !capped {
            return Err(crate::keep_watching::too_many_watched());
        }
        if self.kept_grants().revocations(id) != revoked {
            return Err(format!("\"{id}\" was stopped. Sign in again."));
        }
        let waiting = matches!(
            self.keep_status(id),
            KeepWatching::NeedsConfirming | KeepWatching::SignInNeeded
        );
        if !waiting || self.keep.has_feed(id) {
            return Err(format!("\"{id}\" is not waiting for a sign-in."));
        }
        let consent = match &*self.sign_in(id) {
            SignIn::Plugin(_) => self.seal_shown_plugin(id, shown.ok_or_else(changed)?)?,
            SignIn::Static => {
                return Err(format!(
                    "\"{id}\" runs no sign-in command. Select it to connect."
                ));
            }
            SignIn::Provider => {
                return Err(format!(
                    "\"{id}\" signs in through an auth provider, which Flint runs only for the \
                     selected cluster. Select it to sign in."
                ));
            }
            SignIn::Unknown => return Err(format!("\"{id}\" has a user Flint can't read.")),
        };
        if !self.start_kept_once(id, consent, Instant::now(), revoked) {
            return Err(format!("\"{id}\" can't sign in now."));
        }
        Ok(())
    }

    /// Starts `id`'s kept connection under `consent`, the seal of a click at `asked`, with access
    /// for one start of its plugin and no input ([`CredentialAccess::signed_in_once`]). Right
    /// before the plugin starts, the click must be under [`SIGN_IN_WINDOW`] old and the files as
    /// they are then must still define what it sealed ([`Self::launch_check`]). A second start is
    /// refused and stops the feed as a failure ([`Self::kept_failed`]), so nothing retries it and
    /// the cluster waits for a sign-in again. Returns whether the feed started: nothing starts
    /// for a cluster whose kept feed may not run now.
    pub(super) fn start_kept_once(
        &mut self,
        id: &str,
        consent: PluginConsent,
        asked: Instant,
        revoked: u64,
    ) -> bool {
        if !self.kept_feed_room(id) {
            return false;
        }
        let consent = Arc::new(consent);
        let launch = self.launch_check(id, Arc::clone(&consent));
        let verify = SendCheck::new(move || {
            let launch = launch.clone();
            Box::pin(async move {
                if asked.elapsed() >= SIGN_IN_WINDOW {
                    return Err("its sign-in was asked for too long ago; sign in again".to_owned());
                }
                launch.refusal().await?;
                // Reading the files may take a while: still in time once they are read.
                if asked.elapsed() >= SIGN_IN_WINDOW {
                    return Err("its sign-in was asked for too long ago; sign in again".to_owned());
                }
                Ok(())
            })
        });
        let attempt = self.next_kept_attempt();
        let spent = self.internal_tx.clone();
        let cluster = id.to_owned();
        let access = CredentialAccess::signed_in_once(
            verify,
            Arc::new(move || {
                let _ = spent.send(Internal::KeepWatchFailed {
                    cluster: cluster.clone(),
                    attempt,
                });
            }),
            self.kept_grants().runs(),
        );
        self.launch_kept(id, attempt, access, Some((consent, revoked)));
        self.publish_keep_watching();
        true
    }

    /// Takes the sign-in asked for `id`, as the access of its attempt starting now in `role`, if
    /// it still holds: asked within [`SIGN_IN_WINDOW`], on the same definition and server,
    /// confirmed, and for the program the click saw. A selected member signs in as the selected
    /// cluster and uses none. Anything else leaves the attempt without it, so the plugin is
    /// refused and the member waits for a sign-in again.
    pub(super) fn take_member_sign_in(
        &mut self,
        id: &str,
        role: Role,
        attempt: u64,
    ) -> Option<CredentialAccess> {
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
        let spent = self.internal_tx.clone();
        let grants = self.kept_grants();
        let cluster = id.to_owned();
        let access = CredentialAccess::signed_in_once(
            check,
            Arc::new(move || {
                let _ = spent.send(Internal::MemberSignInSpent {
                    cluster: cluster.clone(),
                    attempt,
                });
            }),
            grants.runs(),
        );
        // Held with the members' grants: a quit or a new kubeconfig file list kills its program.
        grants.admit_member(id, &access);
        Some(access)
    }

    /// Members whose connection one sign-in from the set view made: up, and also those still
    /// connecting.
    pub(super) fn signed_in_once_members(&self) -> Vec<ClusterId> {
        self.clusters
            .iter()
            .filter(|slot| {
                slot.access.signed_in_once_access()
                    && self.members.contains_key(&slot.info.name)
                    && matches!(
                        slot.state,
                        ConnectionState::Connected(_) | ConnectionState::Connecting
                    )
            })
            .map(|slot| slot.info.name.clone())
            .collect()
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
