use super::*;

fn environment(https_proxy: Option<&str>, no_proxy: Option<&str>) -> ProxyEnvironment {
    ProxyEnvironment {
        https_proxy: https_proxy.map(str::to_owned),
        no_proxy: no_proxy.map(str::to_owned),
    }
}

const SERVER: &str = "https://api.example.test:6443";

#[test]
fn a_proxy_url_wins_over_the_environment_and_no_proxy() {
    let env = environment(Some("http://env:1"), Some("api.example.test"));
    assert_eq!(
        chosen(Some("http://entry:2"), Some(SERVER), &env),
        Some("http://entry:2")
    );
}

#[test]
fn no_proxy_exempts_only_the_environment_proxy() {
    let exempt = environment(Some("http://env:1"), Some(".example.test"));
    assert_eq!(chosen(None, Some(SERVER), &exempt), None);
    let other = environment(Some("http://env:1"), Some("other.test"));
    assert_eq!(chosen(None, Some(SERVER), &other), Some("http://env:1"));
    assert_eq!(
        chosen(Some(" "), Some(SERVER), &other),
        Some("http://env:1")
    );
}

#[test]
fn a_server_that_cannot_be_read_is_not_exempt() {
    let env = environment(Some("http://env:1"), Some("*"));
    assert_eq!(chosen(None, Some("not a url"), &env), Some("http://env:1"));
}

#[test]
fn shown_never_holds_credentials() {
    assert_eq!(
        shown("http://user:s3cret@Proxy.Example:3128/path?token=abc"),
        "http://proxy.example:3128"
    );
    for unreadable in ["user:s3cret@proxy:3128", "http://user:s3 cret@proxy", ""] {
        let text = shown(unreadable);
        assert_eq!(text, UNREADABLE, "{unreadable}");
    }
}

#[test]
fn unsupported_schemes_are_refused_without_credentials() {
    for url in [
        "socks5://user:s3cret@bastion:1080",
        "https://user:s3cret@proxy:3128",
    ] {
        let refused = ProxyTarget::parse(url).unwrap_err();
        let text = refused.to_string();
        assert!(matches!(refused, ProxyRefused::Unsupported { .. }), "{url}");
        assert!(!text.contains("s3cret") && !text.contains("user"), "{text}");
        assert!(text.contains("did not connect"), "{text}");
    }
    assert_eq!(
        ProxyTarget::parse("user:s3cret@proxy:3128").unwrap_err(),
        ProxyRefused::Unreadable
    );
}

#[test]
fn a_target_keeps_credentials_out_of_its_address_and_debug() {
    let target = ProxyTarget::parse("http://user:pa%40ss@proxy:3128").unwrap();
    assert_eq!(target.uri.to_string(), "http://proxy:3128/");
    // base64 of `user:pa@ss`: percent escapes are decoded first.
    assert_eq!(
        target.authorization.as_ref().unwrap(),
        "Basic dXNlcjpwYUBzcw=="
    );
    let debug = format!("{target:?}");
    assert!(
        !debug.contains("dXNlcjpwYUBzcw") && !debug.contains("user"),
        "{debug}"
    );
}

#[test]
fn a_no_proxy_wildcard_exempts_names_and_addresses() {
    let env = environment(Some("http://env:1"), Some("other.test, *"));
    for server in [SERVER, "https://10.0.0.1:6443", "https://[fd00::1]:6443"] {
        assert_eq!(chosen(None, Some(server), &env), None, "{server}");
    }
}

#[test]
fn a_no_proxy_port_exempts_only_that_port() {
    for entry in ["api.example.test:6443", ".example.test:6443"] {
        let env = environment(Some("http://env:1"), Some(entry));
        assert_eq!(chosen(None, Some(SERVER), &env), None, "{entry}");
        let other = "https://api.example.test:7443";
        assert_eq!(
            chosen(None, Some(other), &env),
            Some("http://env:1"),
            "{entry}"
        );
    }
    let default_port = environment(Some("http://env:1"), Some("api.example.test:443"));
    assert_eq!(
        chosen(None, Some("https://api.example.test"), &default_port),
        None
    );
    for (entry, server) in [
        ("10.0.0.1:6443", "https://10.0.0.1:6443"),
        ("[fd00::1]:6443", "https://[fd00::1]:6443"),
        ("[fd00::1]", "https://[fd00::1]:6443"),
        ("fd00::1", "https://[fd00::1]:6443"),
    ] {
        let env = environment(Some("http://env:1"), Some(entry));
        assert_eq!(chosen(None, Some(server), &env), None, "{entry}");
    }
    let env = environment(Some("http://env:1"), Some("[fd00::1]:7443"));
    assert_eq!(
        chosen(None, Some("https://[fd00::1]:6443"), &env),
        Some("http://env:1")
    );
}

#[test]
fn an_authority_that_cannot_be_trusted_is_refused_and_not_shown() {
    for proxy in [
        "http://user:s3cret@proxy:65536",
        "http://proxy:65536",
        "http://user:s3/cret@proxy:3128",
        "http://user:s3?cret@proxy:3128",
        "http://proxy:",
    ] {
        assert_eq!(shown(proxy), UNREADABLE, "{proxy}");
        assert_eq!(
            ProxyTarget::parse(proxy).unwrap_err(),
            ProxyRefused::Unreadable,
            "{proxy}"
        );
    }
    assert_eq!(
        ProxyTarget::parse("socks5://user:s3/cret@bastion:1080").unwrap_err(),
        ProxyRefused::Unreadable
    );
}

#[test]
fn a_malformed_authority_is_refused_whole() {
    for proxy in [
        "http://user:s3cret@[::1]junk:3128",
        "http://[::1]junk:3128",
        "http://[not-an-address]:3128",
        "http://user:s3cret@pro xy:3128",
        "http://proxy:+3128",
    ] {
        assert_eq!(shown(proxy), UNREADABLE, "{proxy}");
        assert!(ProxyTarget::parse(proxy).is_err(), "{proxy}");
    }
    assert_eq!(shown("http://[::1]:3128"), "http://[::1]:3128");
}

#[test]
fn a_no_proxy_wildcard_suffix_honors_its_port() {
    let env = environment(Some("http://env:1"), Some("*.example.test:6443"));
    assert_eq!(chosen(None, Some(SERVER), &env), None);
    for server in [
        "https://api.example.test:7443",
        "https://example.test:6443",
        "https://apiexample.test:6443",
    ] {
        assert_eq!(
            chosen(None, Some(server), &env),
            Some("http://env:1"),
            "{server}"
        );
    }
    let any_port = environment(Some("http://env:1"), Some("*.EXAMPLE.test"));
    assert_eq!(chosen(None, Some(SERVER), &any_port), None);
}

#[test]
fn a_route_identity_tells_accounts_apart_without_holding_them() {
    let one = route_identity("http://alice:s3cret@proxy:3128");
    let two = route_identity("http://bob:s3cret@proxy:3128");
    let three = route_identity("http://alice:other@proxy:3128");
    // Identities are compared without formatting them into a failure message.
    assert!(one != two, "different users differ");
    assert!(one != three, "different passwords differ");
    assert!(
        one == route_identity("http://alice:s3cret@proxy:3128"),
        "the same account is stable"
    );
    assert!(
        one != route_identity("http://proxy:3128"),
        "an account differs from no account"
    );
    assert_eq!(route_identity("http://proxy:3128"), "http://proxy:3128");
    for text in [&one, &two, &three] {
        assert!(
            !text.contains("alice") && !text.contains("s3cret"),
            "an identity holds no credential"
        );
        assert!(
            shown(text) == "http://proxy:3128",
            "shown without its digest"
        );
    }
}

const ACCOUNT: &str = "http://alice:s3cret@proxy:3128";

#[test]
fn installs_with_different_keys_have_different_digests() {
    let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let at = |dir: &tempfile::TempDir| route_identity_with(ACCOUNT, || open_route_key(dir.path()));
    assert!(at(&one) != at(&two), "each install has its own key");
    assert!(is_stable_identity(&at(&one)));
}

#[test]
fn one_install_keeps_its_identity_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    // A restart, or the other of the app and `flint mcp`: each opens the file afresh.
    let first = route_identity_with(ACCOUNT, || open_route_key(dir.path()));
    let second = route_identity_with(ACCOUNT, || open_route_key(dir.path()));
    assert!(first == second, "the identity survives a restart");
    let path = dir.path().join(ROUTE_KEY_FILE);
    let meta = std::fs::metadata(path).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    assert_eq!((meta.len(), meta.permissions().mode() & 0o777), (32, 0o600));
}

#[test]
fn an_unreadable_key_gives_no_stable_identity() {
    let dir = tempfile::tempdir().unwrap();
    let saved = route_identity_with(ACCOUNT, || open_route_key(dir.path()));
    std::fs::write(dir.path().join(ROUTE_KEY_FILE), b"short").unwrap();

    let now = route_identity_with(ACCOUNT, || open_route_key(dir.path()));

    assert!(now != saved, "a damaged key changes the identity");
    assert!(now != shown(ACCOUNT), "never the address alone");
    assert!(!is_stable_identity(&now));
    let other = route_identity_with("http://bob:s3cret@proxy:3128", || None);
    assert!(now != other, "accounts stay apart");
    assert!(
        now == route_identity_with(ACCOUNT, || None),
        "same across reloads"
    );
    assert_eq!(
        std::fs::read(dir.path().join(ROUTE_KEY_FILE)).unwrap(),
        b"short",
        "a damaged key is not replaced"
    );

    let mut settings = crate::settings::Settings::default();
    let granted = grant_endpoint(SERVER, Some(&saved)).into_owned();
    settings
        .trusted_clusters
        .push(crate::settings::ClusterPin::new("prod", granted));
    let unlocks = crate::protection::SessionUnlocks::default();
    unlocks.unlock("prod", &grant_endpoint(SERVER, Some(&now)));
    assert_eq!(
        crate::protection::protection(&settings, &unlocks, "prod", Some(SERVER), Some(&now)),
        crate::protection::Protection::Protected,
        "neither the saved grant nor an unlock on this route opens writes"
    );
}

#[test]
fn a_saved_grant_holds_no_part_of_the_credential() {
    let dir = tempfile::tempdir().unwrap();
    let identity = route_identity_with(ACCOUNT, || open_route_key(dir.path()));
    let mut settings = crate::settings::Settings::default();
    settings
        .trusted_clusters
        .push(crate::settings::ClusterPin::new(
            "prod",
            grant_endpoint(SERVER, Some(&identity)),
        ));
    let saved = toml::to_string(&settings).unwrap();
    assert!(
        saved.contains("http://proxy:3128#"),
        "the grant keeps the proxy and a digest"
    );
    assert!(
        !saved.contains("alice") && !saved.contains("s3cret"),
        "the grant holds no credential"
    );
}

#[test]
fn only_a_proxy_account_needs_the_route_key() {
    let refused = check_account_key_with(ACCOUNT, || None).unwrap_err();
    let text = refused.to_string();
    assert!(text.contains("did not connect"), "{text}");
    assert!(
        !text.contains("alice") && !text.contains("s3cret"),
        "{text}"
    );
    check_account_key_with("http://proxy:3128", || panic!("no account, no key")).unwrap();
    let key = || Some(hmac::Key::new(hmac::HMAC_SHA256, &[1; 32]));
    check_account_key_with(ACCOUNT, key).unwrap();
}

#[test]
fn a_key_file_left_open_is_made_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(ROUTE_KEY_FILE);
    std::fs::write(&path, [3; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert!(open_route_key(dir.path()).is_some());

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn without_the_route_key_a_proxy_account_connects_to_nothing() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (plugin, mark) = (dir.path().join("plugin"), dir.path().join("launched"));
    std::fs::write(
        &plugin,
        format!(
            "#!/bin/sh\ntouch '{}'\nprintf '{{\"status\":{{\"token\":\"t\"}}}}'\n",
            mark.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let yaml = format!(
        "apiVersion: v1\nkind: Config\ncurrent-context: c\nclusters:\n- name: c\n  cluster:\n    \
         server: https://127.0.0.1:1\n    proxy-url: http://alice:s3cret@127.0.0.1:{port}\n\
         contexts:\n- name: c\n  context:\n    cluster: c\n    user: u\nusers:\n- name: u\n  \
         user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      \
         command: {}\n      interactiveMode: Never\n",
        plugin.display()
    );
    let kubeconfig = kube::config::Kubeconfig::from_yaml(&yaml).unwrap();

    ROUTE_KEY_FAILS.set(true);
    let outcome = crate::cluster::connect(&kubeconfig, "c").await;
    ROUTE_KEY_FAILS.set(false);

    let error = outcome
        .err()
        .expect("the connection was refused")
        .to_string();
    assert!(error.contains("did not connect"), "{error}");
    assert!(
        !error.contains("alice") && !error.contains("s3cret"),
        "{error}"
    );
    assert!(!mark.exists(), "the plugin was launched");
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "reached the proxy"
    );
}

#[test]
fn a_grant_differs_for_each_route() {
    assert_eq!(grant_endpoint(SERVER, None), SERVER);
    let one = grant_endpoint(SERVER, Some("http://one:1"));
    let two = grant_endpoint(SERVER, Some("http://two:1"));
    assert_ne!(one, SERVER);
    assert_ne!(one, two);
    assert_eq!(
        one,
        grant_endpoint("https://api.example.test:6443/", Some("http://one:1"))
    );
}
