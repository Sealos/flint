//! Connections through local fake `CONNECT` and SOCKS5 proxies, to the fake API server only.

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
        "socks4://user:proxy-secret@127.0.0.1:1080",
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

/// Set for the child processes of the proxy setting tests: the setting is used once per process.
const SETTING_CHILD: &str = "FLINT_TEST_PROXY_SETTING_CHILD";

/// Runs the test `child` in a child process whose environment names a proxy where nothing
/// listens, so a connection that reaches the server did not use it.
fn run_setting_child(child: &str) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([child, "--exact", "--nocapture"])
        .env(SETTING_CHILD, "1")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env_remove("https_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env("KUBECONFIG", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success());
}

/// Writes `proxy` as the `[proxy]` of a settings.toml in `dir`, loads it as Flint does at
/// startup, and uses it for this process.
fn use_proxy_setting(dir: &std::path::Path, proxy: &str) -> flint_core::settings::Settings {
    std::fs::write(dir.join("settings.toml"), format!("[proxy]\n{proxy}")).unwrap();
    let settings = flint_core::settings::load_settings(dir).unwrap();
    settings.proxy.use_for_this_process();
    settings
}

// covers UP-125: the settings proxy, with credentials, is used and never shown.
#[test]
fn a_cluster_is_read_through_the_settings_proxy() {
    run_setting_child("settings_proxy_child");
}

#[tokio::test]
async fn settings_proxy_child() {
    if std::env::var_os(SETTING_CHILD).is_none() {
        return;
    }
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_proxy(api.addr()).await;
    let dir = tempfile::tempdir().unwrap();
    let settings = use_proxy_setting(
        dir.path(),
        &format!("url = \"http://user:pa%40ss@{proxy}\"\nno_proxy = [\"other.test\"]\n"),
    );
    assert!(!format!("{settings:?}").contains("pa%40ss"));

    let kubeconfig = api.kubeconfig(None);
    let context = kubeconfig.contexts[0].name.clone();
    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();

    assert!(arrivals(&api) > 0, "the request reached the server");
    {
        let seen = seen.lock().unwrap();
        let head = &seen.heads[0];
        assert!(
            head.starts_with(&format!("CONNECT {} HTTP/1.1", api.addr())),
            "{head}"
        );
        // base64 of `user:pa@ss`.
        assert!(head.contains("dXNlcjpwYUBzcw=="), "{head}");
        assert_eq!(seen.first_bytes[0], 0x16, "TLS runs inside the tunnel");
    }

    // The context is pinned to the setting's route, by its address and an opaque digest.
    let path = dir.path().join("kubeconfig");
    api.write_kubeconfig(&path, None).unwrap();
    let loaded = flint_core::kubeconfig::load(&[path]);
    let route = loaded.contexts[0].proxy_url.clone().unwrap();
    assert!(route.starts_with(&format!("http://{proxy}#")), "{route}");
    assert!(
        !route.contains("pa%40ss") && !route.contains("user"),
        "{route}"
    );
}

// covers UP-125: a cluster's own proxy-url wins over the settings proxy.
#[test]
fn a_cluster_proxy_url_wins_over_the_settings_proxy() {
    run_setting_child("cluster_proxy_wins_child");
}

#[tokio::test]
async fn cluster_proxy_wins_child() {
    if std::env::var_os(SETTING_CHILD).is_none() {
        return;
    }
    let api = FakeApiServer::start().await.unwrap();
    let (setting_proxy, setting_seen) = fake_proxy(api.addr()).await;
    let (cluster_proxy, cluster_seen) = fake_proxy(api.addr()).await;
    let dir = tempfile::tempdir().unwrap();
    use_proxy_setting(dir.path(), &format!("url = \"http://{setting_proxy}\"\n"));

    let (kubeconfig, context) = through(api.kubeconfig(None), &format!("http://{cluster_proxy}"));
    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();

    assert!(!cluster_seen.lock().unwrap().heads.is_empty());
    assert!(setting_seen.lock().unwrap().heads.is_empty());
}

// covers UP-125: the setting's no-proxy list reaches a server directly.
#[test]
fn the_settings_no_proxy_list_reaches_a_server_directly() {
    run_setting_child("settings_no_proxy_child");
}

#[tokio::test]
async fn settings_no_proxy_child() {
    if std::env::var_os(SETTING_CHILD).is_none() {
        return;
    }
    let api = FakeApiServer::start().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    use_proxy_setting(
        dir.path(),
        "url = \"http://127.0.0.1:9\"\nno_proxy = [\"127.0.0.0/8\"]\n",
    );

    let kubeconfig = api.kubeconfig(None);
    let context = kubeconfig.contexts[0].name.clone();
    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();
    assert!(arrivals(&api) > 0);
}

// covers UP-125: a settings proxy that is down is no reason to connect directly.
#[test]
fn a_settings_proxy_that_is_down_does_not_fall_back() {
    run_setting_child("settings_proxy_down_child");
}

#[tokio::test]
async fn settings_proxy_down_child() {
    if std::env::var_os(SETTING_CHILD).is_none() {
        return;
    }
    let api = FakeApiServer::start().await.unwrap();
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let dir = tempfile::tempdir().unwrap();
    use_proxy_setting(
        dir.path(),
        &format!("url = \"http://user:proxy-secret@127.0.0.1:{port}\"\n"),
    );

    let kubeconfig = api.kubeconfig(None);
    let context = kubeconfig.contexts[0].name.clone();
    let error = match cluster::connect(&kubeconfig, &context).await {
        Ok(client) => cluster::server_version(&client).await.unwrap_err(),
        Err(e) => e,
    };
    assert_eq!(arrivals(&api), 0, "it connected directly");
    let text = error.to_string();
    assert!(text.contains("could not reach the proxy"), "{text}");
    assert!(!text.contains("proxy-secret"), "{text}");
}

/// What the fake SOCKS5 proxy saw.
#[derive(Default)]
struct SocksSeen {
    /// Each user name and password the client signed in with.
    accounts: Vec<(String, String)>,
    /// Each request's destination: its address type (1 IPv4, 3 a name, 4 IPv6), host and port.
    destinations: Vec<(u8, String, u16)>,
    /// The first byte the client sent inside each tunnel.
    first_bytes: Vec<u8>,
}

/// A SOCKS5 proxy tunnelling to `upstream`, wherever the client asked to go. With `account`, it
/// takes only that user name and password.
async fn fake_socks(
    upstream: SocketAddr,
    account: Option<(&'static str, &'static str)>,
) -> (String, Arc<Mutex<SocksSeen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(SocksSeen::default()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let _ = socks_tunnel(client, upstream, account, log).await;
            });
        }
    });
    (format!("{}:{}", addr.ip(), addr.port()), seen)
}

/// Reads a length-prefixed field of a SOCKS5 sign-in.
async fn read_field(client: &mut TcpStream) -> std::io::Result<String> {
    let mut len = [0u8; 1];
    client.read_exact(&mut len).await?;
    let mut field = vec![0u8; usize::from(len[0])];
    client.read_exact(&mut field).await?;
    Ok(String::from_utf8_lossy(&field).into_owned())
}

async fn socks_tunnel(
    mut client: TcpStream,
    upstream: SocketAddr,
    account: Option<(&'static str, &'static str)>,
    seen: Arc<Mutex<SocksSeen>>,
) -> std::io::Result<()> {
    let mut greeting = [0u8; 2];
    client.read_exact(&mut greeting).await?;
    let mut methods = vec![0u8; usize::from(greeting[1])];
    client.read_exact(&mut methods).await?;
    match account {
        Some((user, password)) => {
            if !methods.contains(&2) {
                return client.write_all(&[5, 0xff]).await;
            }
            client.write_all(&[5, 2]).await?;
            let mut version = [0u8; 1];
            client.read_exact(&mut version).await?;
            let given = (
                read_field(&mut client).await?,
                read_field(&mut client).await?,
            );
            let accepted = given.0 == user && given.1 == password;
            seen.lock().unwrap().accounts.push(given);
            client.write_all(&[1, u8::from(!accepted)]).await?;
            if !accepted {
                return Ok(());
            }
        }
        None => client.write_all(&[5, 0]).await?,
    }
    let mut request = [0u8; 4];
    client.read_exact(&mut request).await?;
    let host = match request[3] {
        1 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => read_field(&mut client).await?,
        4 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => return Ok(()),
    };
    let mut port = [0u8; 2];
    client.read_exact(&mut port).await?;
    seen.lock()
        .unwrap()
        .destinations
        .push((request[3], host, u16::from_be_bytes(port)));
    let mut server = TcpStream::connect(upstream).await?;
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    let mut byte = [0u8; 1];
    client.read_exact(&mut byte).await?;
    seen.lock().unwrap().first_bytes.push(byte[0]);
    server.write_all(&byte).await?;
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
}

/// `kubeconfig` with its server addressed as `localhost`, which its certificate names.
fn by_name(mut kubeconfig: Kubeconfig, api: &FakeApiServer) -> Kubeconfig {
    kubeconfig.clusters[0].cluster.as_mut().unwrap().server =
        Some(format!("https://localhost:{}", api.addr().port()));
    kubeconfig
}

// covers UP-125: socks5h hands the server's name to the proxy, with the URL's account.
#[tokio::test]
async fn a_cluster_is_read_through_a_socks5h_proxy_that_resolves_its_name() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_socks(api.addr(), Some(("alice", "pa@ss"))).await;
    let url = format!("socks5h://alice:pa%40ss@{proxy}");
    let (kubeconfig, context) = through(by_name(api.kubeconfig(None), &api), &url);

    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();

    assert!(arrivals(&api) > 0, "the request reached the server");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.accounts[0], ("alice".to_owned(), "pa@ss".to_owned()));
    let port = api.addr().port();
    assert_eq!(seen.destinations[0], (3, "localhost".to_owned(), port));
    // 0x16 opens a TLS handshake: the proxy carries TLS, not the request.
    assert_eq!(seen.first_bytes[0], 0x16);
}

// covers UP-125: socks5 resolves the server's name here and hands the proxy an address.
#[tokio::test]
async fn a_socks5_proxy_is_given_an_address_resolved_here() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_socks(api.addr(), None).await;
    let url = format!("socks5://{proxy}");
    let (kubeconfig, context) = through(by_name(api.kubeconfig(None), &api), &url);

    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();

    let seen = seen.lock().unwrap();
    let (kind, host, port) = &seen.destinations[0];
    assert!(
        *kind == 1 || *kind == 4,
        "the name went to the proxy: {host}"
    );
    assert!(host == "127.0.0.1" || host == "::1", "{host}");
    assert_eq!(*port, api.addr().port());
    assert_eq!(seen.first_bytes[0], 0x16);
}

#[tokio::test]
async fn a_socks_proxy_that_refuses_the_account_says_so_and_sends_nothing() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_socks(api.addr(), Some(("alice", "right"))).await;
    for url in [
        format!("socks5h://alice:proxy-secret@{proxy}"),
        format!("socks5://{proxy}"),
    ] {
        let (kubeconfig, context) = through(api.kubeconfig(None), &url);
        let error = match cluster::connect(&kubeconfig, &context).await {
            Ok(client) => cluster::server_version(&client).await.unwrap_err(),
            Err(e) => e,
        };
        let text = error.to_string();
        assert!(text.contains("asks for credentials"), "{text}");
        assert!(!text.contains("proxy-secret"), "{text}");
    }
    assert!(seen.lock().unwrap().destinations.is_empty());
    assert_eq!(arrivals(&api), 0);
}

#[tokio::test]
async fn a_socks5_server_name_that_does_not_resolve_says_so() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_socks(api.addr(), None).await;
    let (mut kubeconfig, context) = through(api.kubeconfig(None), &format!("socks5://{proxy}"));
    // `.invalid` never resolves (RFC 2606).
    kubeconfig.clusters[0].cluster.as_mut().unwrap().server =
        Some("https://flint-test.invalid:6443".to_owned());

    let error = match cluster::connect(&kubeconfig, &context).await {
        Ok(client) => cluster::server_version(&client).await.unwrap_err(),
        Err(e) => e,
    };

    let text = error.to_string();
    if seen.lock().unwrap().destinations.is_empty() {
        assert!(text.contains("could not resolve"), "{text}");
        assert_eq!(arrivals(&api), 0);
    } else {
        // Some resolvers answer every name; then the proxy was given that answer.
        eprintln!("skipped: this resolver answers for flint-test.invalid");
    }
}

#[tokio::test]
async fn a_socks_proxy_that_is_down_does_not_fall_back_to_a_direct_connection() {
    let api = FakeApiServer::start().await.unwrap();
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let url = format!("socks5h://user:proxy-secret@127.0.0.1:{port}");
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

#[tokio::test]
async fn nothing_goes_through_a_socks_proxy_to_a_server_without_verified_tls() {
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_socks(api.addr(), None).await;
    let (mut kubeconfig, context) = through(api.kubeconfig(None), &format!("socks5h://{proxy}"));
    kubeconfig.clusters[0]
        .cluster
        .as_mut()
        .unwrap()
        .insecure_skip_tls_verify = Some(true);

    let Err(error) = cluster::connect(&kubeconfig, &context).await else {
        panic!("connected through the proxy");
    };

    assert!(error.to_string().contains("did not connect"), "{error}");
    assert!(
        seen.lock().unwrap().destinations.is_empty(),
        "the proxy was used"
    );
    assert_eq!(arrivals(&api), 0);
}

// covers UP-125: the settings proxy can be a SOCKS5 one.
#[test]
fn a_cluster_is_read_through_a_socks_settings_proxy() {
    run_setting_child("socks_settings_proxy_child");
}

#[tokio::test]
async fn socks_settings_proxy_child() {
    if std::env::var_os(SETTING_CHILD).is_none() {
        return;
    }
    let api = FakeApiServer::start().await.unwrap();
    let (proxy, seen) = fake_socks(api.addr(), Some(("alice", "pa@ss"))).await;
    let dir = tempfile::tempdir().unwrap();
    use_proxy_setting(
        dir.path(),
        &format!("url = \"socks5h://alice:pa%40ss@{proxy}\"\n"),
    );

    let kubeconfig = by_name(api.kubeconfig(None), &api);
    let context = kubeconfig.contexts[0].name.clone();
    let client = cluster::connect(&kubeconfig, &context).await.unwrap();
    cluster::server_version(&client).await.unwrap();

    assert!(arrivals(&api) > 0, "the request reached the server");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.destinations[0].1, "localhost");
    assert_eq!(seen.first_bytes[0], 0x16);
}

/// Set for the child process of [`an_unreadable_settings_file_refuses_the_settings_route`].
const UNREADABLE_SETTINGS_CHILD: &str = "FLINT_TEST_UNREADABLE_SETTINGS_CHILD";

// covers UP-125: an unreadable settings.toml sends nothing direct or through the environment.
#[test]
fn an_unreadable_settings_file_refuses_the_settings_route() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("settings.toml"),
        "[proxy]\nurl = \"socks5h://alice:proxy-secret@127.0.0.1:1\" oops\n",
    )
    .unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["unreadable_settings_child", "--exact", "--nocapture"])
        .env(UNREADABLE_SETTINGS_CHILD, "1")
        .env("FLINT_CONFIG_DIR", dir.path())
        // No environment proxy: before, a cluster without a proxy-url went direct.
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        // No login shell for `PATH`.
        .env_remove("SHELL")
        .env("KUBECONFIG", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn unreadable_settings_child() {
    if std::env::var_os(UNREADABLE_SETTINGS_CHILD).is_none() {
        return;
    }
    // SAFETY: this child runs this one test, before any runtime starts.
    let boot = unsafe { flint_core::bootstrap::bootstrap() }.unwrap();
    let error = boot.settings_error.clone().unwrap();
    assert!(
        error.contains("proxy setting could not be applied"),
        "{error}"
    );
    assert!(!error.contains("proxy-secret"), "{error}");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let api = FakeApiServer::start().await.unwrap();
            let kubeconfig = api.kubeconfig(None);
            let context = kubeconfig.contexts[0].name.clone();
            let Err(refused) = cluster::connect(&kubeconfig, &context).await else {
                panic!("connected without the proxy setting");
            };
            assert!(refused.to_string().contains("settings.toml"), "{refused}");
            assert_eq!(arrivals(&api), 0, "it connected directly");

            // A cluster's own proxy-url is still used.
            let (proxy, _) = fake_socks(api.addr(), None).await;
            let (kubeconfig, context) = through(kubeconfig, &format!("socks5h://{proxy}"));
            let client = cluster::connect(&kubeconfig, &context).await.unwrap();
            cluster::server_version(&client).await.unwrap();
            assert!(arrivals(&api) > 0);
        });
}
