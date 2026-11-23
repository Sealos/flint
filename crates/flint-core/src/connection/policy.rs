//! Whether a connection's credential may run now.
//!
//! Some credentials run a program: an exec plugin or an auth provider's command, at connect and
//! again whenever a token is due, on any request. A [`CredentialPolicy`] says which of them may
//! do that for a connection in a given [`Role`], and a [`CredentialGrant`] applies the answer to
//! one connection: before its credential is resolved, before a refresh starts, and while a
//! refresh runs, which a demotion ends. A denial fails closed: nothing is started or sent.
//!
//! A [`CredentialLease`] is one use of a grant. It ends at the first role the grant does not
//! allow, and a later return to an allowed role does not bring it back, however quickly it came.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::watch;

use crate::cluster::SendCheck;
use crate::credential::CredentialSource;

/// What a connection is to the app right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The cluster the person is working in.
    Foreground,
    /// A cluster kept connected while another one is in front.
    Background,
    /// Its definition can't be confirmed for now. Reads already running go on, and the connection
    /// may return to another role when the pause ends.
    Paused,
    /// The connection was replaced or dropped. Final: no later role applies to it.
    Retired,
}

const ROLES: [Role; 4] = [
    Role::Foreground,
    Role::Background,
    Role::Paused,
    Role::Retired,
];

/// The current role, and how many times each role has been entered. The counts survive a change
/// a watcher did not get to see, which the role alone does not.
#[derive(Debug, Clone, Copy)]
struct RoleState {
    role: Role,
    entered: [u64; ROLES.len()],
}

impl RoleState {
    fn new(role: Role) -> Self {
        RoleState {
            role,
            entered: [0; ROLES.len()],
        }
    }
}

/// Which credentials may run, for a connection in a role. Asked often and from async code, so it
/// must answer at once.
pub(crate) trait CredentialPolicy: Send + Sync {
    fn may_run(&self, source: &CredentialSource, role: Role) -> bool;
}

/// Every credential may run in every role.
pub(crate) struct AlwaysAllow;

impl CredentialPolicy for AlwaysAllow {
    fn may_run(&self, _: &CredentialSource, _: Role) -> bool {
        true
    }
}

/// Credentials that run a program do so only for the foreground connection. Static credentials
/// and token files run nothing, so they are allowed in either role.
pub(crate) struct ForegroundOnly;

impl CredentialPolicy for ForegroundOnly {
    fn may_run(&self, source: &CredentialSource, role: Role) -> bool {
        !source.runs_a_command() || role == Role::Foreground
    }
}

/// A cluster the person chose to keep watching: its credential runs in the background role only,
/// so ending the access ends a running plugin. An auth provider's command can't be stopped once
/// it runs, so it never runs here.
pub(crate) struct KeptInBackground;

impl CredentialPolicy for KeptInBackground {
    fn may_run(&self, source: &CredentialSource, role: Role) -> bool {
        role == Role::Background && *source != CredentialSource::AuthProvider
    }
}

/// Told once when a kept connection's credential fails or is refused.
pub(crate) type OnCredentialFailure = Arc<dyn Fn() + Send + Sync>;

/// How many plugins of kept connections are running, so a quit can wait for them to be killed.
#[derive(Clone)]
pub(crate) struct PluginRuns(Arc<watch::Sender<usize>>);

impl Default for PluginRuns {
    fn default() -> Self {
        PluginRuns(Arc::new(watch::channel(0).0))
    }
}

impl PluginRuns {
    fn track(&self) -> PluginRun {
        self.0.send_modify(|count| *count += 1);
        PluginRun(Arc::clone(&self.0))
    }

    /// Resolves once none is running.
    pub(crate) async fn ended(&self) {
        let mut count = self.0.subscribe();
        let _ = count.wait_for(|running| *running == 0).await;
    }
}

/// One plugin counted in [`PluginRuns`], until dropped.
pub(crate) struct PluginRun(Arc<watch::Sender<usize>>);

impl Drop for PluginRun {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

/// A policy and the current role of one connection. Clones share the role, so whoever holds one
/// can change it while the connection is being made or used.
#[derive(Clone)]
pub(crate) struct CredentialAccess {
    policy: Arc<dyn CredentialPolicy>,
    role: Arc<watch::Sender<RoleState>>,
    /// Asked right before a credential's program starts, for a caller whose answer takes a wait.
    launch_check: Option<SendCheck>,
    /// Set for a kept connection: its first failure ends this access, so nothing retries.
    on_failure: Option<OnCredentialFailure>,
    /// Counts this connection's running plugins, for a kept connection.
    plugin_runs: Option<PluginRuns>,
}

impl CredentialAccess {
    pub(crate) fn new(policy: Arc<dyn CredentialPolicy>, role: Role) -> Self {
        let (role, _) = watch::channel(RoleState::new(role));
        CredentialAccess {
            policy,
            role: Arc::new(role),
            launch_check: None,
            on_failure: None,
            plugin_runs: None,
        }
    }

    /// Access for a connection kept in the background with the person's consent. The first
    /// failure of its credential ends it and calls `on_failure`. `check` is asked right before
    /// each start of its program, and `runs` counts those running.
    pub(crate) fn consented(
        on_failure: OnCredentialFailure,
        check: Option<SendCheck>,
        runs: PluginRuns,
    ) -> Self {
        CredentialAccess {
            on_failure: Some(on_failure),
            launch_check: check,
            plugin_runs: Some(runs),
            ..Self::new(Arc::new(KeptInBackground), Role::Background)
        }
    }

    /// Ends a kept connection's access after a failure, telling its owner the first time. Does
    /// nothing for any other connection.
    fn credential_failed(&self) {
        let Some(on_failure) = &self.on_failure else {
            return;
        };
        let first = self.role.send_if_modified(|state| {
            let live = state.role != Role::Retired;
            if live {
                state.role = Role::Retired;
                state.entered[Role::Retired as usize] += 1;
            }
            live
        });
        if first {
            on_failure();
        }
    }

    /// [`AlwaysAllow`], for a connection in the foreground.
    pub(crate) fn always() -> Self {
        Self::new(Arc::new(AlwaysAllow), Role::Foreground)
    }

    /// [`Self::always`], with `check` asked right before a credential's program starts.
    pub(crate) fn always_checked(check: SendCheck) -> Self {
        CredentialAccess {
            launch_check: Some(check),
            ..Self::always()
        }
    }

    /// Changes the role, unless this access is retired. A refresh the new role does not allow is
    /// ended.
    pub(crate) fn set_role(&self, role: Role) {
        self.role.send_if_modified(|state| {
            let changed = state.role != role && state.role != Role::Retired;
            if changed {
                state.role = role;
                state.entered[role as usize] += 1;
            }
            changed
        });
    }

    /// Ends this access for good, for every clone of every client made under it.
    pub(crate) fn revoke(&self) {
        self.set_role(Role::Retired);
    }

    /// This access, for a connection whose credential comes from `source`.
    pub(crate) fn grant(&self, source: CredentialSource) -> CredentialGrant {
        CredentialGrant {
            access: self.clone(),
            source,
            refused: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// [`CredentialAccess`] applied to one connection's credential.
#[derive(Clone)]
pub(crate) struct CredentialGrant {
    access: CredentialAccess,
    source: CredentialSource,
    /// A server answered 401 while this connection was in the background.
    refused: Arc<AtomicBool>,
}

impl CredentialGrant {
    fn allows(&self, role: Role) -> bool {
        self.access.policy.may_run(&self.source, role)
    }

    /// Whether this connection's credential can start a program.
    pub(crate) fn runs_a_command(&self) -> bool {
        self.source.runs_a_command()
    }

    /// Records a 401. The foreground has its own sign-in path, so only one outside it counts.
    pub(crate) fn note_refused(&self) {
        if self.access.role.borrow().role != Role::Foreground {
            self.refused.store(true, Ordering::Relaxed);
            self.access.credential_failed();
        }
    }

    /// A run of the credential's program failed.
    pub(crate) fn note_failed(&self) {
        self.access.credential_failed();
    }

    /// Whether nobody is looking at this connection: a kept one. Its program gets no stdin, and
    /// a credential it can't use counts as a failure.
    pub(crate) fn unattended(&self) -> bool {
        self.access.on_failure.is_some()
    }

    /// Whether a 401 was recorded since this was last asked.
    pub(crate) fn take_refused(&self) -> bool {
        self.refused.swap(false, Ordering::Relaxed)
    }

    /// Whether the credential may run now.
    pub(crate) fn allowed(&self) -> bool {
        // Copied out, so the policy runs without the channel's lock.
        let role = self.access.role.borrow().role;
        self.allows(role)
    }

    /// Whether some role would stop this credential.
    fn can_be_denied(&self) -> bool {
        !ROLES.iter().all(|role| self.allows(*role))
    }

    /// One use of the credential, from now until the grant is first denied.
    pub(crate) fn lease(&self) -> CredentialLease {
        let state = *self.access.role.borrow();
        CredentialLease {
            began_allowed: self.allows(state.role),
            entered: state.entered,
            grant: self.clone(),
        }
    }

    /// Resolves once the credential may no longer run, at once if it may not now.
    #[cfg(test)]
    pub(crate) async fn revoked(&self) {
        self.lease().revoked().await;
    }
}

/// One use of a [`CredentialGrant`]: a login, a refresh, a write's check, or the life of a
/// connection whose command cannot be stopped. Once ended it stays ended.
#[derive(Clone)]
pub(crate) struct CredentialLease {
    grant: CredentialGrant,
    began_allowed: bool,
    /// [`RoleState::entered`] when it began.
    entered: [u64; ROLES.len()],
}

impl CredentialLease {
    fn holds_in(&self, state: &RoleState) -> bool {
        self.began_allowed
            && ROLES.iter().all(|role| {
                self.grant.allows(*role)
                    || state.entered[*role as usize] == self.entered[*role as usize]
            })
    }

    /// Whether no role the grant denies has been entered since this began.
    pub(crate) fn holds(&self) -> bool {
        let state = *self.grant.access.role.borrow();
        self.holds_in(&state)
    }

    /// Runs `start` if this still holds. No lock is kept while it runs, since `start` can stall
    /// (a spawn from a network mount) and a role change must never wait on it. A change that lands
    /// after this check finds `start` already begun, and [`Self::revoked`] then ends what it began.
    pub(crate) fn start_if_held<T>(&self, start: impl FnOnce() -> T) -> Option<T> {
        self.holds().then(start)
    }

    /// Why the credential's program must not start now, if the access has a check that says so.
    pub(crate) async fn launch_refusal(&self) -> Option<String> {
        match &self.grant.access.launch_check {
            Some(check) => check.refusal().await.err(),
            None => None,
        }
    }

    /// [`CredentialGrant::unattended`].
    pub(crate) fn unattended(&self) -> bool {
        self.grant.unattended()
    }

    /// Counts a plugin as running until the returned value is dropped, for a kept connection.
    pub(crate) fn track_run(&self) -> Option<PluginRun> {
        self.grant
            .access
            .plugin_runs
            .as_ref()
            .map(PluginRuns::track)
    }

    /// [`CredentialGrant::note_failed`].
    pub(crate) fn note_failed(&self) {
        self.grant.note_failed();
    }

    /// Whether the credential behind this can be stopped by a role at all.
    pub(crate) fn can_be_denied(&self) -> bool {
        self.grant.can_be_denied()
    }

    /// Resolves once this has ended, at once if it already has.
    pub(crate) async fn revoked(&self) {
        let mut changes = self.grant.access.role.subscribe();
        loop {
            let state = *changes.borrow_and_update();
            if !self.holds_in(&state) || changes.changed().await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
