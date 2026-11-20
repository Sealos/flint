//! The pure parts of the bottom dock (#13; UP-42): which cluster a tab's session runs on and
//! whether its context still points there, how a tab is titled, and how the tab list names a
//! tab. No GPUI types, so all of it is testable without a window; the dock (`dock.rs`) draws
//! with them.

use flint_core::{ClusterId, display};

/// The longest tab title, in characters, before its middle is cut. The cluster is drawn beside
/// it and is shortened on its own ([`display::short_name`]).
pub const TITLE_CHARS: usize = 36;

/// How a tab's session is doing, shown as a dot on the tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabStatus {
    Starting,
    Live,
    Retrying,
    Ended,
    Failed,
}

/// The cluster a tab's session runs on: the kubeconfig context and the API server it pointed at
/// when the tab opened. Like an AI exposure (ADR-0009) and a trusted cluster (ADR-0015), a tab is
/// pinned to both, so a context re-pointed at another server is another cluster: opening the
/// same pod there opens a new tab instead of showing the old server's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TabCluster {
    pub context: ClusterId,
    /// The server URL without a trailing slash; `None` when the context names no cluster entry.
    pub server: Option<String>,
}

impl TabCluster {
    pub fn new(context: impl Into<ClusterId>, server: Option<&str>) -> Self {
        TabCluster {
            context: context.into(),
            server: server.map(normalize_server),
        }
    }

    /// The context's short name, as the sidebar shows it: what the tab says it runs on.
    pub fn label(&self) -> String {
        display::short_name(&self.context)
    }

    /// Whether the context, now pointing at `server`, is still this tab's cluster.
    pub fn is_at(&self, server: Option<&str>) -> bool {
        self.server.as_deref() == server.map(normalize_server).as_deref()
    }
}

/// `https://host:6443/` and `https://host:6443` are the same server.
fn normalize_server(server: &str) -> String {
    server.trim_end_matches('/').to_owned()
}

/// A tab's title as drawn: cut in the middle when long, so both the pod's name and its container
/// stay readable.
pub fn shown_title(title: &str) -> String {
    display::middle_truncate(title, TITLE_CHARS)
}

/// What a tab's accessible label and full name say: its title and its cluster.
pub fn full_title(title: &str, cluster: &TabCluster) -> String {
    format!("{title} on {}", cluster.context)
}

/// Why a shell ended when its context was re-pointed at another server.
pub fn repointed_note(context: &str) -> String {
    format!(
        "The context {context} now points at a different API server, so this shell was ended: \
         it ran on the old one. Open a shell again to reach the new server."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-42: a tab is pinned to its context and the server it pointed at.
    #[test]
    fn a_tab_cluster_is_its_context_and_server() {
        let tab = TabCluster::new("prod", Some("https://10.0.0.1:6443/"));
        assert_eq!(tab.server.as_deref(), Some("https://10.0.0.1:6443"));
        assert!(tab.is_at(Some("https://10.0.0.1:6443")));
        assert!(tab.is_at(Some("https://10.0.0.1:6443/")));
        assert!(!tab.is_at(Some("https://10.0.0.2:6443")));
        assert!(!tab.is_at(None));
        assert_ne!(tab, TabCluster::new("prod", Some("https://10.0.0.2:6443")));
        assert!(TabCluster::new("x", None).is_at(None));
    }

    // covers UP-42: tabs name their cluster in its short form, and long titles keep both ends.
    #[test]
    fn titles_and_labels_stay_short() {
        let eks = TabCluster::new(
            "arn:aws:eks:eu-west-1:123456789012:cluster/payments-prod",
            None,
        );
        assert_eq!(eks.label(), "cluster/payments-prod");
        let long = "Shell: checkout-api-7d9f8b6c5d-x2x9z · istio-proxy";
        let shown = shown_title(long);
        assert_eq!(shown.chars().count(), TITLE_CHARS);
        assert!(shown.starts_with("Shell: checkout"), "{shown}");
        assert!(shown.ends_with("istio-proxy"), "{shown}");
        assert_eq!(shown_title("web-1 · app"), "web-1 · app");
        assert_eq!(
            full_title("api (Deployment)", &TabCluster::new("kind-dev", None)),
            "api (Deployment) on kind-dev"
        );
    }
}
