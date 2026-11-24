//! Ties a write's dialogs to the connection they were opened on. Once that connection changes, a
//! dialog about it can no longer be answered, even if the context later returns to the same
//! server: what the person read was about a connection that has since been replaced.

use std::rc::Rc;

use flint_core::ClusterSummary;
use gpui_kit::*;

use super::clusters::ClustersModel;

/// What a notice says when an open confirmation closed because its connection changed.
pub(in crate::ui) fn connection_changed_text() -> String {
    "The cluster connection changed, so this action was cancelled and nothing was sent. Open it \
     again to review it on the current connection."
        .to_owned()
}

/// Which connection of a context a dialog was opened on. The core's counters only go up, so a
/// context moved and put back never compares equal to what it was before the move.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectionIdentity {
    generation: u64,
    definition: u64,
    endpoint: u64,
    /// Moves on a disconnect: a dialog opened before it is never answered after it.
    write_epoch: u64,
    server: Option<String>,
}

impl ConnectionIdentity {
    /// The connection of `cluster` now, if a write could be asked for on it.
    fn of(clusters: &[ClusterSummary], cluster: &str) -> Option<Self> {
        let summary = clusters.iter().find(|c| c.info.name == cluster)?;
        (!summary.provisional && !summary.paused).then(|| Self {
            generation: summary.generation,
            definition: summary.definition,
            endpoint: summary.endpoint,
            write_epoch: summary.write_epoch,
            server: summary.info.server.clone(),
        })
    }
}

/// Answers whether the connection a dialog was opened on is still the current one.
#[derive(Clone)]
pub(in crate::ui) struct ConnectionGuard(Rc<dyn Fn(&App) -> bool>);

impl ConnectionGuard {
    /// A guard on the connection of `cluster` now; `None` if it has no settled connection.
    pub(in crate::ui) fn of(
        model: &Entity<ClustersModel>,
        cluster: &str,
        cx: &App,
    ) -> Option<Self> {
        let opened = ConnectionIdentity::of(&model.read(cx).clusters, cluster)?;
        let (model, cluster) = (model.downgrade(), cluster.to_owned());
        Some(Self(Rc::new(move |cx| {
            model.upgrade().is_some_and(|model| {
                ConnectionIdentity::of(&model.read(cx).clusters, &cluster).as_ref() == Some(&opened)
            })
        })))
    }

    /// A guard for a dialog test with no connection behind it.
    #[cfg(test)]
    pub(in crate::ui) fn always() -> Self {
        Self(Rc::new(|_| true))
    }

    /// A guard that answers from `check`, for a dialog test.
    #[cfg(test)]
    pub(in crate::ui) fn from_check(check: impl Fn() -> bool + 'static) -> Self {
        Self(Rc::new(move |_| check()))
    }

    pub(in crate::ui) fn holds(&self, cx: &App) -> bool {
        (self.0)(cx)
    }
}

#[cfg(test)]
mod tests {
    use flint_core::kubeconfig::ContextInfo;
    use flint_core::{ClusterSummary, ConnectionState};

    use super::ConnectionIdentity;

    fn summary(generation: u64, definition: u64, server: &str) -> ClusterSummary {
        ClusterSummary {
            info: ContextInfo {
                name: "dev".into(),
                cluster: None,
                user: None,
                namespace: None,
                server: Some(server.to_owned()),
                proxy_url: None,
                source: "/k".into(),
            },
            state: ConnectionState::Disconnected,
            generation,
            definition,
            endpoint: 1,
            write_epoch: 0,
            confirmed: true,
            provisional: false,
            paused: false,
        }
    }

    fn identity(clusters: &[ClusterSummary]) -> Option<ConnectionIdentity> {
        ConnectionIdentity::of(clusters, "dev")
    }

    // Moved, moved back, or its definition edited: never the connection a dialog was opened on.
    #[test]
    fn any_change_of_connection_is_another_identity() {
        let opened = identity(&[summary(1, 1, "https://a.invalid")]);
        assert!(opened.is_some());
        assert_eq!(opened, identity(&[summary(1, 1, "https://a.invalid")]));
        for changed in [
            summary(2, 1, "https://a.invalid"),
            summary(1, 2, "https://a.invalid"),
            summary(1, 1, "https://b.invalid"),
        ] {
            assert_ne!(opened, identity(&[changed]));
        }
    }

    // Disconnected since, even if connected again on the same definition.
    #[test]
    fn a_disconnect_is_another_identity() {
        let opened = identity(&[summary(1, 1, "https://a.invalid")]);
        let mut reconnected = summary(1, 1, "https://a.invalid");
        reconnected.write_epoch += 1;
        assert_ne!(opened, identity(&[reconnected]));
    }

    // A context that left, is paused, or may be redefined by an unreadable file has none.
    #[test]
    fn an_unsettled_context_has_no_identity() {
        assert_eq!(identity(&[]), None);
        let mut paused = summary(1, 1, "https://a.invalid");
        paused.paused = true;
        assert_eq!(identity(&[paused]), None);
        let mut provisional = summary(1, 1, "https://a.invalid");
        provisional.provisional = true;
        assert_eq!(identity(&[provisional]), None);
    }
}
