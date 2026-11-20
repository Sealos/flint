//! Clusters are protected (read-only in Flint) by default (ADR-0015).
//!
//! A cluster is [`Protection::Protected`] unless its API server is on this machine
//! ([`is_local_server`]), the user trusts it (`trusted_clusters` in `settings.toml`), or the user
//! unlocked it for this session ([`SessionUnlocks`]). Trust and unlocks are pinned to the context
//! and the server it pointed at when the user chose, so a re-pointed context is protected again.
//!
//! The core is the gate: every action that changes a cluster, or runs code in it (exec today;
//! edit, delete, scale, port-forward and the rest as they arrive), asks the cluster manager for
//! a client through the one check in `ClusterManager::write_client`, which refuses with
//! [`WriteRefusal::Protected`]. A front end shows the same state with [`protection`], but cannot
//! open a way around it.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::settings::{ClusterPin, Settings};

/// Whether Flint may change a cluster, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protection {
    /// Read-only in Flint until the user unlocks or trusts it.
    Protected,
    /// Its API server is on this machine (kind, minikube, Docker Desktop, OrbStack).
    Local,
    /// Unlocked by the user until Flint quits, or until Lock Again.
    Unlocked,
    /// Trusted in `settings.toml`: unlocked across restarts.
    Trusted,
}

impl Protection {
    /// Whether actions that change the cluster (or run code in it) may run.
    pub fn allows_changes(self) -> bool {
        !matches!(self, Protection::Protected)
    }

    /// Its name in the automation state (`protected`, `local`, `unlocked`, `trusted`).
    pub fn name(self) -> &'static str {
        match self {
            Protection::Protected => "protected",
            Protection::Local => "local",
            Protection::Unlocked => "unlocked",
            Protection::Trusted => "trusted",
        }
    }
}

/// The protection of `context`, now pointing at `server`. A context with no server is
/// protected: nothing can be pinned to it, and nothing can connect either.
pub fn protection(
    settings: &Settings,
    unlocks: &SessionUnlocks,
    context: &str,
    server: Option<&str>,
) -> Protection {
    let Some(server) = server else {
        return Protection::Protected;
    };
    if settings.trust(context, Some(server)).is_trusted() {
        Protection::Trusted
    } else if !settings.protect_local_clusters && is_local_server(server) {
        Protection::Local
    } else if unlocks.is_unlocked(context, server) {
        Protection::Unlocked
    } else {
        Protection::Protected
    }
}

/// Docker Desktop's kubeconfig names its cluster by this host, which its installer maps to
/// `127.0.0.1` in `/etc/hosts`.
const DOCKER_DESKTOP_HOST: &str = "kubernetes.docker.internal";

/// Whether an API server URL is on this machine: `localhost` (or a name under `.localhost`), a
/// loopback address (`127.0.0.0/8`, `::1`), or Docker Desktop's `kubernetes.docker.internal`.
/// Names are not resolved: what counts is what the kubeconfig says. A URL that does not parse
/// is not local.
pub fn is_local_server(server: &str) -> bool {
    let Ok(uri) = server.trim().parse::<http::Uri>() else {
        return false;
    };
    let Some(host) = uri.host() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<IpAddr>() {
        return ip.to_canonical().is_loopback();
    }
    host == "localhost" || host.ends_with(".localhost") || host == DOCKER_DESKTOP_HOST
}

/// The clusters the user unlocked for this session, each pinned to the server the user saw
/// when unlocking. Shared between the front end and the core: an unlock takes effect at once,
/// before the next action is asked for. Cheap to clone; nothing is written to disk, so every
/// unlock ends when Flint quits.
#[derive(Debug, Clone, Default)]
pub struct SessionUnlocks(Arc<Mutex<BTreeSet<ClusterPin>>>);

impl SessionUnlocks {
    /// Unlocks `context` at `server` until Flint quits or [`SessionUnlocks::lock`]. An older
    /// unlock of the same context at another server is dropped.
    pub fn unlock(&self, context: &str, server: &str) {
        let mut set = self.set();
        set.retain(|p| p.context != context);
        set.insert(ClusterPin::new(context, server));
    }

    /// Locks `context` again (Lock Again). Returns whether it was unlocked.
    pub fn lock(&self, context: &str) -> bool {
        let mut set = self.set();
        let before = set.len();
        set.retain(|p| p.context != context);
        set.len() != before
    }

    /// Whether `context` is unlocked at `server`.
    pub fn is_unlocked(&self, context: &str, server: &str) -> bool {
        self.set().iter().any(|p| p.matches(context, server))
    }

    fn set(&self) -> std::sync::MutexGuard<'_, BTreeSet<ClusterPin>> {
        // A panic while holding the lock leaves a valid set: take it anyway.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Why the core would not hand out a client for an action that changes a cluster.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WriteRefusal {
    #[error(
        "{cluster} is protected: Flint does not change it, or open shells in it, until you \
         unlock or trust it"
    )]
    Protected { cluster: String },
    #[error("\"{cluster}\" is not connected")]
    NotConnected { cluster: String },
    #[error("the core is not running")]
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-54: clusters on this machine start unlocked; anything else is protected.
    #[test]
    fn local_servers_are_the_loopback_and_localhost() {
        for local in [
            "https://127.0.0.1:6443",
            "https://127.0.0.1:6443/",
            "https://127.1.2.3",
            "https://localhost:8443",
            "https://LOCALHOST:8443",
            "https://localhost.:8443",
            "https://kind.localhost:6443",
            "https://[::1]:6443",
            "https://[::ffff:127.0.0.1]:6443",
            "https://kubernetes.docker.internal:6443",
            "http://127.0.0.1:8001",
        ] {
            assert!(is_local_server(local), "{local}");
        }
        for remote in [
            "https://10.0.0.1:6443",
            "https://192.168.49.2:8443",
            "https://0.0.0.0:6443",
            "https://ABCD.gr7.eu-west-1.eks.amazonaws.com",
            "https://localhost.example.com",
            "https://mylocalhost:6443",
            "https://127.0.0.1.nip.io",
            "https://[2001:db8::1]:6443",
            "not a url",
            "",
        ] {
            assert!(!is_local_server(remote), "{remote}");
        }
    }

    // covers UP-54, UP-55
    #[test]
    fn protection_follows_trust_locality_and_unlocks() {
        let unlocks = SessionUnlocks::default();
        let mut settings = Settings::default();
        let prod = "https://prod.example:6443";
        let kind = "https://127.0.0.1:6443";
        let of = |s: &Settings, context: &str, server: Option<&str>| {
            protection(s, &unlocks, context, server)
        };
        assert_eq!(of(&settings, "prod", Some(prod)), Protection::Protected);
        assert_eq!(of(&settings, "prod", None), Protection::Protected);
        assert_eq!(of(&settings, "kind", Some(kind)), Protection::Local);
        assert!(!Protection::Protected.allows_changes());
        assert!(Protection::Local.allows_changes());

        unlocks.unlock("prod", prod);
        assert_eq!(of(&settings, "prod", Some(prod)), Protection::Unlocked);
        // Pinned: the same name at another server is protected.
        assert_eq!(
            of(&settings, "prod", Some("https://other.example")),
            Protection::Protected
        );
        assert!(unlocks.lock("prod"));
        assert!(!unlocks.lock("prod"));
        assert_eq!(of(&settings, "prod", Some(prod)), Protection::Protected);

        settings
            .trusted_clusters
            .push(ClusterPin::new("prod", prod));
        assert_eq!(of(&settings, "prod", Some(prod)), Protection::Trusted);
        assert_eq!(
            of(&settings, "prod", Some("https://elsewhere")),
            Protection::Protected,
            "a re-pointed trusted context is protected again"
        );

        settings.protect_local_clusters = true;
        assert_eq!(of(&settings, "kind", Some(kind)), Protection::Protected);
        unlocks.unlock("kind", kind);
        assert_eq!(of(&settings, "kind", Some(kind)), Protection::Unlocked);
        // Unlocking at a new server replaces the old unlock.
        unlocks.unlock("kind", "https://127.0.0.1:7443");
        assert_eq!(of(&settings, "kind", Some(kind)), Protection::Protected);
    }

    #[test]
    fn unlocks_are_shared_between_clones() {
        let unlocks = SessionUnlocks::default();
        let seen = unlocks.clone();
        unlocks.unlock("a", "https://a/");
        assert!(seen.is_unlocked("a", "https://a"));
        assert_eq!(
            [
                Protection::Protected,
                Protection::Local,
                Protection::Unlocked,
                Protection::Trusted
            ]
            .map(Protection::name),
            ["protected", "local", "unlocked", "trusted"]
        );
    }

    #[test]
    fn refusals_name_the_cluster() {
        let refusal = WriteRefusal::Protected {
            cluster: "prod".into(),
        };
        assert!(refusal.to_string().starts_with("prod is protected"));
    }
}
