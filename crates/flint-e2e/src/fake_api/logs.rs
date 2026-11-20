//! Pod logs, served as the API server serves `GET /api/v1/namespaces/<ns>/pods/<pod>/log`.
//!
//! What a real cluster does, and where it is decided (Kubernetes v1.37, the fixtures' version):
//!
//! - **Options** (`ValidatePodLogOptions`, `pkg/apis/core/validation`): a negative `tailLines`,
//!   a `limitBytes` below 1, a `sinceSeconds` below 1, or both `sinceSeconds` and `sinceTime`
//!   is a 422 `Invalid` about `PodLogOptions`, checked before the pod is looked up.
//! - **Container** (`validateContainer`, `pkg/registry/core/pod/strategy.go`): without
//!   `container`, a pod with one container means that one; with more, a 400 "a container name
//!   must be specified for pod X, choose one of: [a b]". An unknown name is a 400 "container c
//!   is not valid for pod X".
//! - **Instance** (`validateContainerLogStatus`, `pkg/kubelet/kubelet_pods.go`): `previous`
//!   reads the last terminated instance, else a 400 "previous terminated container ... not
//!   found". Otherwise a running container is read; a terminated one, or one waiting after an
//!   earlier instance terminated (CrashLoopBackOff), gives that instance's log; one waiting
//!   with no earlier instance is a 400 "... is waiting to start: `<reason>`". The kubelet's 400 is
//!   passed on as a `BadRequest` with its text (`GenericHttpResponseChecker`).
//! - **Reading** (`ReadLogs`, `k8s.io/cri-client/pkg/logs/logs.go`): `tailLines` picks the last
//!   lines of the log first, then lines older than the "since" instant are skipped (a line at
//!   exactly that instant is kept). `sinceSeconds` counts back from now; `sinceTime` is passed
//!   to the kubelet at second precision (`LogLocation` formats it as RFC 3339). With
//!   `timestamps`, each line starts with its time in UTC, nine fractional digits, and a space.
//!   `limitBytes` cuts the output after that many bytes.
//! - **Following** (`follow=true`) keeps the response open while the container runs: new lines
//!   arrive as they are written, and the response ends cleanly (EOF) when the container stops,
//!   whether it crashed, restarted, or its pod was deleted. A container that is not running, or
//!   a previous instance, is read to its end and the response ends.
//!
//! kube-rs (`kube-core` 5.1, `Request::logs`) already rounds `sinceTime` down to the second
//! before sending it, as client-go does.

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use bytes::Bytes;
use k8s_openapi::jiff::Timestamp;
use serde_json::Value;
use tokio::sync::mpsc;

use super::clock::Clock;
use super::route::Query;
use super::state::ApiError;

/// A container's log, for the log helpers of [`super::FakeApiServer`]: one container of one
/// pod, its current instance or the previous one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContainerLog {
    pub namespace: String,
    pub pod: String,
    /// `None`: the pod's only container (as the API server picks it).
    pub container: Option<String>,
    /// The previous (terminated) instance instead of the current one.
    pub previous: bool,
}

impl ContainerLog {
    /// The current instance of the pod's only container.
    pub fn new(namespace: &str, pod: &str) -> Self {
        ContainerLog {
            namespace: namespace.to_owned(),
            pod: pod.to_owned(),
            container: None,
            previous: false,
        }
    }

    /// A named container.
    pub fn container(mut self, container: &str) -> Self {
        self.container = Some(container.to_owned());
        self
    }

    /// The previous instance (what `previous=true` reads).
    pub fn previous(mut self) -> Self {
        self.previous = true;
        self
    }
}

/// A container of a pod: namespace, pod, container.
pub(crate) type Key = (String, String, String);

/// The options of a log request, validated.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LogQuery {
    pub container: Option<String>,
    pub follow: bool,
    pub previous: bool,
    pub timestamps: bool,
    pub tail_lines: Option<i64>,
    pub limit_bytes: Option<i64>,
    pub since_seconds: Option<i64>,
    /// Already at second precision.
    pub since_time: Option<Timestamp>,
}

/// One `field.Error` of a validation, as `Error()` prints it.
fn field_error(field: &str, body: String) -> String {
    format!("{field}: {body}")
}

impl LogQuery {
    /// Validates a request's options as `ValidatePodLogOptions` does; `pod` names the pod in the
    /// 422.
    pub fn parse(query: &Query, pod: &str) -> Result<LogQuery, ApiError> {
        let mut errors = Vec::new();
        if let Some(tail) = query.tail_lines.filter(|t| *t < 0) {
            errors.push(field_error(
                "tailLines",
                format!("Invalid value: {tail}: must be greater than or equal to 0"),
            ));
        }
        if let Some(limit) = query.limit_bytes.filter(|l| *l < 1) {
            errors.push(field_error(
                "limitBytes",
                format!("Invalid value: {limit}: must be greater than 0"),
            ));
        }
        match (query.since_seconds, &query.since_time) {
            (Some(_), Some(_)) => errors.push(field_error(
                "",
                "Forbidden: at most one of `sinceTime` or `sinceSeconds` may be specified".into(),
            )),
            (Some(seconds), None) if seconds < 1 => errors.push(field_error(
                "sinceSeconds",
                format!("Invalid value: {seconds}: must be greater than 0"),
            )),
            _ => {}
        }
        if !errors.is_empty() {
            // `ToAggregate` prints one error bare and several in brackets.
            let joined = if errors.len() == 1 {
                errors.remove(0)
            } else {
                format!("[{}]", errors.join(", "))
            };
            return Err(ApiError::new(
                422,
                "Invalid",
                format!("PodLogOptions \"{pod}\" is invalid: {joined}"),
            ));
        }
        let since_time = match &query.since_time {
            Some(text) => {
                let at: Timestamp = text.parse().map_err(|_| {
                    ApiError::new(
                        400,
                        "BadRequest",
                        format!("invalid sinceTime {text:?}: not an RFC 3339 time"),
                    )
                })?;
                // `LogLocation` hands it to the kubelet as RFC 3339, without fractions.
                Some(Timestamp::from_second(at.as_second()).unwrap_or(at))
            }
            None => None,
        };
        Ok(LogQuery {
            container: query.container.clone(),
            follow: query.follow,
            previous: query.previous,
            timestamps: query.timestamps,
            tail_lines: query.tail_lines,
            limit_bytes: query.limit_bytes,
            since_seconds: query.since_seconds,
            since_time,
        })
    }

    /// The instant older lines are skipped before, as the kubelet derives it.
    fn since(&self, now: Timestamp) -> Option<Timestamp> {
        let from_seconds = self.since_seconds.and_then(|s| {
            now.checked_sub(Duration::from_secs(u64::try_from(s).unwrap_or(0)))
                .ok()
        });
        match (from_seconds, self.since_time) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

fn bad_request(message: String) -> ApiError {
    ApiError::new(400, "BadRequest", message)
}

/// The names of a pod's containers: init, regular, then ephemeral (`podutil.VisitContainers`).
fn all_containers(pod: &Value) -> Vec<&str> {
    ["initContainers", "containers", "ephemeralContainers"]
        .iter()
        .flat_map(|list| {
            pod.pointer(&format!("/spec/{list}"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .collect()
}

/// The container a request reads (`validateContainer`). A pod served from its row only (no
/// recorded spec, like the kube-system pods) takes any container name, or "" for none.
pub(crate) fn select_container(pod: &Value, requested: Option<&str>) -> Result<String, ApiError> {
    let name = pod
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(regular) = pod.pointer("/spec/containers").and_then(Value::as_array) else {
        return Ok(requested.unwrap_or("").to_owned());
    };
    match requested {
        Some(container) if all_containers(pod).contains(&container) => Ok(container.to_owned()),
        Some(container) => Err(bad_request(format!(
            "container {container} is not valid for pod {name}"
        ))),
        None => match regular.as_slice() {
            [only] => Ok(only["name"].as_str().unwrap_or("").to_owned()),
            [] => Err(bad_request(format!(
                "a container name must be specified for pod {name}"
            ))),
            _ => Err(bad_request(format!(
                "a container name must be specified for pod {name}, choose one of: [{}]",
                all_containers(pod).join(" ")
            ))),
        },
    }
}

/// Which instance of a container a request reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Instance {
    /// The previous (last terminated) instance.
    pub previous: bool,
    /// It runs, so a followed stream stays open.
    pub running: bool,
}

/// The instance a request reads (`validateContainerLogStatus`). A pod served from its row only
/// (no recorded status) counts as running.
pub(crate) fn instance(pod: &Value, container: &str, previous: bool) -> Result<Instance, ApiError> {
    let pod_name = pod
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if pod.get("status").is_none() {
        return Ok(Instance {
            previous,
            running: !previous,
        });
    }
    let status = [
        "containerStatuses",
        "initContainerStatuses",
        "ephemeralContainerStatuses",
    ]
    .iter()
    .flat_map(|list| {
        pod.pointer(&format!("/status/{list}"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    })
    .find(|s| s["name"] == container)
    .ok_or_else(|| {
        bad_request(format!(
            "container {container:?} in pod {pod_name:?} is not available"
        ))
    })?;
    let has_id = |terminated: &Value| {
        terminated
            .get("containerID")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
    };
    let last = status.pointer("/lastState/terminated");
    let state = &status["state"];
    let current = |running| {
        Ok(Instance {
            previous: false,
            running,
        })
    };
    let last_instance = Ok(Instance {
        previous: true,
        running: false,
    });
    let terminated_error = || {
        Err(bad_request(format!(
            "container {container:?} in pod {pod_name:?} is terminated"
        )))
    };
    if previous {
        return match last {
            Some(last) if has_id(last) => last_instance,
            _ => Err(bad_request(format!(
                "previous terminated container {container:?} in pod {pod_name:?} not found"
            ))),
        };
    }
    if state.get("running").is_some() {
        return current(true);
    }
    if let Some(terminated) = state.get("terminated") {
        return match last {
            _ if has_id(terminated) => current(false),
            Some(last) if has_id(last) => last_instance,
            _ => terminated_error(),
        };
    }
    if let Some(last) = last {
        return if has_id(last) {
            last_instance
        } else {
            terminated_error()
        };
    }
    let waiting = |reason: &str| {
        Err(bad_request(format!(
            "container {container:?} in pod {pod_name:?} is waiting to start{reason}"
        )))
    };
    match state.pointer("/waiting/reason").and_then(Value::as_str) {
        Some("ErrImagePull") => waiting(": image can't be pulled"),
        Some("ImagePullBackOff") => waiting(": trying and failing to pull image"),
        Some(reason) => waiting(&format!(": {reason}")),
        None if state.get("waiting").is_some() => waiting(": "),
        None => waiting(" - no logs yet"),
    }
}

/// One line of a log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    at: Timestamp,
    /// Without its newline.
    text: String,
}

/// A container's instances. `None`: never written, so a few canned lines stand in.
#[derive(Debug, Default)]
struct Instances {
    current: Option<Vec<Entry>>,
    previous: Option<Vec<Entry>>,
}

/// What a log response sends: chunks of text, or an error that breaks the connection.
pub(crate) type Chunk = Result<Bytes, io::Error>;

/// An open followed response.
#[derive(Debug)]
struct Follower {
    key: Key,
    timestamps: bool,
    since: Option<Timestamp>,
    /// Bytes left under `limitBytes`.
    remaining: Option<u64>,
    tx: mpsc::UnboundedSender<Chunk>,
}

impl Follower {
    /// Sends what of `entries` passes the follower's filter. Returns false once the follower
    /// is done (the client went away, or the byte limit was reached).
    fn send(&mut self, entries: &[Entry]) -> bool {
        let text = render(entries, self.timestamps, self.since, &mut self.remaining);
        if !text.is_empty() && self.tx.send(Ok(Bytes::from(text))).is_err() {
            return false;
        }
        self.remaining != Some(0)
    }
}

/// The time format of the kubelet's timestamps (`RFC3339NanoFixed`), in UTC.
pub(crate) fn format_timestamp(at: Timestamp) -> String {
    let seconds = super::clock::format(at.as_second(), 0);
    format!(
        "{}.{:09}Z",
        seconds.trim_end_matches('Z'),
        at.subsec_nanosecond()
    )
}

/// The text of `entries` from `since` on, with timestamps if asked, cut at `remaining` bytes
/// (which it counts down).
fn render(
    entries: &[Entry],
    timestamps: bool,
    since: Option<Timestamp>,
    remaining: &mut Option<u64>,
) -> String {
    let mut out = String::new();
    for entry in entries {
        if since.is_some_and(|since| entry.at < since) {
            continue;
        }
        if *remaining == Some(0) {
            break;
        }
        let mut line = if timestamps {
            format!("{} {}\n", format_timestamp(entry.at), entry.text)
        } else {
            format!("{}\n", entry.text)
        };
        if let Some(left) = remaining {
            let cut = usize::try_from(*left).unwrap_or(usize::MAX);
            if line.len() > cut {
                let mut end = cut;
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                line.truncate(end);
            }
            *left -= line.len() as u64;
        }
        out.push_str(&line);
    }
    out
}

/// Every pod log the server holds, and the followed responses reading them.
#[derive(Debug)]
pub(crate) struct Logs {
    clock: Clock,
    instances: HashMap<Key, Instances>,
    followers: Vec<Follower>,
    /// The newest timestamp given to a line, so lines written in a row are ordered.
    last: Option<Timestamp>,
}

impl Logs {
    pub fn new(clock: Clock) -> Self {
        Logs {
            clock,
            instances: HashMap::new(),
            followers: Vec::new(),
            last: None,
        }
    }

    /// The lines that stand in for a log nobody wrote: three, a few seconds before the server
    /// started.
    fn canned(&self, key: &Key, previous: bool) -> Vec<Entry> {
        let (namespace, pod, _) = key;
        let what = if previous { "previous log" } else { "log" };
        (1..=3)
            .map(|n| Entry {
                at: self
                    .clock
                    .start()
                    .checked_sub(Duration::from_secs(4 - n))
                    .unwrap_or(Timestamp::UNIX_EPOCH),
                text: format!("fake {what} line {n} of {namespace}/{pod}"),
            })
            .collect()
    }

    fn entries(&mut self, key: &Key, previous: bool) -> &mut Vec<Entry> {
        let canned = self.canned(key, previous);
        let instances = self.instances.entry(key.clone()).or_default();
        let slot = if previous {
            &mut instances.previous
        } else {
            &mut instances.current
        };
        slot.get_or_insert(canned)
    }

    /// A time for a new line: `at`, or now, never before the last line written without one.
    fn stamp(&mut self, at: Option<Timestamp>) -> Timestamp {
        let at = at.unwrap_or_else(|| {
            let now = self.clock.now_precise();
            match self.last {
                Some(last) if now <= last => {
                    last.checked_add(Duration::from_nanos(1)).unwrap_or(last)
                }
                _ => now,
            }
        });
        self.last = Some(self.last.map_or(at, |last| last.max(at)));
        at
    }

    fn lines(&mut self, text: &str, at: Option<Timestamp>) -> Vec<Entry> {
        text.lines()
            .map(|line| Entry {
                at: self.stamp(at),
                text: line.to_owned(),
            })
            .collect()
    }

    /// Opens a response on the log of `key`: what is written already, filtered by `query`, and
    /// then, when following a running instance, every line appended until the stream ends.
    pub fn open(
        &mut self,
        key: &Key,
        instance: Instance,
        query: &LogQuery,
    ) -> mpsc::UnboundedReceiver<Chunk> {
        let now = self.clock.now_precise();
        let since = query.since(now);
        let entries = self.entries(key, instance.previous);
        let from = match query.tail_lines {
            Some(n) => entries
                .len()
                .saturating_sub(usize::try_from(n).unwrap_or(0)),
            None => 0,
        };
        let mut remaining = query.limit_bytes.map(|l| l.unsigned_abs());
        let text = render(&entries[from..], query.timestamps, since, &mut remaining);
        let (tx, rx) = mpsc::unbounded_channel();
        if !text.is_empty() {
            let _ = tx.send(Ok(Bytes::from(text)));
        }
        if query.follow && instance.running && !instance.previous && remaining != Some(0) {
            self.followers.push(Follower {
                key: key.clone(),
                timestamps: query.timestamps,
                since,
                remaining,
                tx,
            });
        }
        rx
    }

    /// Replaces an instance's log. Open streams are not told: a log is only ever appended to.
    pub fn set(&mut self, key: &Key, previous: bool, text: &str) {
        let lines = self.lines(text, None);
        *self.entries(key, previous) = lines;
    }

    /// Appends lines to the current instance (at `at`, or now) and sends them to its open
    /// streams. Returns how many streams got them.
    pub fn append(&mut self, key: &Key, at: Option<Timestamp>, text: &str) -> usize {
        let lines = self.lines(text, at);
        self.entries(key, false).extend(lines.iter().cloned());
        let mut reached = 0;
        self.followers.retain_mut(|f| {
            if f.tx.is_closed() {
                return false;
            }
            if f.key != *key {
                return true;
            }
            reached += 1;
            f.send(&lines)
        });
        reached
    }

    /// Ends the open streams of `key` cleanly, as when the container stops. Returns how many.
    pub fn end(&mut self, key: &Key) -> usize {
        self.close(|k| k == key, None)
    }

    /// Breaks the connections of `key`'s open streams mid-way. Returns how many.
    pub fn drop_streams(&mut self, key: &Key) -> usize {
        self.close(|k| k == key, Some("fault: log connection dropped"))
    }

    fn close(&mut self, matches: impl Fn(&Key) -> bool, error: Option<&str>) -> usize {
        let mut closed = 0;
        self.followers.retain(|f| {
            if f.tx.is_closed() {
                return false;
            }
            if !matches(&f.key) {
                return true;
            }
            if let Some(error) = error {
                let _ = f.tx.send(Err(io::Error::other(error)));
            }
            closed += 1;
            false
        });
        closed
    }

    /// Open streams, of `key` or of every container.
    pub fn open_streams(&mut self, key: Option<&Key>) -> usize {
        self.followers.retain(|f| !f.tx.is_closed());
        self.followers
            .iter()
            .filter(|f| key.is_none_or(|k| f.key == *k))
            .count()
    }

    /// The container restarted: its current log becomes the previous one, a new empty log
    /// starts, and its open streams end.
    pub fn restart(&mut self, key: &Key) {
        self.end(key);
        let current = std::mem::take(self.entries(key, false));
        let instances = self.instances.entry(key.clone()).or_default();
        instances.previous = Some(current);
    }

    /// The pod is gone: its streams end and its logs are forgotten, so a pod created under its
    /// name starts with its own.
    pub fn remove_pod(&mut self, namespace: &str, pod: &str) -> usize {
        let of_pod = |k: &Key| k.0 == namespace && k.1 == pod;
        self.instances.retain(|k, _| !of_pod(k));
        self.close(of_pod, None)
    }

    /// A new pod starts with empty logs (no canned lines) for these containers.
    pub fn start_pod(&mut self, namespace: &str, pod: &str, containers: &[String]) {
        self.remove_pod(namespace, pod);
        for container in containers {
            let key = (namespace.to_owned(), pod.to_owned(), container.clone());
            self.instances.insert(
                key,
                Instances {
                    current: Some(Vec::new()),
                    previous: None,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::UNIX_EPOCH;

    fn key(container: &str) -> Key {
        ("demo".into(), "web".into(), container.into())
    }

    fn logs() -> Logs {
        Logs::new(Clock::new(UNIX_EPOCH + Duration::from_secs(1_900_000_000)))
    }

    fn at(seconds: i64, nanos: i32) -> Timestamp {
        Timestamp::new(seconds, nanos).unwrap()
    }

    fn query(text: &str) -> LogQuery {
        LogQuery::parse(&Query::parse(Some(text)), "web").unwrap()
    }

    fn running() -> Instance {
        Instance {
            previous: false,
            running: true,
        }
    }

    /// Everything a response has sent so far, and whether it ended (`Some(true)`: cleanly).
    fn drain(rx: &mut mpsc::UnboundedReceiver<Chunk>) -> (String, Option<bool>) {
        let mut out = String::new();
        loop {
            match rx.try_recv() {
                Ok(Ok(bytes)) => out.push_str(std::str::from_utf8(&bytes).unwrap()),
                Ok(Err(_)) => return (out, Some(false)),
                Err(mpsc::error::TryRecvError::Empty) => return (out, None),
                Err(mpsc::error::TryRecvError::Disconnected) => return (out, Some(true)),
            }
        }
    }

    fn pod(spec: Value, statuses: Value) -> Value {
        json!({"metadata": {"name": "web"}, "spec": spec, "status": {"containerStatuses": statuses}})
    }

    #[test]
    fn options_are_validated_like_the_api_server() {
        let err = |text: &str| {
            LogQuery::parse(&Query::parse(Some(text)), "web")
                .unwrap_err()
                .body["message"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            err("tailLines=-1"),
            "PodLogOptions \"web\" is invalid: tailLines: Invalid value: -1: must be greater \
             than or equal to 0"
        );
        assert_eq!(
            err("sinceSeconds=5&sinceTime=2026-01-01T00:00:00Z&limitBytes=0"),
            "PodLogOptions \"web\" is invalid: [limitBytes: Invalid value: 0: must be greater \
             than 0, : Forbidden: at most one of `sinceTime` or `sinceSeconds` may be specified]"
        );
        assert!(err("sinceSeconds=0").contains("sinceSeconds: Invalid value: 0"));
        let invalid = LogQuery::parse(&Query::parse(Some("tailLines=-1")), "web").unwrap_err();
        assert_eq!(
            (invalid.code, invalid.body["reason"].as_str()),
            (422, Some("Invalid"))
        );
        let bad = LogQuery::parse(&Query::parse(Some("sinceTime=yesterday")), "web").unwrap_err();
        assert_eq!(bad.code, 400);
        // sinceTime reaches the kubelet without fractions.
        let q = query("sinceTime=2026-01-01T00:00:01.900Z&follow=true&timestamps=true");
        assert_eq!(q.since_time, Some("2026-01-01T00:00:01Z".parse().unwrap()));
        assert!(q.follow && q.timestamps && !q.previous);
    }

    #[test]
    fn since_counts_back_from_now_or_takes_the_later_bound() {
        let now = at(1000, 0);
        assert_eq!(query("sinceSeconds=10").since(now), Some(at(990, 0)));
        assert_eq!(query("").since(now), None);
        let both = LogQuery {
            since_seconds: Some(10),
            since_time: Some(at(995, 0)),
            ..LogQuery::default()
        };
        assert_eq!(both.since(now), Some(at(995, 0)));
    }

    #[test]
    fn containers_are_picked_like_the_api_server() {
        let one = pod(json!({"containers": [{"name": "app"}]}), json!([]));
        assert_eq!(select_container(&one, None).unwrap(), "app");
        assert_eq!(select_container(&one, Some("app")).unwrap(), "app");
        let err = select_container(&one, Some("nope")).unwrap_err();
        assert_eq!(err.code, 400);
        assert_eq!(
            err.body["message"],
            "container nope is not valid for pod web"
        );
        let two = pod(
            json!({"initContainers": [{"name": "init"}], "containers": [{"name": "app"}, {"name": "side"}]}),
            json!([]),
        );
        assert_eq!(
            select_container(&two, None).unwrap_err().body["message"],
            "a container name must be specified for pod web, choose one of: [init app side]"
        );
        assert_eq!(select_container(&two, Some("init")).unwrap(), "init");
        let none = pod(json!({"containers": []}), json!([]));
        assert_eq!(
            select_container(&none, None).unwrap_err().body["message"],
            "a container name must be specified for pod web"
        );
        // A pod known only from its row takes any name.
        let bare = json!({"metadata": {"name": "web"}});
        assert_eq!(select_container(&bare, Some("x")).unwrap(), "x");
        assert_eq!(select_container(&bare, None).unwrap(), "");
    }

    #[test]
    fn instances_are_picked_like_the_kubelet() {
        let spec = json!({"containers": [{"name": "app"}]});
        let with = |status: Value| {
            let mut merged = json!({"name": "app", "containerID": "c://2"});
            for (k, v) in status.as_object().unwrap() {
                merged[k] = v.clone();
            }
            pod(spec.clone(), json!([merged]))
        };
        let running = with(json!({"state": {"running": {}}, "lastState": {}}));
        assert_eq!(
            instance(&running, "app", false).unwrap(),
            Instance {
                previous: false,
                running: true
            }
        );
        let message = |pod: &Value, previous| {
            instance(pod, "app", previous).unwrap_err().body["message"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            message(&running, true),
            "previous terminated container \"app\" in pod \"web\" not found"
        );
        // CrashLoopBackOff: the last instance is read, even without `previous`.
        let crashing = with(json!({"state": {"waiting": {"reason": "CrashLoopBackOff"}},
                                   "lastState": {"terminated": {"containerID": "c://1"}}}));
        let last = Instance {
            previous: true,
            running: false,
        };
        assert_eq!(instance(&crashing, "app", false).unwrap(), last);
        assert_eq!(instance(&crashing, "app", true).unwrap(), last);
        let done = with(json!({"state": {"terminated": {"containerID": "c://2", "exitCode": 0}}}));
        assert_eq!(
            instance(&done, "app", false).unwrap(),
            Instance {
                previous: false,
                running: false
            }
        );
        let never_ran = with(json!({"state": {"terminated": {"containerID": ""}}}));
        assert_eq!(
            message(&never_ran, false),
            "container \"app\" in pod \"web\" is terminated"
        );
        let pulling = with(json!({"state": {"waiting": {"reason": "ImagePullBackOff"}}}));
        assert_eq!(
            message(&pulling, false),
            "container \"app\" in pod \"web\" is waiting to start: trying and failing to pull image"
        );
        let pull = with(json!({"state": {"waiting": {"reason": "ErrImagePull"}}}));
        assert!(message(&pull, false).ends_with("is waiting to start: image can't be pulled"));
        let creating = with(json!({"state": {"waiting": {"reason": "ContainerCreating"}}}));
        assert!(message(&creating, false).ends_with("is waiting to start: ContainerCreating"));
        let unknown = with(json!({"state": {}}));
        assert!(message(&unknown, false).ends_with("is waiting to start - no logs yet"));
        assert_eq!(
            instance(&running, "nope", false).unwrap_err().body["message"],
            "container \"nope\" in pod \"web\" is not available"
        );
        let bare = json!({"metadata": {"name": "web"}});
        assert!(instance(&bare, "", false).unwrap().running);
    }

    #[test]
    fn timestamps_are_fixed_width_utc() {
        assert_eq!(
            format_timestamp(at(1_767_225_600, 1_500)),
            "2026-01-01T00:00:00.000001500Z"
        );
        assert_eq!(
            format_timestamp(at(1_767_225_601, 0)),
            "2026-01-01T00:00:01.000000000Z"
        );
    }

    #[test]
    fn tail_comes_before_since_and_since_keeps_its_own_second() {
        let mut logs = logs();
        let k = key("app");
        logs.set(&k, false, "");
        logs.append(&k, Some(at(100, 0)), "a\nb");
        logs.append(&k, Some(at(101, 0)), "c");
        logs.append(&k, Some(at(101, 500)), "d");
        logs.append(&k, Some(at(102, 0)), "e");
        let finished = Instance {
            previous: false,
            running: false,
        };
        let read = |logs: &mut Logs, q: &str| drain(&mut logs.open(&k, finished, &query(q))).0;
        assert_eq!(read(&mut logs, ""), "a\nb\nc\nd\ne\n");
        assert_eq!(read(&mut logs, "tailLines=2"), "d\ne\n");
        assert_eq!(read(&mut logs, "tailLines=0"), "");
        // The last four lines, then those from 101 s on: the tail is taken first.
        let since = format!("sinceTime={}&tailLines=4", format_timestamp(at(101, 300)));
        assert_eq!(read(&mut logs, &since), "c\nd\ne\n");
        assert_eq!(read(&mut logs, "limitBytes=5"), "a\nb\nc");
        let stamped = read(&mut logs, "timestamps=true&tailLines=1");
        assert_eq!(stamped, "1970-01-01T00:01:42.000000000Z e\n");
    }

    #[test]
    fn unwritten_logs_are_canned_before_the_start() {
        let mut logs = logs();
        let k = key("app");
        let (text, end) = drain(&mut logs.open(&k, running(), &query("timestamps=true")));
        assert_eq!(end, Some(true), "not following: the response ends");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "2030-03-17T17:46:37.000000000Z fake log line 1 of demo/web"
        );
        let previous = Instance {
            previous: true,
            running: false,
        };
        let (text, _) = drain(&mut logs.open(&k, previous, &query("")));
        assert!(text.starts_with("fake previous log line 1 of demo/web\n"));
    }

    #[test]
    fn followers_get_appended_lines_until_the_end() {
        let mut logs = logs();
        let (app, side) = (key("app"), key("side"));
        logs.start_pod("demo", "web", &["app".into(), "side".into()]);
        logs.append(&app, None, "before");
        let mut rx = logs.open(&app, running(), &query("follow=true&tailLines=10"));
        let mut other = logs.open(&side, running(), &query("follow=true"));
        assert_eq!(drain(&mut rx), ("before\n".into(), None));
        assert_eq!(logs.open_streams(None), 2);
        assert_eq!(logs.open_streams(Some(&app)), 1);
        assert_eq!(logs.append(&app, None, "one\ntwo\n"), 1);
        assert_eq!(drain(&mut rx), ("one\ntwo\n".into(), None));
        assert_eq!(drain(&mut other), (String::new(), None));
        // Lines written without a time are ordered, even within one clock tick.
        let stamped = drain(&mut logs.open(&app, running(), &query("timestamps=true"))).0;
        let times: Vec<&str> = stamped
            .lines()
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert!(times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
        // A clean end, then a dropped connection.
        assert_eq!(logs.end(&app), 1);
        assert_eq!(drain(&mut rx).1, Some(true));
        assert_eq!(logs.drop_streams(&side), 1);
        assert_eq!(drain(&mut other).1, Some(false));
        assert_eq!(logs.open_streams(None), 0);
        assert_eq!(logs.append(&app, None, "later"), 0);
    }

    #[test]
    fn closed_responses_stop_counting() {
        let mut logs = logs();
        let k = key("app");
        let rx = logs.open(&k, running(), &query("follow=true"));
        assert_eq!(logs.open_streams(None), 1);
        drop(rx);
        assert_eq!(logs.open_streams(None), 0);
        assert_eq!(logs.append(&k, None, "x"), 0);
    }

    #[test]
    fn only_running_current_instances_are_followed() {
        let mut logs = logs();
        let k = key("app");
        let stopped = Instance {
            previous: false,
            running: false,
        };
        let previous = Instance {
            previous: true,
            running: false,
        };
        let mut a = logs.open(&k, stopped, &query("follow=true"));
        let mut b = logs.open(&k, previous, &query("follow=true"));
        assert_eq!(drain(&mut a).1, Some(true));
        assert_eq!(drain(&mut b).1, Some(true));
        // A byte limit already reached ends the stream too.
        let mut c = logs.open(&k, running(), &query("follow=true&limitBytes=3"));
        assert_eq!(drain(&mut c), ("fak".into(), Some(true)));
        let mut d = logs.open(
            &k,
            running(),
            &query("follow=true&tailLines=0&limitBytes=4"),
        );
        logs.append(&k, None, "abcdef");
        assert_eq!(drain(&mut d), ("abcd".into(), Some(true)));
        assert_eq!(logs.open_streams(None), 0);
    }

    #[test]
    fn followers_skip_lines_written_before_their_since() {
        let mut logs = logs();
        let k = key("app");
        logs.set(&k, false, "");
        let since = format!("follow=true&sinceTime={}", format_timestamp(at(200, 0)));
        let mut rx = logs.open(&k, running(), &query(&since));
        logs.append(&k, Some(at(150, 0)), "old");
        logs.append(&k, Some(at(250, 0)), "new");
        assert_eq!(drain(&mut rx).0, "new\n");
    }

    #[test]
    fn restarts_keep_the_last_instance_and_end_streams() {
        let mut logs = logs();
        let k = key("app");
        logs.set(&k, false, "first run\n");
        let mut rx = logs.open(&k, running(), &query("follow=true"));
        drain(&mut rx);
        logs.restart(&k);
        assert_eq!(drain(&mut rx).1, Some(true));
        let previous = Instance {
            previous: true,
            running: false,
        };
        assert_eq!(
            drain(&mut logs.open(&k, previous, &query(""))).0,
            "first run\n"
        );
        assert_eq!(drain(&mut logs.open(&k, running(), &query(""))).0, "");
        logs.set(&k, true, "replaced\n");
        assert_eq!(
            drain(&mut logs.open(&k, previous, &query(""))).0,
            "replaced\n"
        );
    }

    #[test]
    fn removed_pods_end_their_streams_and_forget_their_logs() {
        let mut logs = logs();
        let k = key("app");
        logs.start_pod("demo", "web", &["app".into()]);
        logs.append(&k, None, "old pod");
        let mut rx = logs.open(&k, running(), &query("follow=true"));
        let mut elsewhere = logs.open(
            &("demo".into(), "db".into(), "app".into()),
            running(),
            &query("follow=true"),
        );
        assert_eq!(logs.remove_pod("demo", "web"), 1);
        assert_eq!(drain(&mut rx).1, Some(true));
        assert_eq!(drain(&mut elsewhere).1, None);
        // A pod created under the name starts empty.
        logs.start_pod("demo", "web", &["app".into()]);
        assert_eq!(drain(&mut logs.open(&k, running(), &query(""))).0, "");
    }
}
