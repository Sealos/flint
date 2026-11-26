//! A proxy setting saved from Settings, put in use without a restart.
//!
//! The setting is written to `settings.toml`, put in use for every connection made from then on,
//! and the kubeconfig files are read again under it. A slot loaded under the old setting is held
//! until a load under the new one replaces it ([`ClusterManager::held`]), and that load replaces
//! every slot: each client, feed and background watch is built again. Nothing rebinds: a context
//! the new setting reaches another way is on another route, so sessions and grants pinned to the
//! old one end as for a re-pointed context, and port-forwards, bound to the connection they
//! started on, all end. What came back is reported once each reconnection settled.

use std::collections::{BTreeSet, HashMap};

use super::ClusterManager;
use crate::ClusterId;
use crate::handle::{ConnectionState, PROXY_REPORT_AFTER, ProxyApplied, ProxyReconnectReport};
use crate::keep_watching::KeepWatching;
use crate::settings::{self, ProxySetting};

/// The last proxy change, and what it still waits for.
#[derive(Default)]
pub(super) struct ProxyChange {
    /// Counts changes, so a report timer of an earlier one finds nothing to do.
    seq: u64,
    pending: Option<Pending>,
}

/// A change whose reconnections have not all settled.
struct Pending {
    seq: u64,
    /// The proxy generation this change put in use: the report waits until every slot is
    /// loaded under it, so the routes it reports are the new ones.
    generation: u64,
    /// Each context's route before the change.
    routes: HashMap<ClusterId, Option<String>>,
    waiting: BTreeSet<ClusterId>,
    report: ProxyReconnectReport,
    settled: tokio::sync::oneshot::Sender<ProxyReconnectReport>,
}

/// Why a cluster the report still waits for when its timer runs out failed.
const TIMED_OUT: &str = "did not connect in time on the new proxy setting";

/// How a cluster waited on came back, once it did.
enum Outcome {
    Reconnected,
    Failed(String),
}

impl ClusterManager {
    /// Saves `setting`, puts it in use, and connects everything again on it ([`ProxyApplied`]).
    /// Refused before anything changes for a setting that does not check out, an invalid
    /// `settings.toml`, one that was unreadable at startup, or one whose saved proxy is no longer
    /// `expected`, the one the change was reviewed over: the file is then read again, so the
    /// front end hears of the proxy it holds.
    pub(super) async fn apply_proxy(
        &mut self,
        setting: ProxySetting,
        expected: ProxySetting,
    ) -> Result<ProxyApplied, String> {
        setting.check()?;
        let setting = setting.normalized();
        if crate::connection::proxy::setting_unreadable_at_startup() {
            return Err(crate::connection::proxy::ReplaceRefused::UnreadableAtStartup.to_string());
        }
        // What runs now, read before anything changes.
        let routes: HashMap<ClusterId, Option<String>> = self
            .clusters
            .iter()
            .map(|slot| (slot.info.name.clone(), slot.info.proxy_url.clone()))
            .collect();
        let running: BTreeSet<ClusterId> = self
            .clusters
            .iter()
            .map(|slot| slot.info.name.clone())
            .filter(|id| {
                self.active.as_deref() == Some(id.as_str())
                    || self.members.contains_key(id)
                    || self.keep_watching(id)
            })
            .collect();

        let written = setting.clone();
        let written = self
            .try_write_settings("the proxy", move |dir| {
                settings::set_proxy(dir, &written, &expected)
            })
            .await;
        if let Err(why) = written {
            // Perhaps changed on disk since it was reviewed: read it now, before the watcher
            // reports it, so the change is reviewed again over what the file holds.
            if why.contains(settings::PROXY_CHANGED_SINCE_REVIEW) {
                self.reload_settings().await;
            }
            return Err(why);
        }
        // A report still waiting for an earlier change goes out now, against the routes that
        // change loaded, before this one replaces them.
        self.finish_proxy_report("a newer proxy setting replaced it before it connected");
        let generation =
            crate::connection::proxy::replace_setting(&setting).map_err(|e| e.to_string())?;
        tracing::info!("proxy setting replaced; connecting every cluster again");

        // Every slot is behind the setting now, so held: no new connection, session or write
        // starts on its old route. A write reviewed on a slot is refused once the load replaces
        // it, and one on a rerouted context by its moved route too.
        let forwards_ended = self.end_forwards_for_proxy();
        self.publish_holds();
        // A member's sign-in asked for and not yet started was for its old route: forgotten.
        self.drop_member_sign_ins();

        self.suspend_kept_plugins();
        self.marks.changed(self.read_seq);
        let loaded = self.reload_kubeconfig().await;

        let rerouted = self.rerouted_since(&routes);
        let reconnecting: Vec<ClusterId> = running
            .into_iter()
            .filter(|id| self.cluster(id).is_some())
            .collect();

        self.proxy_change.seq += 1;
        let seq = self.proxy_change.seq;
        let (settled, settled_rx) = tokio::sync::oneshot::channel();
        self.proxy_change.pending = Some(Pending {
            seq,
            generation,
            routes,
            waiting: reconnecting.iter().cloned().collect(),
            report: ProxyReconnectReport::default(),
            settled,
        });
        let internal = self.internal_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PROXY_REPORT_AFTER).await;
            let _ = internal.send(super::Internal::ProxyReportDue { change: seq });
        });
        self.sync_proxy_report();
        Ok(ProxyApplied {
            reconnecting,
            rerouted,
            forwards_ended,
            loaded,
            settled: settled_rx,
        })
    }

    /// Moves each cluster the pending report waits for that has settled into it, and sends the
    /// report once none is left.
    pub(super) fn sync_proxy_report(&mut self) {
        let Some(pending) = &self.proxy_change.pending else {
            return;
        };
        let outcomes: Vec<(ClusterId, Outcome)> = pending
            .waiting
            .iter()
            .filter_map(|id| Some((id.clone(), self.proxy_outcome(id)?)))
            .collect();
        let Some(pending) = &mut self.proxy_change.pending else {
            return;
        };
        for (id, outcome) in outcomes {
            pending.waiting.remove(&id);
            match outcome {
                Outcome::Reconnected => pending.report.reconnected.push(id),
                Outcome::Failed(why) => pending.report.failed.push((id, why)),
            }
        }
        let generation = pending.generation;
        let routes_loaded = self
            .clusters
            .iter()
            .all(|slot| slot.proxy_generation == generation);
        if pending.waiting.is_empty() && routes_loaded {
            self.finish_proxy_report(TIMED_OUT);
        }
    }

    /// The report timer of change `change` ran out: what has not settled counts as failed.
    pub(super) fn proxy_report_due(&mut self, change: u64) {
        if self
            .proxy_change
            .pending
            .as_ref()
            .is_some_and(|pending| pending.seq == change)
        {
            self.finish_proxy_report(TIMED_OUT);
        }
    }

    /// The contexts whose route is not what `routes` had.
    fn rerouted_since(&self, routes: &HashMap<ClusterId, Option<String>>) -> Vec<ClusterId> {
        self.clusters
            .iter()
            .filter(|slot| {
                routes
                    .get(&slot.info.name)
                    .is_some_and(|before| *before != slot.info.proxy_url)
            })
            .map(|slot| slot.info.name.clone())
            .collect()
    }

    /// Sends the pending report, with what it still waits for as failed for `why`.
    fn finish_proxy_report(&mut self, why: &str) {
        let Some(mut pending) = self.proxy_change.pending.take() else {
            return;
        };
        pending.report.rerouted = self.rerouted_since(&pending.routes);
        for id in std::mem::take(&mut pending.waiting) {
            pending.report.failed.push((id, why.to_owned()));
        }
        let _ = pending.settled.send(pending.report);
    }

    /// How `id` came back from a proxy change, or None while it still may.
    fn proxy_outcome(&self, id: &str) -> Option<Outcome> {
        let Some(slot) = self.cluster(id) else {
            return Some(Outcome::Failed(
                "it is no longer in your kubeconfig files".to_owned(),
            ));
        };
        if self.held(id) {
            // Waits for a load under the new setting, or the timer.
            return None;
        }
        match &slot.state {
            ConnectionState::Connected(_) => return Some(Outcome::Reconnected),
            ConnectionState::Failed(error) => {
                return Some(Outcome::Failed(error.as_str().to_owned()));
            }
            ConnectionState::Connecting => return None,
            ConnectionState::Disconnected | ConnectionState::AwaitingConfirmation => {}
        }
        if self.kept_read_client(id).is_some() {
            return Some(Outcome::Reconnected);
        }
        match self.keep_status(id) {
            KeepWatching::Watching | KeepWatching::Paused => None,
            KeepWatching::NeedsConfirming => Some(Outcome::Failed(
                "keep watching must be confirmed again for its new route".to_owned(),
            )),
            KeepWatching::SignInNeeded => Some(Outcome::Failed(
                "it needs a sign-in to be watched in the background".to_owned(),
            )),
            // A member may still wait for its turn to connect.
            KeepWatching::Off
                if slot.reconnect
                    || self.active.as_deref() == Some(id)
                    || self.members.contains_key(id) =>
            {
                None
            }
            KeepWatching::Off => Some(Outcome::Failed("it is not connected".to_owned())),
        }
    }
}

#[cfg(test)]
#[path = "proxy_change_tests.rs"]
mod tests;
