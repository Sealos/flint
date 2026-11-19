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
use crate::cluster::{ClusterError, error_chain};

/// One request to the log endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogRequest {
    pub target: LogTarget,
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
/// [`ClusterError::kube`] and [`error_chain`], which summarise exec auth plugin errors instead of
/// quoting their command, environment, or output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogError {
    pub message: String,
    /// Retrying cannot help (the pod or container does not exist, access is denied).
    pub fatal: bool,
}

impl LogError {
    pub fn retry(message: impl Into<String>) -> Self {
        LogError {
            message: message.into(),
            fatal: false,
        }
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        LogError {
            message: message.into(),
            fatal: true,
        }
    }

    pub fn from_kube(err: kube::Error) -> Self {
        let fatal = match &err {
            kube::Error::Api(status) => match status.code {
                403 | 404 => true,
                // A container that is still starting is worth waiting for; a missing container
                // or previous instance is not.
                400 => !is_starting(&status.message),
                _ => false,
            },
            _ => false,
        };
        LogError {
            message: ClusterError::kube(err).to_string(),
            fatal,
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
    /// The container finished for good.
    Finished,
    /// The pod was deleted.
    Gone,
}

pub(crate) trait LogSource: Send + Sync + 'static {
    /// Opens a log stream.
    fn open(&self, request: &LogRequest) -> BoxFuture<'static, Result<RawLines, LogError>>;

    /// Decides whether a followed stream that ended should be reopened.
    fn after_end(&self, target: &LogTarget) -> BoxFuture<'static, Result<AfterEnd, LogError>>;
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
            let reader = api
                .log_stream(&request.target.pod, &request.params())
                .await
                .map_err(LogError::from_kube)?;
            Ok(split_lines(reader))
        }
        .boxed()
    }

    fn after_end(&self, target: &LogTarget) -> BoxFuture<'static, Result<AfterEnd, LogError>> {
        let client = self.client.clone();
        let target = target.clone();
        async move {
            let client = client().await?;
            let api: Api<Pod> = Api::namespaced(client, &target.namespace);
            let pod = api
                .get_opt(&target.pod)
                .await
                .map_err(LogError::from_kube)?;
            Ok(fate(pod.as_ref(), target.container.as_deref()))
        }
        .boxed()
    }
}

/// Whether the container of `pod` (the first one when `container` is `None`) can log again.
pub(crate) fn fate(pod: Option<&Pod>, container: Option<&str>) -> AfterEnd {
    let Some(pod) = pod else {
        return AfterEnd::Gone;
    };
    let status = pod.status.as_ref();
    if matches!(
        status.and_then(|s| s.phase.as_deref()),
        Some("Succeeded" | "Failed")
    ) {
        return AfterEnd::Finished;
    }
    let spec = pod.spec.as_ref();
    let name = container.map(str::to_owned).or_else(|| {
        spec.and_then(|s| s.containers.first())
            .map(|c| c.name.clone())
    });
    let exit_code = status
        .into_iter()
        .flat_map(|s| {
            s.container_statuses
                .iter()
                .flatten()
                .chain(s.init_container_statuses.iter().flatten())
        })
        .find(|c| Some(&c.name) == name.as_ref())
        .and_then(|c| c.state.as_ref()?.terminated.as_ref())
        .map(|t| t.exit_code);
    let policy = spec
        .and_then(|s| s.restart_policy.as_deref())
        .unwrap_or("Always");
    match (exit_code, policy) {
        (Some(_), "Never") | (Some(0), "OnFailure") => AfterEnd::Finished,
        _ => AfterEnd::Restarts,
    }
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
                    let error =
                        LogError::retry(format!("the log stream broke: {}", error_chain(&e)));
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
        assert!(!starting.fatal);
        let missing = LogError::from_kube(api_error(400, "container x is not valid for pod p"));
        assert!(missing.fatal);
        assert!(missing.message.contains("not valid"), "{}", missing.message);
        assert!(LogError::from_kube(api_error(404, "pods \"p\" not found")).fatal);
        assert!(!LogError::from_kube(api_error(500, "boom")).fatal);
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
        assert!(!error.fatal);
        assert!(error.message.starts_with("authentication failed"));
        assert!(!error.message.contains("s3cr3t") && !error.message.contains("token-abc"));
    }

    fn pod(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).unwrap()
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
}
