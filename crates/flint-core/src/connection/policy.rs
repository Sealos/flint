//! Whether a connection's credential may run now.
//!
//! Some credentials run a program: an exec plugin or an auth provider's command, at connect and
//! again whenever a token is due, on any request. A [`CredentialPolicy`] says which of them may
//! do that for a connection in a given [`Role`], and a [`CredentialGrant`] applies the answer to
//! one connection: before its credential is resolved, before a refresh starts, and while a
//! refresh runs, which a demotion ends. A denial fails closed: nothing is started or sent.

use std::sync::Arc;

use http::Request;
use tokio::sync::watch;
use tower::BoxError;
use tower::filter::Predicate;

use crate::auth_plugin;
use crate::credential::CredentialSource;

/// What a connection is to the app right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The cluster the person is working in.
    Foreground,
    /// A cluster kept connected while another one is in front.
    #[cfg_attr(not(test), expect(dead_code, reason = "no connection is demoted yet"))]
    Background,
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
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "not wired into a connection yet")
)]
pub(crate) struct ForegroundOnly;

impl CredentialPolicy for ForegroundOnly {
    fn may_run(&self, source: &CredentialSource, role: Role) -> bool {
        match source {
            CredentialSource::Definition | CredentialSource::TokenFile(_) => true,
            CredentialSource::Plugin | CredentialSource::AuthProvider => role == Role::Foreground,
        }
    }
}

/// A policy and the current role of one connection. Clones share the role, so whoever holds one
/// can change it while the connection is being made or used.
#[derive(Clone)]
pub(crate) struct CredentialAccess {
    policy: Arc<dyn CredentialPolicy>,
    role: Arc<watch::Sender<Role>>,
}

impl CredentialAccess {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "not wired into a connection yet")
    )]
    pub(crate) fn new(policy: Arc<dyn CredentialPolicy>, role: Role) -> Self {
        let (role, _) = watch::channel(role);
        CredentialAccess {
            policy,
            role: Arc::new(role),
        }
    }

    /// [`AlwaysAllow`], for a connection in the foreground.
    pub(crate) fn always() -> Self {
        let (role, _) = watch::channel(Role::Foreground);
        CredentialAccess {
            policy: Arc::new(AlwaysAllow),
            role: Arc::new(role),
        }
    }

    /// Changes the role. A refresh the new role does not allow is ended.
    #[cfg_attr(not(test), expect(dead_code, reason = "no connection is demoted yet"))]
    pub(crate) fn set_role(&self, role: Role) {
        self.role.send_replace(role);
    }

    /// This access, for a connection whose credential comes from `source`.
    pub(crate) fn grant(&self, source: CredentialSource) -> CredentialGrant {
        CredentialGrant {
            access: self.clone(),
            source,
        }
    }
}

/// [`CredentialAccess`] applied to one connection's credential.
#[derive(Clone)]
pub(crate) struct CredentialGrant {
    access: CredentialAccess,
    source: CredentialSource,
}

impl CredentialGrant {
    fn allows(&self, role: Role) -> bool {
        self.access.policy.may_run(&self.source, role)
    }

    /// Whether the credential may run now.
    pub(crate) fn allowed(&self) -> bool {
        // Copied out, so the policy runs without the channel's lock.
        let role = *self.access.role.borrow();
        self.allows(role)
    }

    /// Resolves once the credential may no longer run, at once if it may not now.
    pub(crate) async fn revoked(&self) {
        let mut changes = self.access.role.subscribe();
        loop {
            let role = *changes.borrow_and_update();
            if !self.allows(role) || changes.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Stops a request before an auth layer that would run the credential's program.
impl<B> Predicate<Request<B>> for CredentialGrant {
    type Request = Request<B>;

    fn check(&mut self, request: Request<B>) -> Result<Request<B>, BoxError> {
        if self.allowed() {
            Ok(request)
        } else {
            Err(auth_plugin::not_allowed().into())
        }
    }
}

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
