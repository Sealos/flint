//! Feeds the open list's "Problems" toggle and the navigation's counts from the telemetry the
//! core already holds (#273). Reads only: it opens no overview, so it starts no feed.

// module-tests: list_problems_tests.rs

use std::collections::HashMap;
use std::sync::Arc;

use flint_core::ClusterId;
use flint_core::telemetry::TelemetrySnapshot;
use gpui_kit::*;

use super::super::list::problems::{ClusterProblems, nav_badges};
use super::Workspace;

type Feeds = Vec<(ClusterId, Option<Arc<TelemetrySnapshot>>)>;

/// Which connection a cluster on screen is: a context moved to another server is another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) struct FeedIdentity {
    pub(in crate::ui) cluster: ClusterId,
    pub(in crate::ui) generation: u64,
    pub(in crate::ui) endpoint: u64,
}

/// The last values read, and the read under way.
#[derive(Default)]
pub(in crate::ui) struct ProblemFeed {
    identities: Vec<FeedIdentity>,
    seen: Feeds,
    values: HashMap<ClusterId, ClusterProblems>,
    building: bool,
    _build: Option<Task<()>>,
}

impl ProblemFeed {
    /// Notes the connections on screen. Values read from any other are dropped.
    pub(in crate::ui) fn follow(&mut self, identities: Vec<FeedIdentity>) {
        if self.identities != identities {
            self.identities = identities;
            self.values.clear();
            self.seen.clear();
        }
    }

    /// Takes a finished read, unless the connections changed while it ran.
    pub(in crate::ui) fn finish(
        &mut self,
        read_for: &[FeedIdentity],
        values: HashMap<ClusterId, ClusterProblems>,
    ) {
        self.building = false;
        if self.identities == read_for {
            self.values = values;
        } else {
            self.seen.clear();
        }
    }

    #[cfg(test)]
    pub(in crate::ui) fn value(&self, cluster: &str) -> Option<&ClusterProblems> {
        self.values.get(cluster)
    }
}

fn same_feeds(a: &Feeds, b: &Feeds) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|((a_id, a), (b_id, b))| {
            a_id == b_id
                && match (a, b) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                }
        })
}

impl Workspace {
    /// The connection of each cluster on screen.
    fn problem_identities(&self, cx: &App) -> Vec<FeedIdentity> {
        let model = self.model.read(cx);
        let identity = |id: &str| {
            let summary = model.clusters.iter().find(|s| s.info.name == id);
            FeedIdentity {
                cluster: id.to_owned(),
                generation: summary.map_or(0, |s| s.generation),
                endpoint: summary.map_or(0, |s| s.endpoint),
            }
        };
        match &self.sets.running {
            Some(run) => run
                .current_members()
                .map(|member| identity(member.context()))
                .collect(),
            None => self.cluster.iter().map(|id| identity(id)).collect(),
        }
    }

    /// The feed of each cluster on screen. A member that left its admitted server, or a slot
    /// that is not the one the views show, has none, so it reads as not checked.
    fn problem_feeds(&self, cx: &App) -> Feeds {
        let model = self.model.read(cx);
        let core = model.core();
        let snapshot = |id: &str| core.telemetry(id).map(|feed| feed.snapshot);
        if let Some(run) = &self.sets.running {
            return run
                .current_members()
                .map(|member| {
                    let id = member.context().to_owned();
                    let holds = model
                        .clusters
                        .iter()
                        .find(|summary| summary.info.name == id)
                        .is_some_and(|s| s.confirmed && member.same_endpoint(s));
                    let feed = holds.then(|| snapshot(&id)).flatten();
                    (id, feed)
                })
                .collect();
        }
        let (Some(cluster), Some(generation)) = (&self.cluster, self.generation) else {
            return Vec::new();
        };
        let feed = model
            .active_cluster()
            .filter(|active| {
                active.info.name == *cluster && active.generation == generation && active.confirmed
            })
            .and_then(|_| snapshot(cluster));
        vec![(cluster.clone(), feed)]
    }

    /// Reads what changed, off the UI thread, and shows the newest values. Called on the
    /// workspace's one-second tick; with nothing new it compares pointers and stops.
    pub(super) fn refresh_problems(&mut self, cx: &mut Context<Self>) {
        let identities = self.problem_identities(cx);
        self.problem_feed.follow(identities.clone());
        self.show_problems(cx);
        if self.problem_feed.building {
            return;
        }
        let feeds = self.problem_feeds(cx);
        if same_feeds(&feeds, &self.problem_feed.seen) {
            return;
        }
        self.problem_feed.seen = feeds.clone();
        self.problem_feed.building = true;
        let job = cx.background_spawn(async move {
            feeds
                .into_iter()
                .map(|(id, feed)| (id, ClusterProblems::read(feed.as_deref())))
                .collect::<HashMap<_, _>>()
        });
        self.problem_feed._build = Some(cx.spawn(async move |this, cx| {
            let values = job.await;
            let _ = this.update(cx, |ws, cx| {
                ws.problem_feed.finish(&identities, values);
                ws.show_problems(cx);
            });
        }));
    }

    /// Gives the open list and the navigation the values of the clusters on screen. A cluster
    /// not read yet is not checked, so values of the last cluster never show for this one. The
    /// counts cover what the list shows: its namespaces and, in a set, its chosen clusters.
    fn show_problems(&mut self, cx: &mut Context<Self>) {
        let clusters: Vec<ClusterId> = match &self.sets.running {
            Some(run) => run
                .current_members()
                .map(|member| member.context().to_owned())
                .collect(),
            None => self.cluster.iter().cloned().collect(),
        };
        let values: HashMap<ClusterId, ClusterProblems> = clusters
            .iter()
            .map(|id| {
                let known = self.problem_feed.values.get(id).cloned();
                (id.clone(), known.unwrap_or_default())
            })
            .collect();
        let (namespaces, chosen) = match (&self.sets.running, &self.cluster) {
            (Some(run), _) => run.list.read(cx).problem_scope(),
            (None, Some(cluster)) => (self.ui_state.namespace_filter(cluster).to_vec(), Vec::new()),
            (None, None) => (Vec::new(), Vec::new()),
        };
        let ordered: Vec<ClusterProblems> = clusters
            .iter()
            .filter(|id| chosen.is_empty() || chosen.contains(id))
            .filter_map(|id| values.get(id).cloned())
            .collect();
        self.nav.update(cx, |nav, cx| {
            nav.set_problem_badges(nav_badges(&ordered, &namespaces), cx)
        });
        if let Some(run) = &self.sets.running {
            run.list
                .update(cx, |list, cx| list.set_problems(values, cx));
        } else if let Some(list) = &self.list {
            list.update(cx, |list, cx| list.set_problems(values, cx));
        }
    }
}
