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
    assert_ne!(one, two);
    assert_ne!(one, three);
    assert_eq!(one, route_identity("http://alice:s3cret@proxy:3128"));
    assert_ne!(one, route_identity("http://proxy:3128"));
    assert_eq!(route_identity("http://proxy:3128"), "http://proxy:3128");
    for text in [&one, &two, &three] {
        assert!(
            !text.contains("alice") && !text.contains("s3cret"),
            "{text}"
        );
        assert_eq!(shown(text), "http://proxy:3128", "shown without its digest");
    }
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
