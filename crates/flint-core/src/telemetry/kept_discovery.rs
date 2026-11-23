//! Discovery for a feed on a kept-watching connection, which has no slot to hold it.
//!
//! It runs inside the feed, on the feed's client, so it ends with the feed. It reuses the token
//! that connection holds and never runs an exec plugin: while the token is due it finds nothing,
//! and every kind stays unchecked. Only the state of the groups problems are read from is kept.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Semaphore, watch};

use super::RequestGate;
use crate::cluster;
use crate::discovery::ClusterDiscovery;
use crate::problems::ProblemKind;
use crate::read;

const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// What a kept feed needs to run discovery.
pub(crate) struct KeptDiscovery {
    pub(crate) client: kube::Client,
    pub(crate) requests: RequestGate,
    /// Shared by every kept feed, so a launch with many watched clusters is no burst.
    pub(crate) turns: Arc<Semaphore>,
}

impl KeptDiscovery {
    /// Publishes on `found` until every group problems are read from is confirmed. What could not
    /// be read publishes `None`, which vouches for nothing.
    pub(super) async fn follow(
        self,
        found: watch::Sender<Option<Arc<ClusterDiscovery>>>,
        version: watch::Sender<u64>,
    ) {
        let client = cluster::reusing_held_token(self.client);
        let mut wait = RETRY_MIN;
        loop {
            let result = {
                let Ok(_turn) = self.turns.acquire().await else {
                    return;
                };
                self.requests.run(read::discover_kinds(&client)).await
            };
            let groups = result
                .ok()
                .map(|kinds| problem_groups(kinds.incomplete_groups()));
            let done = groups.as_ref().is_some_and(Vec::is_empty);
            found.send_replace(groups.map(|groups| {
                Arc::new(ClusterDiscovery::with_incomplete_groups(Vec::new(), groups))
            }));
            version.send_modify(|count| *count += 1);
            if done {
                return;
            }
            tokio::time::sleep(wait).await;
            wait = wait.saturating_mul(2).min(RETRY_MAX);
        }
    }
}

/// The groups of `incomplete` that problems are read from, so what is kept has a fixed bound
/// however many groups a server reports.
fn problem_groups(incomplete: Vec<String>) -> Vec<String> {
    let mut groups: Vec<String> = incomplete
        .into_iter()
        .map(|group| group.to_ascii_lowercase())
        .filter(|group| {
            group == "core"
                || ProblemKind::ALL
                    .iter()
                    .any(|kind| kind.api_groups().contains(&group.as_str()))
        })
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

#[cfg(test)]
mod tests {
    use super::problem_groups;

    #[test]
    fn only_the_groups_problems_are_read_from_are_kept() {
        let mut reported: Vec<String> = (0..10_000).map(|at| format!("g{at}.example")).collect();
        reported.extend(["APPS", "apps", "batch", "core", ""].map(str::to_owned));
        assert_eq!(problem_groups(reported), ["", "apps", "batch", "core"]);
        assert!(problem_groups(vec!["metrics.k8s.io".to_owned()]).is_empty());
    }
}
