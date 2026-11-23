//! The fake API server's `pods/portforward`: a WebSocket on the `v4.channel.k8s.io` protocol whose
//! one port echoes what it is sent, so a test can see bytes cross a real forward.
// module-tests: ../../tests/port_forward.rs

use std::sync::Arc;

use futures::{SinkExt as _, StreamExt as _};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::super::state::ApiError;
use super::{Body, FakeApiServer, Gvr, Inner, Query, RequestRecord, Route, error, lock};

const PROTOCOL: &str = "v4.channel.k8s.io";

impl FakeApiServer {
    /// Every port-forward request so far, oldest first, refused or not.
    pub fn forward_requests(&self) -> Vec<RequestRecord> {
        self.requests()
            .into_iter()
            .filter(|r| Route::parse(&r.path).subresource() == Some("portforward"))
            .collect()
    }
}

/// The one port asked for, from the raw query: `ports=8080`.
fn port_of(query: Option<&str>) -> Option<u16> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("ports="))?
        .parse()
        .ok()
}

pub(super) fn open_forward(
    inner: &Arc<Inner>,
    req: Request<Incoming>,
    upgrade: bool,
    namespace: &str,
    pod: &str,
    _query: &Query,
) -> Response<Body> {
    if let Err(e) = lock(&inner.state).object(&Gvr::core("pods"), Some(namespace), pod) {
        return error(&e);
    }
    if !upgrade {
        return error(&ApiError::new(
            400,
            "BadRequest",
            "Upgrade request required",
        ));
    }
    let Some(port) = port_of(req.uri().query()) else {
        return error(&ApiError::new(400, "BadRequest", "one port is required"));
    };
    let key = req
        .headers()
        .get(http::header::SEC_WEBSOCKET_KEY)
        .map(|k| k.as_bytes().to_vec())
        .unwrap_or_default();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(&key);
    let mut shutdown = inner.shutdown.clone();
    tokio::spawn(async move {
        let Ok(upgraded) = hyper::upgrade::on(req).await else {
            return;
        };
        let ws = WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, None).await;
        tokio::select! {
            () = echo(ws, port) => {}
            _ = shutdown.changed() => {}
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::CONNECTION, "Upgrade")
        .header(http::header::SEC_WEBSOCKET_ACCEPT, accept)
        .header(http::header::SEC_WEBSOCKET_PROTOCOL, PROTOCOL)
        .body(Empty::new().map_err(|never| match never {}).boxed_unsync())
        .expect("a valid response")
}

/// A 503 whose body goes on until the client hangs up.
pub(super) fn endless_error() -> Response<Body> {
    let chunks = futures::stream::unfold((), |()| async {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let chunk = hyper::body::Frame::data(bytes::Bytes::from_static(&[b'x'; 4096]));
        Some((Ok::<_, std::io::Error>(chunk), ()))
    });
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .body(http_body_util::StreamBody::new(Box::pin(chunks)).boxed_unsync())
        .expect("a valid response")
}

/// Names the port on its data and error channels, as the API server does, then echoes data.
async fn echo(mut ws: WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>, port: u16) {
    let [low, high] = port.to_le_bytes();
    for channel in [0u8, 1] {
        let opening = vec![channel, low, high];
        if ws.send(Message::binary(opening)).await.is_err() {
            return;
        }
    }
    while let Some(Ok(message)) = ws.next().await {
        match message {
            Message::Binary(data) if data.first() == Some(&0) => {
                if ws.send(Message::binary(data)).await.is_err() {
                    return;
                }
            }
            Message::Close(_) => return,
            _ => {}
        }
    }
}
