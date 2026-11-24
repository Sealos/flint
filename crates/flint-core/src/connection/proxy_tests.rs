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
        "socks4://user:s3cret@bastion:1080",
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
    let Kind::Connect { authorization } = &target.kind else {
        panic!("not a CONNECT proxy");
    };
    // base64 of `user:pa@ss`: percent escapes are decoded first.
    assert_eq!(authorization.as_ref().unwrap(), "Basic dXNlcjpwYUBzcw==");
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

fn setting(url: Option<&str>, no_proxy: &[&str]) -> ProxySetting {
    ProxySetting {
        url: url.map(str::to_owned),
        no_proxy: no_proxy.iter().map(|e| (*e).to_owned()).collect(),
    }
}

#[test]
fn a_cluster_proxy_url_wins_over_the_setting_which_wins_over_the_environment() {
    let env = environment(Some("http://env:1"), None);
    let set = setting(Some("http://setting:2"), &[]);
    let merged = env.clone().under(Some(&set));
    assert_eq!(
        chosen(Some("http://entry:3"), Some(SERVER), &merged),
        Some("http://entry:3")
    );
    assert_eq!(
        chosen(None, Some(SERVER), &merged),
        Some("http://setting:2")
    );
    assert_eq!(chosen(None, Some(SERVER), &env), Some("http://env:1"));
    let nothing = environment(None, None).under(Some(&ProxySetting::default()));
    assert_eq!(chosen(None, Some(SERVER), &nothing), None);
}

#[test]
fn an_empty_setting_leaves_the_environment_as_it_is() {
    let env = environment(Some("http://env:1"), Some("other.test"));
    for empty in [ProxySetting::default(), setting(Some("  "), &["", " "])] {
        assert!(empty.is_empty());
        let merged = env.clone().under(Some(&empty));
        assert_eq!(merged.https_proxy.as_deref(), Some("http://env:1"));
        assert_eq!(merged.no_proxy.as_deref(), Some("other.test"));
    }
}

#[test]
fn only_the_settings_no_proxy_list_exempts_from_its_proxy() {
    // The environment's NO_PROXY belongs to the environment's proxy.
    let env = environment(Some("http://env:1"), Some(".example.test"));
    let set = setting(Some("http://setting:2"), &[]);
    let merged = env.clone().under(Some(&set));
    assert_eq!(
        chosen(None, Some(SERVER), &merged),
        Some("http://setting:2")
    );

    let exempting = setting(Some("http://setting:2"), &[" api.example.test ", ""]);
    let merged = env.clone().under(Some(&exempting));
    assert_eq!(chosen(None, Some(SERVER), &merged), None);
    assert_eq!(
        chosen(None, Some("https://other.test"), &merged),
        Some("http://setting:2")
    );
}

#[test]
fn a_no_proxy_setting_alone_applies_to_the_environment_proxy() {
    let env = environment(Some("http://env:1"), Some("other.test"));
    let merged = env.under(Some(&setting(None, &["*.example.test"])));
    assert_eq!(chosen(None, Some(SERVER), &merged), None);
    assert_eq!(
        chosen(None, Some("https://other.test"), &merged),
        Some("http://env:1")
    );
}

#[test]
fn the_settings_no_proxy_list_matches_names_domains_addresses_and_ranges() {
    let set = setting(
        Some("http://setting:2"),
        &[
            "exact.test",
            ".corp.test",
            "*.wild.test",
            "10.0.0.0/8",
            "192.0.2.7:6443",
        ],
    );
    let merged = environment(None, None).under(Some(&set));
    for exempt in [
        "https://exact.test",
        "https://api.corp.test:6443",
        "https://a.wild.test",
        "https://10.1.2.3:6443",
        "https://192.0.2.7:6443",
    ] {
        assert_eq!(chosen(None, Some(exempt), &merged), None, "{exempt}");
    }
    for proxied in [
        "https://notexact.test",
        "https://wild.test",
        "https://11.1.2.3",
        "https://192.0.2.7:443",
    ] {
        assert_eq!(
            chosen(None, Some(proxied), &merged),
            Some("http://setting:2"),
            "{proxied}"
        );
    }
}

#[test]
fn a_proxy_setting_never_shows_its_credentials() {
    let set = setting(Some(ACCOUNT), &["10.0.0.0/8"]);
    assert_eq!(set.shown_url().as_deref(), Some("http://proxy:3128"));
    let settings = crate::settings::Settings {
        proxy: set.clone(),
        ..Default::default()
    };
    for text in [
        format!("{set:?}"),
        format!("{settings:?}"),
        format!("{settings:#?}"),
    ] {
        assert!(text.contains("http://proxy:3128"), "{text}");
        assert!(
            !text.contains("alice") && !text.contains("s3cret"),
            "{text}"
        );
    }
    let unreadable = setting(Some("alice:s3cret@proxy:3128"), &[]);
    assert_eq!(unreadable.shown_url().as_deref(), Some(UNREADABLE));
    assert!(!format!("{unreadable:?}").contains("s3cret"));
    // Its route identity tells the account apart without holding it.
    let identity = route_identity(ACCOUNT);
    assert!(identity.starts_with("http://proxy:3128#"));
    assert!(!identity.contains("alice") && !identity.contains("s3cret"));
}

#[test]
fn an_unsupported_setting_is_refused_without_its_credentials() {
    let merged = environment(None, None).under(Some(&setting(
        Some("socks4://alice:s3cret@proxy:1080"),
        &[],
    )));
    let chosen = chosen(None, Some(SERVER), &merged).unwrap();
    let text = ProxyTarget::parse(chosen).unwrap_err().to_string();
    assert!(text.contains("socks4://proxy:1080"), "{text}");
    assert!(
        !text.contains("s3cret") && !text.contains("alice"),
        "{text}"
    );
}

#[test]
fn the_proxy_section_loads_from_settings_toml() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(crate::settings::SETTINGS_FILE),
        "[proxy]\nurl = \"http://alice:s3cret@proxy:3128\"\n\
         no_proxy = [\"10.0.0.0/8\", \".corp.test\"]\n",
    )
    .unwrap();
    let settings = crate::settings::load_settings(dir.path()).unwrap();
    assert_eq!(settings.proxy.url.as_deref(), Some(ACCOUNT));
    assert_eq!(settings.proxy.no_proxy, ["10.0.0.0/8", ".corp.test"]);
    assert_eq!(
        settings.proxy.no_proxy_list().as_deref(),
        Some("10.0.0.0/8,.corp.test")
    );
    // An unset section is not written back.
    let saved = toml::to_string(&crate::settings::Settings::default()).unwrap();
    assert!(!saved.contains("proxy"), "{saved}");
}

#[test]
fn a_settings_error_near_the_proxy_does_not_quote_its_credentials() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(crate::settings::SETTINGS_FILE),
        "[proxy]\nurl = \"http://alice:s3cret@proxy:3128\" oops\n",
    )
    .unwrap();
    let error = crate::settings::load_settings(dir.path()).unwrap_err();
    let reason = error.safe_reason();
    assert!(
        !reason.contains("s3cret") && !reason.contains("alice"),
        "{reason}"
    );
    let summary = crate::redact::safe_error_summary(&error.to_string());
    assert!(!summary.contains("s3cret"), "{summary}");
}

/// The SOCKS5 parts of `target`.
fn socks(target: &ProxyTarget) -> (SocksResolution, Option<(String, String)>) {
    match &target.kind {
        Kind::Socks {
            resolution,
            account,
        } => (*resolution, account.clone()),
        Kind::Connect { .. } => panic!("not a SOCKS proxy"),
    }
}

#[test]
fn socks5_resolves_names_here_and_socks5h_at_the_proxy() {
    let local = ProxyTarget::parse("socks5://bastion:1081").unwrap();
    assert!(local.is_socks());
    assert_eq!(socks(&local), (SocksResolution::Local, None));
    assert_eq!(local.uri.to_string(), "socks5://bastion:1081/");
    let remote = ProxyTarget::parse(" SOCKS5H://Bastion:1081 ").unwrap();
    assert_eq!(socks(&remote), (SocksResolution::Proxy, None));
    assert_eq!(remote.uri.to_string(), "socks5h://bastion:1081/");
    assert!(!ProxyTarget::parse("http://proxy:3128").unwrap().is_socks());
}

#[test]
fn a_socks_proxy_without_a_port_uses_1080() {
    for (proxy, address) in [
        ("socks5://bastion", "socks5://bastion:1080/"),
        ("socks5h://alice:s3cret@[::1]", "socks5h://[::1]:1080/"),
    ] {
        let target = ProxyTarget::parse(proxy).unwrap();
        assert_eq!(target.uri.to_string(), address, "{proxy}");
    }
}

#[test]
fn a_socks_account_is_decoded_and_never_shown() {
    let proxy = "socks5h://alice:pa%40ss@bastion:1080";
    let target = ProxyTarget::parse(proxy).unwrap();
    assert_eq!(
        socks(&target).1,
        Some(("alice".to_owned(), "pa@ss".to_owned()))
    );
    for text in [
        format!("{target:?}"),
        target.uri.to_string(),
        shown(proxy),
        route_identity(proxy),
    ] {
        assert!(!text.contains("alice") && !text.contains("pa%40ss"));
    }
}

#[test]
fn a_socks_account_longer_than_socks5_allows_is_refused() {
    let long = "a".repeat(256);
    for proxy in [
        format!("socks5://{long}:s3cret@bastion:1080"),
        format!("socks5h://alice:{long}@bastion:1080"),
    ] {
        let refused = ProxyTarget::parse(&proxy).unwrap_err();
        assert_eq!(refused, ProxyRefused::SocksCredentialTooLong);
        assert!(!refused.to_string().contains("s3cret"));
    }
    let limit = "a".repeat(255);
    assert!(ProxyTarget::parse(&format!("socks5://{limit}:{limit}@bastion:1080")).is_ok());
}

#[test]
fn each_proxy_kind_is_its_own_route() {
    let identities = [
        route_identity("http://bastion:1080"),
        route_identity("socks5://bastion:1080"),
        route_identity("socks5h://bastion:1080"),
        route_identity("socks5h://alice:s3cret@bastion:1080"),
        route_identity("socks5h://bob:s3cret@bastion:1080"),
    ];
    for (i, one) in identities.iter().enumerate() {
        for two in &identities[i + 1..] {
            assert!(one != two, "two routes share an identity");
        }
    }
    assert!(identities[2] == "socks5h://bastion:1080");
    assert!(identities[3].starts_with("socks5h://bastion:1080#"));
}

#[test]
fn a_socks_proxy_follows_the_same_precedence_and_no_proxy() {
    let env = environment(Some("socks5://env:1080"), Some(".example.test"));
    assert_eq!(chosen(None, Some(SERVER), &env), None);
    assert_eq!(
        chosen(None, Some("https://other.test"), &env),
        Some("socks5://env:1080")
    );
    let set = setting(Some("socks5h://setting:1080"), &["other.test"]);
    let merged = env.under(Some(&set));
    assert_eq!(
        chosen(None, Some(SERVER), &merged),
        Some("socks5h://setting:1080")
    );
    assert_eq!(chosen(None, Some("https://other.test"), &merged), None);
    assert_eq!(
        chosen(Some("http://entry:3128"), Some(SERVER), &merged),
        Some("http://entry:3128")
    );
}

#[test]
fn an_unreadable_settings_file_refuses_routes_that_would_take_it() {
    let env = ProxyEnvironment::unreadable_setting();
    // Nothing is exempt: the setting's no-proxy list is unknown too.
    let chosen_proxy = chosen(None, Some(SERVER), &env).unwrap();
    let refused = ProxyTarget::parse(chosen_proxy).unwrap_err();
    assert_eq!(refused, ProxyRefused::SettingsUnreadable);
    assert!(refused.to_string().contains("settings.toml"), "{refused}");
    assert!(refused.to_string().contains("did not connect"), "{refused}");
    // A cluster's own proxy-url is still used.
    assert_eq!(
        chosen(Some("socks5://entry:1080"), Some(SERVER), &env),
        Some("socks5://entry:1080")
    );
    // Its route is named plainly and holds nothing of the file.
    assert_eq!(shown(chosen_proxy), SETTINGS_UNREADABLE_SHOWN);
    assert!(route_identity(chosen_proxy) == SETTINGS_UNREADABLE_SHOWN);
}

#[test]
fn the_settings_error_says_the_proxy_setting_was_not_applied() {
    let reason = "invalid TOML at line 1, column 2 in /cfg/settings.toml".to_owned();
    assert_eq!(with_proxy_note(reason.clone(), false), reason);
    let noted = with_proxy_note(reason.clone(), true);
    assert!(noted.starts_with(&reason), "{noted}");
    assert!(
        noted.contains("proxy setting could not be applied"),
        "{noted}"
    );
}

#[test]
fn a_plugin_gets_the_settings_proxy_and_no_proxy_list() {
    let variables = setting_variables(&setting(Some(" http://proxy:3128 "), &["10.0.0.0/8"]));
    assert_eq!(
        variables,
        [
            ("HTTPS_PROXY", Some("http://proxy:3128".to_owned())),
            ("https_proxy", Some("http://proxy:3128".to_owned())),
            ("NO_PROXY", Some("10.0.0.0/8".to_owned())),
            ("no_proxy", Some("10.0.0.0/8".to_owned())),
        ]
    );
    // A URL without a list removes the environment's, which belongs to its own proxy.
    let socks = setting_variables(&setting(Some("socks5h://bastion:1080"), &[]));
    let url = Some("socks5h://bastion:1080".to_owned());
    assert_eq!(
        socks,
        [
            ("HTTPS_PROXY", url.clone()),
            ("https_proxy", url.clone()),
            ("ALL_PROXY", url.clone()),
            ("all_proxy", url),
            ("NO_PROXY", None),
            ("no_proxy", None),
        ]
    );
    // A list alone replaces NO_PROXY only.
    assert_eq!(
        setting_variables(&setting(None, &["other.test"])),
        [
            ("NO_PROXY", Some("other.test".to_owned())),
            ("no_proxy", Some("other.test".to_owned())),
        ]
    );
    assert!(setting_variables(&ProxySetting::default()).is_empty());
}

#[test]
fn a_plugin_never_gets_the_settings_proxy_credentials() {
    let explicit: Vec<(OsString, Option<OsString>)> =
        setting_variables(&setting(Some("socks5://alice:s3cret@bastion:1080"), &[]))
            .into_iter()
            .map(|(name, value)| (OsString::from(name), value.map(OsString::from)))
            .collect();
    let changes = plugin_environment(Vec::new(), &explicit);
    let proxies = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];
    assert_eq!(changes.len(), proxies.len());
    for (name, address) in changes {
        assert!(proxies.iter().any(|p| name == *p), "{name:?}");
        assert_eq!(address.as_deref(), Some("socks5://bastion:1080"));
    }
}
