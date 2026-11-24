//! The API server's side of a forward: the lookups ahead of a connection and the `portforward`
//! WebSocket, each with a limit on what a server can make Flint hold in memory.

use std::fmt::Debug;

use futures::{SinkExt as _, StreamExt as _};
use http::header::{
    CONNECTION, HeaderValue, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL,
    SEC_WEBSOCKET_VERSION, UPGRADE,
};
use http_body_util::{BodyExt as _, Limited};
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::Pod;
use kube::Resource;
use kube::api::{GetParams, ListParams};
use kube::core::{ObjectList, Request};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream};
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use super::MAX_RELAYS;

const PROTOCOL: &str = "v4.channel.k8s.io";
/// The largest WebSocket frame or message taken from a server. Servers send far smaller ones.
const MAX_MESSAGE: usize = 256 * 1024;
const READ_BUFFER: usize = 64 * 1024;
const WRITE_BUFFER: usize = 64 * 1024;
const MAX_WRITE_BUFFER: usize = 128 * 1024;
/// What is buffered each way between the WebSocket and the local connection.
const PIPE: usize = 64 * 1024;
/// The most read from the local connection into one message.
const CHUNK: usize = 32 * 1024;
/// The most one relay can have queued: a message being assembled, and its next frame unread.
const QUEUED_PER_RELAY: usize = 2 * MAX_MESSAGE + READ_BUFFER + MAX_WRITE_BUFFER + 2 * PIPE + CHUNK;
/// How long the pod's side may keep answering after the local side has stopped sending.
const HALF_CLOSE_WAIT: std::time::Duration = std::time::Duration::from_secs(15);
/// The most all relays together can have queued.
const QUEUED_BYTES: usize = 128 * 1024 * 1024;
const _: () = assert!(MAX_RELAYS * QUEUED_PER_RELAY <= QUEUED_BYTES);

/// The largest response to a lookup.
const MAX_LOOKUP_BODY: usize = 4 * 1024 * 1024;
/// How many lookup bodies are read at once, across all forwards.
static LOOKUPS: Semaphore = Semaphore::const_new(16);

/// Why a lookup or an upgrade got no usable answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WireError {
    /// The server answered with this status.
    Status(u16),
    /// No answer that could be used.
    Failed,
}

/// Reads one JSON response, up to [`MAX_LOOKUP_BODY`]. A refusal's body is not read.
async fn read_json<T: DeserializeOwned>(
    client: &kube::Client,
    request: http::Request<Vec<u8>>,
) -> Result<T, WireError> {
    let response = client
        .send(request.map(kube::client::Body::from))
        .await
        .map_err(|_| WireError::Failed)?;
    let status = response.status();
    if !status.is_success() {
        return Err(WireError::Status(status.as_u16()));
    }
    let _turn = LOOKUPS.acquire().await.map_err(|_| WireError::Failed)?;
    let body = Limited::new(response.into_body(), MAX_LOOKUP_BODY)
        .collect()
        .await
        .map_err(|_| WireError::Failed)?
        .to_bytes();
    serde_json::from_slice(&body).map_err(|_| WireError::Failed)
}

fn namespaced<T: Resource<DynamicType = ()>>(namespace: &str) -> Request {
    Request::new(T::url_path(&(), Some(namespace)))
}

/// One object of a namespace.
pub(super) async fn get<T>(
    client: &kube::Client,
    namespace: &str,
    name: &str,
) -> Result<T, WireError>
where
    T: Resource<DynamicType = ()> + DeserializeOwned,
{
    let request = namespaced::<T>(namespace)
        .get(name, &GetParams::default())
        .map_err(|_| WireError::Failed)?;
    read_json(client, request).await
}

/// The objects of a namespace that `params` selects.
pub(super) async fn list<T>(
    client: &kube::Client,
    namespace: &str,
    params: &ListParams,
) -> Result<Vec<T>, WireError>
where
    T: Resource<DynamicType = ()> + DeserializeOwned + Clone + Debug,
{
    let request = namespaced::<T>(namespace)
        .list(params)
        .map_err(|_| WireError::Failed)?;
    let list: ObjectList<T> = read_json(client, request).await?;
    Ok(list.items)
}

/// Ends the task carrying a connection when dropped.
pub(super) struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// An open channel to one port of a pod.
pub(super) struct Tunnel {
    pub(super) stream: DuplexStream,
    /// Resolves with `Ok` if the server reported an error or broke the protocol.
    pub(super) failed: oneshot::Receiver<()>,
    pub(super) task: AbortOnDrop,
}

fn config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(READ_BUFFER)
        .write_buffer_size(WRITE_BUFFER)
        .max_write_buffer_size(MAX_WRITE_BUFFER)
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE))
}

fn says(
    response: &http::Response<kube::client::Body>,
    name: http::HeaderName,
    value: &str,
) -> bool {
    response
        .headers()
        .get(name)
        .and_then(|found| found.to_str().ok())
        .is_some_and(|found| found.eq_ignore_ascii_case(value))
}

/// Opens `pod`'s `port`, sending the upgrade once through `client`.
pub(super) async fn open(
    client: &kube::Client,
    namespace: &str,
    pod: &str,
    port: u16,
) -> Result<Tunnel, WireError> {
    let request = namespaced::<Pod>(namespace)
        .portforward(pod, &[port])
        .map_err(|_| WireError::Failed)?;
    let (mut parts, body) = request.into_parts();
    let key = generate_key();
    let headers = &mut parts.headers;
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(SEC_WEBSOCKET_VERSION, HeaderValue::from_static("13"));
    headers.insert(SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(PROTOCOL));
    headers.insert(
        SEC_WEBSOCKET_KEY,
        HeaderValue::from_str(&key).map_err(|_| WireError::Failed)?,
    );
    let request = http::Request::from_parts(parts, kube::client::Body::from(body));
    let response = client.send(request).await.map_err(|_| WireError::Failed)?;
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(WireError::Status(response.status().as_u16()));
    }
    let accepted = says(&response, UPGRADE, "websocket")
        && says(&response, CONNECTION, "upgrade")
        && says(&response, SEC_WEBSOCKET_PROTOCOL, PROTOCOL)
        && response
            .headers()
            .get(SEC_WEBSOCKET_ACCEPT)
            .map(HeaderValue::as_bytes)
            == Some(derive_accept_key(key.as_bytes()).as_bytes());
    if !accepted {
        return Err(WireError::Failed);
    }
    let upgraded = hyper::upgrade::on(response)
        .await
        .map_err(|_| WireError::Failed)?;
    let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Client, Some(config()))
        .await;
    Ok(carry(ws, port))
}

fn carry<S>(ws: WebSocketStream<S>, port: u16) -> Tunnel
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (stream, pipe) = tokio::io::duplex(PIPE);
    let (failed_tx, failed) = oneshot::channel();
    let task = tokio::spawn(pump(ws, pipe, port, failed_tx));
    Tunnel {
        stream,
        failed,
        task: AbortOnDrop(task),
    }
}

/// Moves bytes between the WebSocket and `pipe` until the server ends it, or the local side has
/// closed and the server has answered or [`HALF_CLOSE_WAIT`] has passed. A local half-close keeps
/// the pod's direction open, since a WebSocket close would cut off a reply still in flight. Each
/// direction waits for its reader, so nothing queues.
async fn pump<S>(ws: WebSocketStream<S>, pipe: DuplexStream, port: u16, failed: oneshot::Sender<()>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut sink, mut frames) = ws.split();
    let (mut from_local, mut to_local) = tokio::io::split(pipe);
    let from_pod = async move {
        // Each channel opens with a frame naming the port.
        let mut opened = [false; 2];
        let mut broken = false;
        while let Some(message) = frames.next().await {
            match message {
                Ok(Message::Binary(frame)) if frame.len() > 1 => {
                    let (channel, body) = (usize::from(frame[0]), &frame[1..]);
                    let Some(seen) = opened.get_mut(channel) else {
                        broken = true;
                        break;
                    };
                    if !*seen {
                        *seen = true;
                        if body != port.to_le_bytes() {
                            broken = true;
                            break;
                        }
                    } else if channel == 1 {
                        broken = true;
                        break;
                    } else if to_local.write_all(body).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(_) => {
                    broken = true;
                    break;
                }
            }
        }
        if broken {
            let _ = failed.send(());
        }
        let _ = to_local.shutdown().await;
    };
    let to_pod = async move {
        let mut chunk = vec![0; CHUNK + 1];
        while let Ok(read @ 1..) = from_local.read(&mut chunk[1..]).await {
            if sink
                .send(Message::binary(chunk[..=read].to_vec()))
                .await
                .is_err()
            {
                return None;
            }
        }
        Some(sink)
    };
    tokio::pin!(from_pod, to_pod);
    tokio::select! {
        () = &mut from_pod => {}
        sink = &mut to_pod => {
            let Some(mut sink) = sink else { return from_pod.await };
            // The local side is done sending; let the pod finish its reply, then close.
            let _ = tokio::time::timeout(HALF_CLOSE_WAIT, &mut from_pod).await;
            let _ = sink.send(Message::Close(None)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn server_and_tunnel() -> (WebSocketStream<DuplexStream>, Tunnel) {
        let (near, far) = tokio::io::duplex(1024 * 1024);
        let client = WebSocketStream::from_raw_socket(near, Role::Client, Some(config())).await;
        let mut server = WebSocketStream::from_raw_socket(far, Role::Server, None).await;
        for channel in [0u8, 1] {
            let [low, high] = 8080u16.to_le_bytes();
            server
                .send(Message::binary(vec![channel, low, high]))
                .await
                .unwrap();
        }
        (server, carry(client, 8080))
    }

    #[tokio::test]
    async fn a_message_within_the_limit_is_carried() {
        let (mut server, mut tunnel) = server_and_tunnel().await;
        let mut frame = vec![0];
        frame.extend_from_slice(b"pong");
        server.send(Message::binary(frame)).await.unwrap();
        let mut back = [0; 4];
        tunnel.stream.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"pong");
    }

    #[tokio::test]
    async fn a_reply_after_the_local_side_stops_sending_is_carried() {
        let (mut server, mut tunnel) = server_and_tunnel().await;
        tunnel.stream.write_all(b"ping").await.unwrap();
        tunnel.stream.shutdown().await.unwrap();
        let got = server.next().await.unwrap().unwrap();
        assert_eq!(got.into_data().as_ref(), b"\0ping");
        // The server sees no close yet, so it can still answer.
        let waiting = tokio::time::timeout(std::time::Duration::from_millis(200), server.next());
        assert!(
            waiting.await.is_err(),
            "no close is sent while it may answer"
        );
        let mut frame = vec![0];
        frame.extend_from_slice(b"pong");
        server.send(Message::binary(frame)).await.unwrap();
        server.send(Message::Close(None)).await.unwrap();
        let mut back = Vec::new();
        tunnel.stream.read_to_end(&mut back).await.unwrap();
        assert_eq!(back, b"pong");
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_server_is_closed_after_the_wait() {
        let (mut server, mut tunnel) = server_and_tunnel().await;
        tunnel.stream.shutdown().await.unwrap();
        let closed = loop {
            match server.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break true,
                Some(Ok(_)) => {}
            }
        };
        assert!(closed);
    }

    #[tokio::test]
    async fn a_message_over_the_limit_ends_the_connection() {
        let (mut server, mut tunnel) = server_and_tunnel().await;
        let mut frame = vec![0; MAX_MESSAGE + 2];
        frame[0] = 0;
        server.send(Message::binary(frame)).await.unwrap();
        let mut sink = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tunnel.stream.read_to_end(&mut sink),
        )
        .await
        .expect("the connection ends")
        .unwrap();
        assert!(sink.is_empty(), "none of it is carried");
        assert!((&mut tunnel.failed).await.is_ok());
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "a client that reaches no server, for reading a canned body"
    )]
    fn answering(body: Vec<u8>) -> kube::Client {
        let service = tower::service_fn(move |_: http::Request<kube::client::Body>| {
            let body = body.clone();
            async move {
                Ok::<_, std::convert::Infallible>(http::Response::new(kube::client::Body::from(
                    body,
                )))
            }
        });
        kube::Client::new(service, "default")
    }

    #[tokio::test]
    async fn a_lookup_body_over_the_limit_is_refused() {
        let mut body = b"{\"padding\":\"".to_vec();
        body.resize(MAX_LOOKUP_BODY + 1, b'x');
        body.extend_from_slice(b"\"}");
        let read: Result<serde_json::Value, _> =
            read_json(&answering(body), http::Request::new(Vec::new())).await;
        assert_eq!(read.unwrap_err(), WireError::Failed);
    }

    #[tokio::test]
    async fn a_lookup_body_within_the_limit_is_read() {
        let read: serde_json::Value = read_json(
            &answering(b"{\"a\":1}".to_vec()),
            http::Request::new(Vec::new()),
        )
        .await
        .unwrap();
        assert_eq!(read["a"], 1);
    }
}
