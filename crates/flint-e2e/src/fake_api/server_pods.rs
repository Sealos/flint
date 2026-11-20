//! The server's pod log and pod churn helpers: write and stream logs ([`ContainerLog`]), end or
//! break open log streams, and create, start, crash, delete and replace pods with the watch
//! events a real cluster sends.

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

    /// Creates a pod, created now, and sends `ADDED` to the pod watches that select it (Table
    /// and plain). Its containers start with empty logs; build it with [`FakePod::waiting`] to
    /// add it Pending and start it with [`FakeApiServer::start_container`]. Returns how many
    /// pod watches were open. Panics if a pod of that name exists (see
    /// [`FakeApiServer::replace_pod`]).
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

    /// Starts a container that waits (a Pending pod's, or one in CrashLoopBackOff) or that
    /// stopped. The pod keeps its uid; pod watches get `MODIFIED`. The started instance's log
    /// begins empty. After a stop or a back-off it is a new instance (one more restart), and the
    /// last one's log is the previous one. Returns how many pod watches were open. Panics if it
    /// already runs.
    pub fn start_container(&self, namespace: &str, pod: &str, container: &str) -> usize {
        let mut state = lock(&self.inner.state);
        let mut object = self.held_pod(&state, namespace, pod);
        let key = (namespace.to_owned(), pod.to_owned(), container.to_owned());
        let mut logs = lock(&self.inner.logs);
        if object.start(container, &self.inner.clock.fixture_now()) {
            logs.rotate(&key);
        } else {
            logs.clear_current(&key);
        }
        drop(logs);
        self.apply_pod(&mut state, "MODIFIED", object)
    }

    /// The running container crashes (exit code 1; panics if it does not run): its open log
    /// streams end cleanly, and pod watches
    /// get `MODIFIED` with it terminated. Until it starts again
    /// ([`FakeApiServer::start_container`]), a log request without `previous` reads this
    /// stopped instance, as the kubelet serves it. Returns how many pod watches were open.
    pub fn crash_container(&self, namespace: &str, pod: &str, container: &str) -> usize {
        let mut state = lock(&self.inner.state);
        let mut object = self.held_pod(&state, namespace, pod);
        object.stop(container, 1, &self.inner.clock.fixture_now());
        let key = (namespace.to_owned(), pod.to_owned(), container.to_owned());
        lock(&self.inner.logs).end(&key);
        self.apply_pod(&mut state, "MODIFIED", object)
    }

    /// [`FakeApiServer::crash_container`], then [`FakeApiServer::start_container`] at once:
    /// two `MODIFIED` events. Returns how many pod watches were open.
    pub fn restart_container(&self, namespace: &str, pod: &str, container: &str) -> usize {
        self.crash_container(namespace, pod, container);
        self.start_container(namespace, pod, container)
    }

    /// Deletes a pod gracefully, as `kubectl delete` does: pod watches get `MODIFIED` with a
    /// `deletionTimestamp` (Terminating); then its containers stop (exit 0), their open log
    /// streams end cleanly, and watches get `MODIFIED` with a terminal phase. The pod can still
    /// be read, and its logs too, until [`FakeApiServer::finish_delete`]. Returns how many pod
    /// watches were open. Panics if the pod does not exist, is not held in full, or is already
    /// being deleted.
    pub fn delete_pod(&self, namespace: &str, name: &str) -> usize {
        let mut state = lock(&self.inner.state);
        self.terminate_pod(&mut state, namespace, name)
    }

    /// Starts a graceful deletion whose containers take their time to shut down: pod watches
    /// get `MODIFIED` with a `deletionTimestamp` (Terminating), while the containers keep
    /// running, and their logs keep growing, until [`FakeApiServer::finish_shutdown`]. Returns
    /// how many pod watches were open. Panics like [`FakeApiServer::delete_pod`].
    pub fn begin_delete_pod(&self, namespace: &str, name: &str) -> usize {
        let mut state = lock(&self.inner.state);
        self.mark_terminating(&mut state, namespace, name)
    }

    /// The containers of a pod [`FakeApiServer::begin_delete_pod`] started deleting stop (exit
    /// 0): their open log streams end cleanly, and pod watches get `MODIFIED` with a terminal
    /// phase. The pod stays readable until [`FakeApiServer::finish_delete`]. Returns how many
    /// pod watches were open. Panics unless the pod is being deleted.
    pub fn finish_shutdown(&self, namespace: &str, name: &str) -> usize {
        let mut state = lock(&self.inner.state);
        let object = self.held_pod(&state, namespace, name);
        assert!(
            object
                .as_json()
                .pointer("/metadata/deletionTimestamp")
                .is_some(),
            "pod {namespace}/{name} is not being deleted; call begin_delete_pod first"
        );
        self.stop_pod(&mut state, namespace, name)
    }

    fn terminate_pod(&self, state: &mut State, namespace: &str, name: &str) -> usize {
        self.mark_terminating(state, namespace, name);
        self.stop_pod(state, namespace, name)
    }

    fn mark_terminating(&self, state: &mut State, namespace: &str, name: &str) -> usize {
        let now = self.inner.clock.fixture_now();
        let mut object = self.held_pod(state, namespace, name);
        assert!(
            object
                .as_json()
                .pointer("/metadata/deletionTimestamp")
                .is_none(),
            "pod {namespace}/{name} is already being deleted; call finish_delete"
        );
        object.mark_deleted(&now, 30);
        self.apply_pod(state, "MODIFIED", object)
    }

    fn stop_pod(&self, state: &mut State, namespace: &str, name: &str) -> usize {
        let now = self.inner.clock.fixture_now();
        let mut object = self.held_pod(state, namespace, name);
        object.stop_all(&now);
        lock(&self.inner.logs).end_pod(namespace, name);
        self.apply_pod(state, "MODIFIED", object)
    }

    /// Ends the deletion of a pod [`FakeApiServer::delete_pod`] started: pod watches get
    /// `DELETED`, and the pod and its logs are gone (404). Returns how many pod watches were
    /// open. Panics unless the pod is being deleted.
    pub fn finish_delete(&self, namespace: &str, name: &str) -> usize {
        let mut state = lock(&self.inner.state);
        let pod = self.held_pod(&state, namespace, name);
        assert!(
            pod.as_json()
                .pointer("/metadata/deletionTimestamp")
                .is_some(),
            "pod {namespace}/{name} is not being deleted; call delete_pod first"
        );
        self.remove_pod(&mut state, namespace, name)
    }

    /// Deletes a pod at once, as `kubectl delete --grace-period=0 --force` does: its log
    /// streams end and pod watches get `DELETED`, with no Terminating step. Returns how many
    /// pod watches were open. Panics if the pod does not exist.
    pub fn force_delete_pod(&self, namespace: &str, name: &str) -> usize {
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
        self.broadcast(&gvr, &Outgoing::Event(Box::new(change)))
    }

    /// Replaces a pod, as a rollout or a StatefulSet does: `old` is deleted gracefully and
    /// gone ([`FakeApiServer::delete_pod`], [`FakeApiServer::finish_delete`]), then `new` is
    /// added ([`FakeApiServer::add_pod`]); it may have the same name (another uid). Pod watches
    /// get `MODIFIED`, `MODIFIED`, `DELETED`, `ADDED`. Returns how many were open.
    pub fn replace_pod(&self, namespace: &str, old: &str, new: FakePod) -> usize {
        let mut state = lock(&self.inner.state);
        self.terminate_pod(&mut state, namespace, old);
        self.remove_pod(&mut state, namespace, old);
        drop(state);
        self.add_pod(new)
    }

    /// The full object of a pod; panics if there is none.
    fn held_pod(&self, state: &State, namespace: &str, pod: &str) -> FakePod {
        state
            .held_object(&pods_gvr(), Some(namespace), pod)
            .cloned()
            .map(FakePod::from_json)
            .unwrap_or_else(|| panic!("pods \"{pod}\" not found in {namespace}, or not in full"))
    }

    /// Stores a pod object with a row printed from it (its Restarts age rendered when served),
    /// and tells the pod watches. `ty` is `ADDED` or `MODIFIED`.
    fn apply_pod(&self, state: &mut State, ty: &'static str, pod: FakePod) -> usize {
        let gvr = pods_gvr();
        let mut object = pod.into_json();
        let printed = pods::table_cells(&object);
        let cells: Vec<(&str, &str)> = printed
            .cells
            .iter()
            .map(|(c, v)| (*c, v.as_str()))
            .collect();
        let mut row = state
            .row_for_object(&gvr, &mut object, &cells)
            .unwrap_or_else(|e| panic!("{e}"));
        let restarts = state
            .columns(&gvr)
            .as_array()
            .and_then(|c| c.iter().position(|c| c["name"] == "Restarts"));
        if let (Some(at), Some(index)) = (&printed.last_restart, restarts) {
            row = row.with_last_restart(index, at);
        }
        let change = state.apply_object(&gvr, ty, object, row);
        self.broadcast(&gvr, &Outgoing::Event(Box::new(change)))
    }
}

#[cfg(test)]
#[path = "server_pods_tests.rs"]
mod tests;
