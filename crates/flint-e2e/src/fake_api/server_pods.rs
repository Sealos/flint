//! The server's pod log and pod churn helpers: write and stream logs ([`ContainerLog`]), end or
//! break open log streams, and create, delete, replace and restart pods with the watch events a
//! real cluster sends.

use std::time::{Duration, SystemTime};

use anyhow::Result;
use k8s_openapi::jiff::Timestamp;

use super::super::logs::{self, ContainerLog, Key};
use super::super::pods::{self, FakePod};
use super::super::state::State;
use super::super::watch::Outgoing;
use super::{FakeApiServer, Gvr, RequestRecord, Route, lock};

fn pods_gvr() -> Gvr {
    Gvr::core("pods")
}

impl FakeApiServer {
    /// The container a [`ContainerLog`] names, picked as the API server picks it. Panics if the
    /// pod does not exist or the container is ambiguous or unknown: a typo should fail the test.
    fn log_key(state: &State, log: &ContainerLog) -> Key {
        let object = state
            .object(&pods_gvr(), Some(&log.namespace), &log.pod)
            .unwrap_or_else(|e| panic!("{}", e.body["message"]));
        let container = logs::select_container(&object, log.container.as_deref())
            .unwrap_or_else(|e| panic!("{}", e.body["message"]));
        (log.namespace.clone(), log.pod.clone(), container)
    }

    /// Sets the log of the pod's only container (default: three canned lines, written a few
    /// seconds before the server started). See [`FakeApiServer::set_container_log`].
    pub fn set_log(&self, namespace: &str, pod: &str, text: &str) {
        self.set_container_log(&ContainerLog::new(namespace, pod), text);
    }

    /// Replaces a container's log (its current or previous instance) with the lines of `text`,
    /// stamped now. Open streams are not told; use [`FakeApiServer::append_log`] for lines they
    /// should get.
    pub fn set_container_log(&self, log: &ContainerLog, text: &str) {
        let state = lock(&self.inner.state);
        let key = FakeApiServer::log_key(&state, log);
        lock(&self.inner.logs).set(&key, log.previous, text);
    }

    /// Appends the lines of `text` to a container's current log, stamped now (in order, never
    /// two alike), and sends them to its open followed streams. Returns how many streams got
    /// them. Panics for a previous instance: it is not written to any more.
    pub fn append_log(&self, log: &ContainerLog, text: &str) -> usize {
        self.append(log, None, text)
    }

    /// Like [`FakeApiServer::append_log`], with every line stamped `at`, to script lines
    /// sharing a second or written out of order.
    pub fn append_log_at(&self, log: &ContainerLog, at: SystemTime, text: &str) -> usize {
        let at = Timestamp::try_from(at).expect("a time the API can express");
        self.append(log, Some(at), text)
    }

    fn append(&self, log: &ContainerLog, at: Option<Timestamp>, text: &str) -> usize {
        assert!(
            !log.previous,
            "a previous instance is not written to; use set_container_log"
        );
        let state = lock(&self.inner.state);
        let key = FakeApiServer::log_key(&state, log);
        lock(&self.inner.logs).append(&key, at, text)
    }

    /// Ends a container's open log streams cleanly (EOF), as the kubelet does when the
    /// container stops. The pod stays as it is, so a client that checks the pod finds it
    /// running and reconnects. Returns how many streams ended.
    pub fn end_log_streams(&self, log: &ContainerLog) -> usize {
        let state = lock(&self.inner.state);
        let key = FakeApiServer::log_key(&state, log);
        lock(&self.inner.logs).end(&key)
    }

    /// Breaks the connections of a container's open log streams mid-way. Returns how many.
    pub fn drop_log_streams(&self, log: &ContainerLog) -> usize {
        let state = lock(&self.inner.state);
        let key = FakeApiServer::log_key(&state, log);
        lock(&self.inner.logs).drop_streams(&key)
    }

    /// How many followed log streams are open, on every pod. A stream closes when it ends or
    /// its client goes away.
    pub fn open_log_streams(&self) -> usize {
        lock(&self.inner.logs).open_streams(None)
    }

    /// How many followed log streams of one container are open.
    pub fn open_log_streams_of(&self, log: &ContainerLog) -> usize {
        let state = lock(&self.inner.state);
        let key = FakeApiServer::log_key(&state, log);
        lock(&self.inner.logs).open_streams(Some(&key))
    }

    /// Waits until exactly `count` followed log streams are open (on every pod): up after
    /// opening log tabs, down to 0 after closing them.
    pub async fn wait_for_log_streams(&self, count: usize, within: Duration) -> Result<()> {
        tokio::time::timeout(within, async {
            while self.open_log_streams() != count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "not {count} open log stream(s) within {within:?} but {}; last requests:\n{}",
                self.open_log_streams(),
                self.last_requests(10)
            )
        })
    }

    /// Every log request so far, oldest first. [`RequestRecord::parsed_query`] shows its
    /// options (`since_time` on a reconnect, `container`, `previous`).
    pub fn log_requests(&self) -> Vec<RequestRecord> {
        self.requests()
            .into_iter()
            .filter(|r| Route::parse(&r.path).subresource() == Some("log"))
            .collect()
    }

    /// The current object of a pod, to change or to make a [`FakePod::sibling`] of. `None` for
    /// a pod that does not exist or was never recorded in full (kube-system's).
    pub fn pod(&self, namespace: &str, name: &str) -> Option<FakePod> {
        lock(&self.inner.state)
            .held_object(&pods_gvr(), Some(namespace), name)
            .cloned()
            .map(FakePod::from_json)
    }

    /// Creates a pod, created and started now, and sends `ADDED` to the pod watches that select
    /// it (Table and plain). Its containers start with empty logs. Returns how many pod watches
    /// were open. Panics if a pod of that name exists (use [`FakeApiServer::replace_pod`]).
    pub fn add_pod(&self, pod: FakePod) -> usize {
        let mut state = lock(&self.inner.state);
        let gvr = pods_gvr();
        assert!(
            state.row(&gvr, Some(pod.namespace()), pod.name()).is_none(),
            "pod {}/{} exists; use replace_pod",
            pod.namespace(),
            pod.name()
        );
        let mut pod = pod;
        pod.created_at(&self.inner.clock.fixture_now());
        lock(&self.inner.logs).start_pod(pod.namespace(), pod.name(), &pod.container_names());
        self.apply_pod(&mut state, "ADDED", pod)
    }

    /// Deletes a pod: its log streams end cleanly (its containers stop), then pod watches get
    /// `DELETED`. Its logs are forgotten. Returns how many pod watches were open. Panics if the
    /// pod does not exist.
    ///
    /// A real deletion first marks the pod (`MODIFIED` with a `deletionTimestamp`) during its
    /// grace period; the fake skips that step.
    pub fn delete_pod(&self, namespace: &str, name: &str) -> usize {
        let mut state = lock(&self.inner.state);
        self.remove_pod(&mut state, namespace, name)
    }

    fn remove_pod(&self, state: &mut State, namespace: &str, name: &str) -> usize {
        let gvr = pods_gvr();
        let row = state
            .row(&gvr, Some(namespace), name)
            .unwrap_or_else(|| panic!("pods \"{name}\" not found in {namespace}"));
        lock(&self.inner.logs).remove_pod(namespace, name);
        let change = state.change(&gvr, "DELETED", row);
        self.broadcast(&gvr, &Outgoing::Event(change))
    }

    /// Replaces a pod, as a rollout or a StatefulSet does: `old` is deleted
    /// ([`FakeApiServer::delete_pod`]), then `new` is added ([`FakeApiServer::add_pod`]), which
    /// may have the same name (another uid). Returns how many pod watches were open.
    pub fn replace_pod(&self, namespace: &str, old: &str, new: FakePod) -> usize {
        let mut state = lock(&self.inner.state);
        self.remove_pod(&mut state, namespace, old);
        drop(state);
        self.add_pod(new)
    }

    /// Restarts a container after a crash (exit code 1), as the kubelet does under
    /// `restartPolicy: Always`: its open log streams end cleanly, its log becomes the previous
    /// one and a new empty log starts, and pod watches get `MODIFIED` with one more restart and
    /// the last instance in `lastState.terminated`. Returns how many pod watches were open.
    pub fn restart_container(&self, namespace: &str, pod: &str, container: &str) -> usize {
        let mut state = lock(&self.inner.state);
        let mut object = state
            .held_object(&pods_gvr(), Some(namespace), pod)
            .cloned()
            .map(FakePod::from_json)
            .unwrap_or_else(|| panic!("pods \"{pod}\" not found in {namespace}, or not in full"));
        object.restart(container, &self.inner.clock.fixture_now());
        let key = (namespace.to_owned(), pod.to_owned(), container.to_owned());
        lock(&self.inner.logs).restart(&key);
        self.apply_pod(&mut state, "MODIFIED", object)
    }

    /// Stores a pod object with a row printed from it, and tells the pod watches.
    fn apply_pod(&self, state: &mut State, ty: &'static str, pod: FakePod) -> usize {
        let gvr = pods_gvr();
        let object = pod.into_json();
        let cells = pods::table_cells(&object);
        let cells: Vec<(&str, &str)> = cells.iter().map(|(c, v)| (*c, v.as_str())).collect();
        let change = state
            .apply_object(&gvr, ty, object, &cells)
            .unwrap_or_else(|e| panic!("{e}"));
        self.broadcast(&gvr, &Outgoing::Event(change))
    }
}

#[cfg(test)]
#[path = "server_pods_tests.rs"]
mod tests;
