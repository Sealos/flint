//! Connections through a local fake `CONNECT` proxy, to the fake API server only.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use flint_core::cluster;
use flint_e2e::fake_api::{FakeApiServer, arrivals_at};
use kube::config::Kubeconfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the fake proxy saw.
#[derive(Default)]
struct Seen {
    /// Each `CONNECT` request's head.
    heads: Vec<String>,
    /// The first byte the client sent inside each tunnel.
    first_bytes: Vec<u8>,
}

/// A proxy answering `CONNECT` by tunnelling to `upstream`, wherever the client asked to go.
async fn fake_proxy(upstream: SocketAddr) -> (String, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            tokio::spawn(tunnel(client, upstream, Arc::clone(&log)));
        }
    });
    (format!("{}:{}", addr.ip(), addr.port()), seen)
}

async fn tunnel(mut client: TcpStream, upstream: SocketAddr, seen: Arc<Mutex<Seen>>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if client.read_exact(&mut byte).await.is_err() {
            return;
        }
        head.push(byte[0]);
    }
    seen.lock()
        .unwrap()
        .heads
        .push(String::from_utf8_lossy(&head).into_owned());
    let Ok(mut server) = TcpStream::connect(upstream).await else {
        return;
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
        || client.read_exact(&mut byte).await.is_err()
    {
        return;
    }
    seen.lock().unwrap().first_bytes.push(byte[0]);
    if server.write_all(&byte).await.is_ok() {
        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
    }
}

fn through(mut kubeconfig: Kubeconfig, proxy: &str) -> (Kubeconfig, String) {
    kubeconfig.clusters[0].cluster.as_mut().unwrap().proxy_url = Some(proxy.to_owned());
    let context = kubeconfig.contexts[0].name.clone();
    (kubeconfig, context)
}

fn arrivals(api: &FakeApiServer) -> usize {
    arrivals_at(&api.url()).map_or(0, |records| records.len())
}

#[tokio::test]
async fn a_cluster_is_read_through_the_proxy_with_tls_to_the_server() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_proxy(api.addr()).await;
    let url = format!("http://user:pa%40ss@{proxy}");
    let (kubeconfig, context) = through(api.kubeconfig(None), &url);

    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();

    assert!(arrivals(&api) > 0, "the request reached the server");
    let seen = seen.lock().unwrap();
    let head = &seen.heads[0];
    assert!(
        head.starts_with(&format!("CONNECT {} HTTP/1.1", api.addr())),
        "{head}"
    );
    // base64 of `user:pa@ss`.
    assert!(head.contains("dXNlcjpwYUBzcw=="), "{head}");
    // 0x16 opens a TLS handshake: the proxy carries TLS, not the request.
    assert_eq!(seen.first_bytes[0], 0x16);
}

#[tokio::test]
async fn a_proxy_that_leads_to_another_server_fails_tls() {
    let api = FakeApiServer::start().await.unwrap();
    let impostor = FakeApiServer::start().await.unwrap();
    let (proxy, _) = fake_proxy(impostor.addr()).await;
    let (kubeconfig, context) = through(api.kubeconfig(None), &format!("http://{proxy}"));

    let outcome = match cluster::connect(&kubeconfig, &context).await {
        Ok(client) => cluster::server_version(&client).await.map(|_| ()),
        Err(e) => Err(e),
    };

    assert!(outcome.is_err(), "the impostor's certificate was accepted");
    assert_eq!(arrivals(&impostor), 0, "a request reached the impostor");
    assert_eq!(arrivals(&api), 0);
}

#[tokio::test]
async fn a_proxy_that_is_down_does_not_fall_back_to_a_direct_connection() {
    let api = FakeApiServer::start().await.unwrap();
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let url = format!("http://user:proxy-secret@127.0.0.1:{port}");
    let (kubeconfig, context) = through(api.kubeconfig(None), &url);

    let error = match cluster::connect(&kubeconfig, &context).await {
        Ok(client) => cluster::server_version(&client).await.unwrap_err(),
        Err(e) => e,
    };

    assert_eq!(arrivals(&api), 0, "it connected directly");
    let text = error.to_string();
    assert!(text.contains("could not reach the proxy"), "{text}");
    assert!(!text.contains("proxy-secret"), "{text}");
}

/// A proxy that answers every `CONNECT` with `status`.
async fn answering_proxy(status: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if client.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let reply = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
                let _ = client.write_all(reply.as_bytes()).await;
            });
        }
    });
    addr.to_string()
}

#[tokio::test]
async fn a_proxy_that_refuses_says_so_and_sends_nothing() {
    let api = FakeApiServer::start().await.unwrap();
    for (status, expected) in [
        ("407 Proxy Authentication Required", "asks for credentials"),
        ("403 Forbidden", "refused to open a tunnel"),
    ] {
        let proxy = answering_proxy(status).await;
        let url = format!("http://user:proxy-secret@{proxy}");
        let (kubeconfig, context) = through(api.kubeconfig(None), &url);
        let error = match cluster::connect(&kubeconfig, &context).await {
            Ok(client) => cluster::server_version(&client).await.unwrap_err(),
            Err(e) => e,
        };
        let text = error.to_string();
        assert!(text.contains(expected), "{status}: {text}");
        assert!(!text.contains("proxy-secret"), "{text}");
    }
    assert_eq!(arrivals(&api), 0);
}

#[tokio::test]
async fn an_unsupported_scheme_is_refused_and_sends_nothing() {
    let api = FakeApiServer::start().await.unwrap();
    for url in [
        "socks5://user:proxy-secret@127.0.0.1:1080",
        "https://user:proxy-secret@127.0.0.1:3128",
    ] {
        let (kubeconfig, context) = through(api.kubeconfig(None), url);
        let Err(error) = cluster::connect(&kubeconfig, &context).await else {
            panic!("{url} was accepted");
        };
        let text = error.to_string();
        assert!(text.contains("does not support"), "{text}");
        assert!(!text.contains("proxy-secret"), "{text}");
    }
    assert_eq!(arrivals(&api), 0);
}

#[tokio::test]
async fn nothing_goes_through_a_proxy_to_a_server_without_verified_tls() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_proxy(api.addr()).await;
    let (mut unverified, context) = through(api.kubeconfig(None), &format!("http://{proxy}"));
    let mut plain = unverified.clone();
    unverified.clusters[0]
        .cluster
        .as_mut()
        .unwrap()
        .insecure_skip_tls_verify = Some(true);
    plain.clusters[0].cluster.as_mut().unwrap().server = Some(format!("http://{}", api.addr()));

    for kubeconfig in [unverified, plain] {
        let Err(error) = cluster::connect(&kubeconfig, &context).await else {
            panic!("connected through the proxy");
        };
        assert!(error.to_string().contains("did not connect"), "{error}");
    }

    assert!(seen.lock().unwrap().heads.is_empty(), "the proxy was used");
    assert_eq!(arrivals(&api), 0, "it connected directly");
}

#[tokio::test]
async fn a_server_with_a_bad_port_is_refused_before_the_proxy() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_proxy(api.addr()).await;
    let (mut kubeconfig, context) = through(api.kubeconfig(None), &format!("http://{proxy}"));
    kubeconfig.clusters[0].cluster.as_mut().unwrap().server =
        Some("https://127.0.0.1:65536".to_owned());

    let error = match cluster::connect(&kubeconfig, &context).await {
        Ok(client) => cluster::server_version(&client).await.unwrap_err(),
        Err(error) => error,
    };

    assert!(error.to_string().contains("did not connect"), "{error}");
    assert!(seen.lock().unwrap().heads.is_empty(), "the proxy was used");
    assert_eq!(arrivals(&api), 0);
}

/// Set for the child process of [`an_undecodable_environment_proxy_is_refused`].
const UNDECODABLE_CHILD: &str = "FLINT_TEST_UNDECODABLE_PROXY_CHILD";

// The proxy variables belong to the whole process, so this runs in a child of its own.
#[test]
fn an_undecodable_environment_proxy_is_refused() {
    use std::os::unix::ffi::OsStrExt as _;
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["undecodable_proxy_child", "--exact", "--nocapture"])
        .env(UNDECODABLE_CHILD, "1")
        .env(
            "HTTPS_PROXY",
            std::ffi::OsStr::from_bytes(b"http://proxy\xff:3128"),
        )
        .env_remove("https_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn undecodable_proxy_child() {
    if std::env::var_os(UNDECODABLE_CHILD).is_none() {
        return;
    }
    let api = FakeApiServer::start().await.unwrap();
    let kubeconfig = api.kubeconfig(None);
    let context = kubeconfig.contexts[0].name.clone();
    let Err(error) = cluster::connect(&kubeconfig, &context).await else {
        panic!("connected without the proxy");
    };
    assert!(error.to_string().contains("can't be read"), "{error}");
    assert_eq!(arrivals(&api), 0, "it connected directly");
}

/// Set for the child process of [`an_exempt_server_ignores_a_malformed_environment_proxy`].
const EXEMPT_CHILD: &str = "FLINT_TEST_EXEMPT_PROXY_CHILD";

// The proxy variables belong to the whole process, so this runs in a child of its own.
#[test]
fn an_exempt_server_ignores_a_malformed_environment_proxy() {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["exempt_server_child", "--exact", "--nocapture"])
        .env(EXEMPT_CHILD, "1")
        .env("HTTPS_PROXY", "http://not a url:1")
        .env_remove("https_proxy")
        .env("NO_PROXY", "127.0.0.1")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn exempt_server_child() {
    if std::env::var_os(EXEMPT_CHILD).is_none() {
        return;
    }
    let api = FakeApiServer::start().await.unwrap();
    let kubeconfig = api.kubeconfig(None);
    let context = kubeconfig.contexts[0].name.clone();
    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();
    assert!(arrivals(&api) > 0);
}

#[tokio::test]
async fn a_client_through_a_proxy_keeps_the_read_only_rule() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, _) = fake_proxy(api.addr()).await;
    let (kubeconfig, context) = through(api.kubeconfig(None), &format!("http://{proxy}"));
    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    let before = arrivals(&api);

    let write = http::Request::post("/api/v1/namespaces/default/configmaps")
        .body(b"{}".to_vec())
        .unwrap();
    assert!(client.request_text(write).await.is_err());

    assert_eq!(arrivals(&api), before, "the write reached the server");
}
