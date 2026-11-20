//! Where a log session reads lines from: the API server (`Api::<Pod>::log_stream`) or, in tests,
//! a fake.

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{AsyncBufRead, AsyncBufReadExt as _, FutureExt as _, StreamExt as _};
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, LogParams};

use super::line::MAX_LINE_BYTES;
use super::session::{LogSince, LogTarget};
use crate::api_error::{ApiError, ApiErrorKind};
use crate::cluster::{ClusterError, Target};

/// One request to the log endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogRequest {
    pub target: LogTarget,
    /// The pod's uid, when the stream belongs to a pod set: opening checks the pod by that name
    /// is still the same pod, so a stream never reads a pod recreated under its name.
    pub uid: Option<String>,
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub since: Option<LogSince>,
    pub previous: bool,
}

impl LogRequest {
    /// kube's parameters. Timestamps are always requested, so the UI can show or hide them
    /// without reloading and a reconnect can resume where the stream broke.
    pub fn params(&self) -> LogParams {
        LogParams {
            container: self.target.container.clone(),
            follow: self.follow,
            previous: self.previous,
            tail_lines: self.tail_lines,
            since_seconds: match self.since {
                Some(LogSince::Seconds(s)) => Some(s),
                _ => None,
            },
            since_time: match self.since {
                Some(LogSince::Time(t)) => Some(t),
                _ => None,
            },
            timestamps: true,
            ..LogParams::default()
        }
    }
}

/// One line as read, before parsing: lossily decoded, without its newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawLine {
    pub text: String,
    /// The line was longer than [`MAX_LINE_BYTES`] and was cut.
    pub truncated: bool,
}

pub(crate) type RawLines = BoxStream<'static, Result<RawLine, LogError>>;

/// A failure to read logs. The message is safe to show: kube errors go through
/// [`ClusterError::kube`] and [`crate::cluster::error_chain`], which summarise exec auth plugin errors instead of
/// quoting their command, environment, or output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogError {
    pub message: String,
    pub kind: ErrorKind,
}

/// What a [`LogError`] means for the stream and the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// Worth retrying after a backoff.
    Retry,
    /// Retrying cannot help (the pod or container does not exist, access is denied): the stream
    /// ends.
    Fatal,
    /// About the cluster, not one stream (it left the kubeconfig files, or the context now
    /// points at another API server): the whole session ends, a workload's included.
    Cluster,
    /// The pod is gone: deleted, or replaced by another pod of the same name.
    PodGone,
    /// A 404: the pod (or its log) does not exist. Retrying cannot help, as for
    /// [`ErrorKind::Fatal`]; a stream that already read the pod, or saw it being deleted, takes
    /// it as the pod's deletion.
    NotFound,
}

impl LogError {
    pub fn retry(message: impl Into<String>) -> Self {
        LogError {
            message: message.into(),
            kind: ErrorKind::Retry,
        }
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        LogError {
            message: message.into(),
            kind: ErrorKind::Fatal,
        }
    }

    /// See [`ErrorKind::PodGone`].
    pub fn pod_gone(message: impl Into<String>) -> Self {
        LogError {
            message: message.into(),
            kind: ErrorKind::PodGone,
        }
    }

    /// See [`ErrorKind::Cluster`].
    pub fn cluster(message: impl Into<String>) -> Self {
        LogError {
            message: message.into(),
            kind: ErrorKind::Cluster,
        }
    }

    /// Retrying cannot help (a [`ErrorKind::Fatal`] or [`ErrorKind::Cluster`] error).
    pub fn is_fatal(&self) -> bool {
        self.kind != ErrorKind::Retry
    }

    /// See [`ErrorKind::Cluster`].
    pub fn is_cluster(&self) -> bool {
        self.kind == ErrorKind::Cluster
    }

    pub fn from_kube(err: kube::Error) -> Self {
        let not_found =
            ApiError::from_kube(&err).is_some_and(|api| api.kind() == ApiErrorKind::NotFound);
        let fatal = ApiError::from_kube(&err).is_some_and(|api| match api.kind() {
            ApiErrorKind::Forbidden | ApiErrorKind::NotFound => true,
            // A container that is still starting is worth waiting for; a missing container
            // or previous instance is not.
            ApiErrorKind::BadRequest => !is_starting(api.message),
            _ => false,
        });
        let message = ClusterError::kube(err).summary().to_string();
        if not_found {
            LogError {
                message,
                kind: ErrorKind::NotFound,
            }
        } else if fatal {
            LogError::fatal(message)
        } else {
            LogError::retry(message)
        }
    }
}

/// Whether a 400 from the log endpoint says the container has not started yet.
fn is_starting(message: &str) -> bool {
    [
        "is waiting to start",
        "ContainerCreating",
        "PodInitializing",
    ]
    .iter()
    .any(|m| message.contains(m))
}

/// What to do after a followed stream ended cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterEnd {
    /// The container may run again (it restarts, or the pod is still running): reconnect.
    Restarts,
    /// As [`AfterEnd::Restarts`], but the pod's status says the container is waiting in
    /// `CrashLoopBackOff`: the kubelet restarts it after its back-off, which can take minutes.
    BackOff,
    /// The container finished for good.
    Finished,
    /// The pod is being deleted (it has a `deletionTimestamp`) but the container has not
    /// stopped: it may still log its shutdown (or never started). Reconnect; the pod may go
    /// before the container shows as stopped.
    Terminating,
    /// The pod is being deleted (it has a `deletionTimestamp`) and the container stopped (or
    /// the pod reached Succeeded or Failed): it will not run again, though the pod and its
    /// logs may still be there.
    Deleting,
    /// The pod was deleted.
    Gone,
}

pub(crate) trait LogSource: Send + Sync + 'static {
    /// Opens a log stream.
    fn open(&self, request: &LogRequest) -> BoxFuture<'static, Result<RawLines, LogError>>;

    /// Decides whether a followed stream that ended should be reopened. With `uid`, a pod of
    /// that name but another uid (recreated under the same name) counts as gone.
    fn after_end(
        &self,
        target: &LogTarget,
        uid: Option<&str>,
    ) -> BoxFuture<'static, Result<AfterEnd, LogError>>;
}

/// Hands out the cluster's current client at every (re)connect, so a session uses a fresh client
/// after the cluster reconnected, and learns when the cluster is gone.
pub(crate) type ClientLookup =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Client, LogError>> + Send + Sync>;

/// The API server.
pub(crate) struct KubeLogSource {
    client: ClientLookup,
}

impl KubeLogSource {
    pub fn new(client: ClientLookup) -> Self {
        KubeLogSource { client }
    }
}

impl LogSource for KubeLogSource {
    fn open(&self, request: &LogRequest) -> BoxFuture<'static, Result<RawLines, LogError>> {
        let client = self.client.clone();
        let request = request.clone();
        async move {
            let client = client().await?;
            let api: Api<Pod> = Api::namespaced(client, &request.target.namespace);
            if let Some(uid) = &request.uid {
                same_pod(api.get_opt(&request.target.pod).await, uid)?;
            }
            let reader = api
                .log_stream(&request.target.pod, &request.params())
                .await
                .map_err(LogError::from_kube)?;
            Ok(split_lines(reader))
        }
        .boxed()
    }

    fn after_end(
        &self,
        target: &LogTarget,
        uid: Option<&str>,
    ) -> BoxFuture<'static, Result<AfterEnd, LogError>> {
        let client = self.client.clone();
        let target = target.clone();
        let uid = uid.map(str::to_owned);
        async move {
            let client = client().await?;
            let api: Api<Pod> = Api::namespaced(client, &target.namespace);
            let pod = api.get_opt(&target.pod).await;
            fate_of(pod, target.container.as_deref(), uid.as_deref())
        }
        .boxed()
    }
}

/// Whether the pod found by name is still the pod with `uid`, checked before its log is opened
/// (the log endpoint takes only a name). A 403 means the user may read logs but not get pods:
/// the check is skipped. The pod may still be replaced between this check and the open; that
/// window is one round trip, against a reconnect that would otherwise read the new pod for good.
pub(crate) fn same_pod(
    lookup: Result<Option<Pod>, kube::Error>,
    uid: &str,
) -> Result<(), LogError> {
    match lookup {
        Ok(Some(pod)) if pod.metadata.uid.as_deref() == Some(uid) => Ok(()),
        Ok(Some(_)) => Err(LogError::pod_gone(
            "the pod was replaced by one of the same name",
        )),
        Ok(None) => Err(LogError::pod_gone("the pod was deleted")),
        Err(kube::Error::Api(status))
            if ApiErrorKind::from_code(status.code) == ApiErrorKind::Forbidden =>
        {
            Ok(())
        }
        Err(e) => Err(LogError::from_kube(e)),
    }
}

/// [`fate`] from the pod lookup. A user may read `pods/log` without being allowed to get the
/// pod: a 403 then means "unknown", and the stream is reopened with backoff. With `uid`, a pod
/// of another uid is another pod (recreated under the same name), so the stream's pod is gone.
pub(crate) fn fate_of(
    lookup: Result<Option<Pod>, kube::Error>,
    container: Option<&str>,
    uid: Option<&str>,
) -> Result<AfterEnd, LogError> {
    match lookup {
        Ok(pod) => {
            let pod = pod.filter(|p| uid.is_none_or(|uid| p.metadata.uid.as_deref() == Some(uid)));
            Ok(fate(pod.as_ref(), container))
        }
        Err(kube::Error::Api(status))
            if ApiErrorKind::from_code(status.code) == ApiErrorKind::Forbidden =>
        {
            Ok(AfterEnd::Restarts)
        }
        Err(e) => Err(LogError::from_kube(e)),
    }
}

/// Whether the container of `pod` (the first one when `container` is `None`) can log again.
pub(crate) fn fate(pod: Option<&Pod>, container: Option<&str>) -> AfterEnd {
    let Some(pod) = pod else {
        return AfterEnd::Gone;
    };
    // A pod being deleted (it has a `deletionTimestamp`) never runs its containers again, but
    // one may still be shutting down and logging: until it stops, the stream is reopened.
    // The kubelet ends the stream before the pod's status shows the container stopped, so
    // `Terminating` may well be the last look before the pod goes.
    let deleting = pod.metadata.deletion_timestamp.is_some();
    let status = pod.status.as_ref();
    if matches!(
        status.and_then(|s| s.phase.as_deref()),
        Some("Succeeded" | "Failed")
    ) {
        return if deleting {
            AfterEnd::Deleting
        } else {
            AfterEnd::Finished
        };
    }
    let spec = pod.spec.as_ref();
    let name = container.map(str::to_owned).or_else(|| {
        spec.and_then(|s| s.containers.first())
            .map(|c| c.name.clone())
    });
    let pod_policy = spec
        .and_then(|s| s.restart_policy.as_deref())
        .unwrap_or("Always");
    // The container's own policy wins: a native sidecar (an init container with
    // `restartPolicy: Always`) restarts in any pod; an ordinary init container runs to success
    // once. Regular containers follow the pod.
    let regular = spec
        .into_iter()
        .flat_map(|s| s.containers.iter())
        .find(|c| Some(&c.name) == name.as_ref());
    let init = spec
        .into_iter()
        .flat_map(|s| s.init_containers.iter().flatten())
        .find(|c| Some(&c.name) == name.as_ref());
    let policy = match (regular, init) {
        (Some(c), _) => c.restart_policy.as_deref().unwrap_or(pod_policy),
        (None, Some(c)) => c.restart_policy.as_deref().unwrap_or("Once"),
        (None, None) => pod_policy,
    };
    let container_status = status
        .into_iter()
        .flat_map(|s| {
            s.container_statuses
                .iter()
                .flatten()
                .chain(s.init_container_statuses.iter().flatten())
        })
        .find(|c| Some(&c.name) == name.as_ref());
    let exit_code = container_status
        .and_then(|c| c.state.as_ref()?.terminated.as_ref())
        .map(|t| t.exit_code);
    let waiting_reason =
        container_status.and_then(|c| c.state.as_ref()?.waiting.as_ref()?.reason.as_deref());
    if deleting {
        return if exit_code.is_some() {
            AfterEnd::Deleting
        } else {
            AfterEnd::Terminating
        };
    }
    match (exit_code, policy) {
        (Some(_), "Never") | (Some(0), "OnFailure" | "Once") => AfterEnd::Finished,
        // A failed init container is retried unless the pod never restarts anything.
        (Some(_), "Once") if pod_policy == "Never" => AfterEnd::Finished,
        _ if waiting_reason == Some(CRASH_LOOP) => AfterEnd::BackOff,
        _ => AfterEnd::Restarts,
    }
}

/// The waiting reason of a container the kubelet restarts after a back-off.
pub(crate) const CRASH_LOOP: &str = "CrashLoopBackOff";

/// The line the kubelet sends as the whole log when it cannot read a container's log file (the
/// previous instance's, early in a restart): `unable to retrieve container logs for
/// containerd://…`. A real log line always starts with a timestamp (Flint asks for them), so
/// this one, without, is the kubelet speaking.
pub(crate) fn is_unavailable_notice(line: &str) -> bool {
    line.starts_with("unable to retrieve container logs for ")
}

/// Splits a byte stream into lines. Invalid UTF-8 is replaced rather than ending the stream, and
/// lines longer than [`MAX_LINE_BYTES`] are cut (the rest is read and dropped).
pub(crate) fn split_lines(reader: impl AsyncBufRead + Send + 'static) -> RawLines {
    let state = (Box::pin(reader), false);
    futures::stream::unfold(state, |(mut reader, done)| async move {
        if done {
            return None;
        }
        let mut line = Vec::new();
        let mut truncated = false;
        loop {
            let buf = match reader.fill_buf().await {
                Ok(buf) => buf,
                Err(e) => {
                    // The source does not know the server: "the connection to the API server
                    // dropped".
                    let error = LogError::retry(
                        ClusterError::stream("log stream", &e, &Target::default())
                            .summary()
                            .to_string(),
                    );
                    return Some((Err(error), (reader, true)));
                }
            };
            if buf.is_empty() {
                if line.is_empty() && !truncated {
                    return None;
                }
                let raw = RawLine {
                    text: String::from_utf8_lossy(&line).into_owned(),
                    truncated,
                };
                return Some((Ok(raw), (reader, true)));
            }
            let newline = buf.iter().position(|b| *b == b'\n');
            let content = newline.unwrap_or(buf.len());
            let room = MAX_LINE_BYTES.saturating_sub(line.len());
            truncated |= content > room;
            line.extend_from_slice(&buf[..content.min(room)]);
            reader.consume_unpin(newline.map_or(content, |at| at + 1));
            if newline.is_some() {
                break;
            }
        }
        let raw = RawLine {
            text: String::from_utf8_lossy(&line).into_owned(),
            truncated,
        };
        Some((Ok(raw), (reader, false)))
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::jiff::Timestamp;
    use serde_json::json;

    fn target() -> LogTarget {
        LogTarget {
            namespace: "ns".into(),
            pod: "p".into(),
            container: Some("c".into()),
        }
    }

    async fn collect(bytes: &[u8]) -> Vec<RawLine> {
        let reader =
            futures::io::BufReader::with_capacity(7, futures::io::Cursor::new(bytes.to_vec()));
        split_lines(reader)
            .map(|l| l.unwrap())
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn splits_lines_across_reads_and_keeps_a_last_line_without_newline() {
        let lines = collect(b"first line\nsecond\n\nlast").await;
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["first line", "second", "", "last"]);
        assert!(lines.iter().all(|l| !l.truncated));
        assert!(collect(b"").await.is_empty());
        assert_eq!(collect(b"a\n").await.len(), 1);
        // Invalid UTF-8 does not end the stream.
        let lossy = collect(b"bad \xff byte\nnext\n").await;
        assert_eq!(lossy[0].text, "bad \u{fffd} byte");
        assert_eq!(lossy[1].text, "next");
    }

    #[tokio::test]
    async fn cuts_runaway_lines() {
        let mut bytes = vec![b'x'; MAX_LINE_BYTES + 100];
        bytes.extend_from_slice(b"\nok\n");
        let lines = collect(&bytes).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text.len(), MAX_LINE_BYTES);
        assert!(lines[0].truncated);
        assert_eq!(lines[1].text, "ok");
    }

    // covers UP-40
    #[test]
    fn params_always_ask_for_timestamps() {
        let since: Timestamp = "2024-05-01T12:00:00Z".parse().unwrap();
        let request = LogRequest {
            target: target(),
            uid: None,
            follow: true,
            tail_lines: Some(10),
            since: Some(LogSince::Time(since)),
            previous: true,
        };
        let params = request.params();
        assert!(params.timestamps && params.follow && params.previous);
        assert_eq!(params.container.as_deref(), Some("c"));
        assert_eq!(params.tail_lines, Some(10));
        assert_eq!(params.since_time, Some(since));
        assert_eq!(params.since_seconds, None);
        let seconds = LogRequest {
            since: Some(LogSince::Seconds(60)),
            ..request
        };
        assert_eq!(seconds.params().since_seconds, Some(60));
        assert_eq!(seconds.params().since_time, None);
    }

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(Box::new(kube::core::Status {
            code,
            reason: "BadRequest".into(),
            message: message.into(),
            ..Default::default()
        }))
    }

    // covers UP-40
    #[test]
    fn errors_say_whether_retrying_helps() {
        let starting = LogError::from_kube(api_error(
            400,
            "container \"c\" in pod \"p\" is waiting to start: ContainerCreating",
        ));
        assert!(!starting.is_fatal());
        let missing = LogError::from_kube(api_error(400, "container x is not valid for pod p"));
        assert!(missing.is_fatal());
        assert!(missing.message.contains("not valid"), "{}", missing.message);
        let not_found = LogError::from_kube(api_error(404, "pods \"p\" not found"));
        assert!(not_found.is_fatal());
        assert_eq!(not_found.kind, ErrorKind::NotFound);
        assert_eq!(missing.kind, ErrorKind::Fatal);
        assert!(!LogError::from_kube(api_error(500, "boom")).is_fatal());
    }

    // covers UP-40
    #[test]
    fn exec_plugin_errors_never_reach_the_message() {
        use std::os::unix::process::ExitStatusExt;
        let err = kube::Error::Auth(kube::client::AuthError::AuthExecRun {
            cmd: "aws eks get-token AWS_SECRET_ACCESS_KEY=s3cr3t".into(),
            out: Box::new(std::process::Output {
                status: std::process::ExitStatus::from_raw(1 << 8),
                stdout: b"token-abc".to_vec(),
                stderr: b"s3cr3t".to_vec(),
            }),
        });
        let error = LogError::from_kube(err);
        assert!(!error.is_fatal());
        assert!(error.message.starts_with("authentication failed"));
        assert!(!error.message.contains("s3cr3t") && !error.message.contains("token-abc"));
    }

    fn pod(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).unwrap()
    }

    // covers UP-40
    #[test]
    fn a_pod_being_deleted_is_deleting_once_its_container_stopped() {
        for phase in ["Succeeded", "Failed"] {
            let done = pod(json!({
                "metadata": {"deletionTimestamp": "2024-05-01T12:00:00Z"},
                "spec": {"containers": [{"name": "app"}]},
                "status": {"phase": phase},
            }));
            assert_eq!(fate(Some(&done), None), AfterEnd::Deleting, "{phase}");
        }
        let terminating = |state: serde_json::Value| {
            pod(json!({
                "metadata": {"deletionTimestamp": "2024-05-01T12:00:00Z"},
                "spec": {"restartPolicy": "Always", "containers": [{"name": "app"}]},
                "status": {"phase": "Running", "containerStatuses": [
                    {"name": "app", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
                     "state": state}]},
            }))
        };
        // Still shutting down (and maybe logging), or never started: the stream is reopened,
        // not ended.
        let running = terminating(json!({"running": {}}));
        assert_eq!(fate(Some(&running), Some("app")), AfterEnd::Terminating);
        let waiting = terminating(json!({"waiting": {"reason": "ContainerCreating"}}));
        assert_eq!(fate(Some(&waiting), Some("app")), AfterEnd::Terminating);
        // Stopped: the pod is going and the container will not run again.
        let stopped = terminating(json!({"terminated": {"exitCode": 0}}));
        assert_eq!(fate(Some(&stopped), Some("app")), AfterEnd::Deleting);
        assert_eq!(fate(Some(&stopped), None), AfterEnd::Deleting);
    }

    // covers UP-40
    #[test]
    fn a_finished_container_is_not_reconnected() {
        assert_eq!(fate(None, None), AfterEnd::Gone);
        let done = pod(json!({"status": {"phase": "Succeeded"}}));
        assert_eq!(fate(Some(&done), None), AfterEnd::Finished);

        let terminated = |policy: &str, code: i32| {
            pod(json!({
                "spec": {"restartPolicy": policy, "containers": [{"name": "app"}, {"name": "side"}]},
                "status": {"phase": "Running", "containerStatuses": [
                    {"name": "app", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
                     "state": {"terminated": {"exitCode": code}}},
                    {"name": "side", "image": "i", "imageID": "", "ready": true, "restartCount": 0,
                     "state": {"running": {}}}
                ]}
            }))
        };
        assert_eq!(
            fate(Some(&terminated("Never", 1)), None),
            AfterEnd::Finished
        );
        assert_eq!(
            fate(Some(&terminated("OnFailure", 0)), Some("app")),
            AfterEnd::Finished
        );
        assert_eq!(
            fate(Some(&terminated("OnFailure", 1)), Some("app")),
            AfterEnd::Restarts
        );
        assert_eq!(
            fate(Some(&terminated("Always", 0)), Some("app")),
            AfterEnd::Restarts
        );
        assert_eq!(
            fate(Some(&terminated("Never", 0)), Some("side")),
            AfterEnd::Restarts,
            "the sidecar still runs"
        );
    }

    // covers UP-40: a crash-looping container is waited for as the kubelet's back-off, not as a
    // broken connection; the kubelet's "unable to retrieve" answer is told apart from a line.
    #[test]
    fn a_crash_looping_container_is_in_back_off() {
        let waiting = |reason: &str| {
            pod(json!({
                "spec": {"restartPolicy": "Always", "containers": [{"name": "app"}]},
                "status": {"phase": "Running", "containerStatuses": [
                    {"name": "app", "image": "i", "imageID": "", "ready": false, "restartCount": 4,
                     "state": {"waiting": {"reason": reason}},
                     "lastState": {"terminated": {"exitCode": 1}}}]},
            }))
        };
        assert_eq!(
            fate(Some(&waiting("CrashLoopBackOff")), Some("app")),
            AfterEnd::BackOff
        );
        assert_eq!(
            fate(Some(&waiting("CrashLoopBackOff")), None),
            AfterEnd::BackOff
        );
        assert_eq!(
            fate(Some(&waiting("ContainerCreating")), Some("app")),
            AfterEnd::Restarts
        );
        assert!(is_unavailable_notice(
            "unable to retrieve container logs for containerd://c54f232e5b"
        ));
        assert!(!is_unavailable_notice(
            "2024-05-01T12:00:00Z unable to retrieve container logs for x"
        ));
    }

    fn with_init(pod_policy: &str, sidecar: bool, code: i32) -> Pod {
        let mut init = json!({"name": "init"});
        if sidecar {
            init["restartPolicy"] = json!("Always");
        }
        pod(json!({
            "spec": {"restartPolicy": pod_policy, "initContainers": [init],
                     "containers": [{"name": "app"}]},
            "status": {"phase": "Running", "initContainerStatuses": [
                {"name": "init", "image": "i", "imageID": "", "ready": false, "restartCount": 0,
                 "state": {"terminated": {"exitCode": code}}}
            ]}
        }))
    }

    // covers UP-40
    #[test]
    fn the_containers_own_restart_policy_decides() {
        let init = Some("init");
        // A native sidecar restarts even in a pod that restarts nothing else.
        assert_eq!(
            fate(Some(&with_init("Never", true, 1)), init),
            AfterEnd::Restarts
        );
        assert_eq!(
            fate(Some(&with_init("OnFailure", true, 0)), init),
            AfterEnd::Restarts
        );
        // An ordinary init container that succeeded is done, even in an Always pod.
        assert_eq!(
            fate(Some(&with_init("Always", false, 0)), init),
            AfterEnd::Finished
        );
        // A failed one is retried, unless the pod never restarts.
        assert_eq!(
            fate(Some(&with_init("Always", false, 1)), init),
            AfterEnd::Restarts
        );
        assert_eq!(
            fate(Some(&with_init("Never", false, 1)), init),
            AfterEnd::Finished
        );
    }

    // covers UP-40
    #[test]
    fn a_forbidden_pod_lookup_falls_back_to_reconnecting() {
        let forbidden = Err(api_error(403, "pods \"p\" is forbidden"));
        assert_eq!(fate_of(forbidden, None, None), Ok(AfterEnd::Restarts));
        assert_eq!(fate_of(Ok(None), None, None), Ok(AfterEnd::Gone));
        // Before opening a pod set's stream, the pod by that name must still be the same pod.
        let named = |uid: &str| Ok(Some(pod(json!({"metadata": {"uid": uid}}))));
        assert_eq!(same_pod(named("u1"), "u1"), Ok(()));
        let replaced = same_pod(named("u2"), "u1").unwrap_err();
        assert_eq!(replaced.kind, ErrorKind::PodGone);
        assert!(replaced.is_fatal());
        assert_eq!(
            same_pod(Ok(None), "u1").unwrap_err().kind,
            ErrorKind::PodGone
        );
        assert_eq!(same_pod(Err(api_error(403, "forbidden")), "u1"), Ok(()));
        assert!(
            !same_pod(Err(api_error(500, "boom")), "u1")
                .unwrap_err()
                .is_fatal()
        );
        // A pod recreated under the same name is another pod.
        let running = || Ok(Some(pod(json!({"metadata": {"uid": "new"}}))));
        assert_eq!(fate_of(running(), None, Some("old")), Ok(AfterEnd::Gone));
        assert_eq!(
            fate_of(running(), None, Some("new")),
            Ok(AfterEnd::Restarts)
        );
        assert_eq!(fate_of(running(), None, None), Ok(AfterEnd::Restarts));
        let missing = fate_of(Err(api_error(500, "boom")), None, None).unwrap_err();
        assert!(!missing.is_fatal());
    }
}
