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
