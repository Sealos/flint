//! A workload's pods, for Shell (UP-47): which pod a shell opens in.
//!
//! The pods are listed lazily, never on every update of the workload:
//! - once for showing, from the API server's cache ([`Freshness::Cached`]), a moment after the
//!   workload is selected (so moving through a list starts no request per row) or when its row
//!   menu opens and the list is missing, failed or older than the workload;
//! - once more when Shell is asked for, as a quorum read ([`Freshness::Current`]), and the
//!   shell opens in a pod of that list, never in one listed before.
//!
//! One list runs at a time: asking while one runs joins it, and a current list asked for while
//! a cached one runs starts when that one ends. Dropping the view drops the request, which
//! aborts it in the core.

// module-tests: row_actions_tests.rs

use std::sync::Arc;
use std::time::Duration;

use flint_core::{Freshness, PodSet, WorkloadPod};
use gpui_kit::*;

use super::{DetailView, Fetch, Subject};

/// How long after a workload is selected its pods are listed for showing: moving through a
/// list with the keys selects many rows in a row, and only the one stopped on is listed.
pub(in crate::ui) const SHOW_DELAY: Duration = Duration::from_millis(300);

/// A workload's pods, as last listed.
#[derive(Debug, Clone, Default)]
pub(in crate::ui) enum Pods {
    #[default]
    NotListed,
    Listed(Arc<[WorkloadPod]>),
    /// The list failed: why.
    Failed(String),
}

/// The pod list running.
pub(super) struct Listing {
    pub freshness: Freshness,
    pub _task: Task<()>,
}

/// Lists pods in place of the core, in tests: `(cluster, namespace, pods, freshness)`.
#[cfg(test)]
pub(in crate::ui) type PodLister = std::rc::Rc<
    dyn Fn(
        &str,
        &str,
        PodSet,
        Freshness,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<WorkloadPod>, String>>>,
    >,
>;

/// Set in tests to answer pod lists without a cluster.
#[cfg(test)]
pub(in crate::ui) struct TestPodLister(pub PodLister);

#[cfg(test)]
impl Global for TestPodLister {}

impl DetailView {
    /// Lists the workload's pods for showing, once a moment has passed (see [`SHOW_DELAY`]).
    pub(super) fn list_pods_soon(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.subject, Subject::Workload(_)) {
            return;
        }
        let timer = cx.background_executor().timer(SHOW_DELAY);
        self._show_pods = cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |view, cx| view.ensure_pods_listed(cx));
        });
    }

    /// Lists the workload's pods for showing if they are not at hand, failed, or were listed
    /// before the workload last changed (the row menu calls this when it opens).
    pub(in crate::ui) fn ensure_pods_listed(&mut self, cx: &mut Context<Self>) {
        let listed = matches!(self.pods, Pods::Listed(_)) && !self.pods_stale;
        if !listed {
            self.list_pods(Freshness::Cached, cx);
        }
    }

    /// Lists the workload's pods, unless a list that will do runs already.
    pub(super) fn list_pods(&mut self, freshness: Freshness, cx: &mut Context<Self>) {
        let Subject::Workload(kind) = self.subject else {
            return;
        };
        if let Some(running) = &self.listing {
            if running.freshness == Freshness::Cached && freshness == Freshness::Current {
                self.current_wanted = true;
            }
            return;
        }
        let o = &self.object;
        let pods = match &self.fetch {
            Fetch::Loaded { detail, .. } => PodSet::of_workload(kind, &o.name, &detail.object),
            _ => PodSet::Workload {
                kind,
                name: o.name.clone(),
            },
        };
        let namespace = o.namespace.clone().unwrap_or_default();
        #[cfg(test)]
        let listed = match cx.try_global::<TestPodLister>() {
            Some(lister) => (lister.0)(&o.cluster, &namespace, pods, freshness),
            None => Box::pin(
                self.core
                    .workload_pods(&o.cluster, &namespace, pods, freshness),
            ),
        };
        #[cfg(not(test))]
        let listed = self
            .core
            .workload_pods(&o.cluster, &namespace, pods, freshness);
        let task = cx.spawn(async move |this, cx| {
            let listed = listed.await;
            let _ = this.update(cx, |view, cx| view.pods_listed(freshness, listed, cx));
        });
        self.listing = Some(Listing {
            freshness,
            _task: task,
        });
        cx.notify();
    }

    /// Takes a pod list in. A current one settles a Shell waiting for it: it opens in a pod of
    /// this list, or is refused with the list's error.
    fn pods_listed(
        &mut self,
        freshness: Freshness,
        listed: Result<Vec<WorkloadPod>, String>,
        cx: &mut Context<Self>,
    ) {
        self.listing = None;
        self.pods_stale = false;
        self.pods = match listed {
            Ok(pods) => Pods::Listed(pods.into()),
            Err(error) => Pods::Failed(error),
        };
        if freshness == Freshness::Current {
            self.run_pending_listed(cx);
        }
        if std::mem::take(&mut self.current_wanted) {
            self.list_pods(Freshness::Current, cx);
        }
        cx.notify();
    }
}
