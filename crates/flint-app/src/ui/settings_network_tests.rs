//! Tests for the Network section of Settings (#743): the proxy checked as typed, its password
//! never shown, the confirmation before Apply, and Apply reconnecting through the core. Applying
//! a proxy changes it for the whole process, so the one test that applies runs in a child
//! process; the others stop before the core is asked. Loopback fixtures only.

use std::path::Path;

use flint_core::handle::{ForwardReconnect, ProxyReconnectReport};
use flint_core::settings::ProxySetting;
use gpui_kit::test::TestAppContextExt as _;
use gpui_kit::{AppContext as _, TestAppContext};
use serde_json::{Value, json};

use super::dock_model::LiveSessions;
use super::settings_network::{
    APPLY_TITLE, MASK, NetworkStop, Progress, impact_text, proposed, same, url_text,
};
use super::settings_view::{Section, stops};
use super::support_tests::{TestWorkspace, WAIT, open_workspace};
use super::view_state::{AppState, DialogState};

const SECRET: &str = "hunter2";

fn with_password() -> ProxySetting {
    ProxySetting {
        url: Some(format!("http://alice:{SECRET}@proxy.example:3128")),
        no_proxy: vec!["localhost".into(), ".corp".into()],
    }
}

#[test]
fn the_saved_password_is_masked_and_kept_until_the_url_changes() {
    let saved = with_password();
    let shown = url_text(&saved);
    assert_eq!(shown, format!("http://{MASK}@proxy.example:3128"));
    assert!(!shown.contains(SECRET) && !shown.contains("alice"));

    // Left as shown, the saved URL is kept, password included.
    let kept = proposed(&saved, &shown, "localhost").unwrap();
    assert_eq!(kept.url, saved.url);
    assert_eq!(kept.no_proxy, ["localhost"]);
    assert!(same(
        &proposed(&saved, &shown, " localhost , .corp ").unwrap(),
        &saved
    ));

    // Edited around the mask: the password must be typed again.
    let why = proposed(&saved, &format!("http://{MASK}@proxy.example:3129"), "").unwrap_err();
    assert!(why.contains("type the whole proxy URL again"), "{why}");

    // A URL without credentials is shown as it is; empty is none.
    let plain = ProxySetting {
        url: Some("socks5://10.0.0.1:1080".into()),
        no_proxy: vec![],
    };
    assert_eq!(url_text(&plain), "socks5://10.0.0.1:1080");
    assert_eq!(url_text(&ProxySetting::default()), "");
    assert_eq!(proposed(&plain, "", "").unwrap(), ProxySetting::default());
}

#[test]
fn a_proxy_is_checked_as_typed_without_quoting_its_password() {
    let saved = ProxySetting::default();
    let unsupported =
        proposed(&saved, &format!("ftp://bob:{SECRET}@x.example:21"), "").unwrap_err();
    assert!(unsupported.contains("does not support"), "{unsupported}");
    assert!(!unsupported.contains(SECRET) && !unsupported.contains("bob"));
    assert!(unsupported.starts_with('T'), "a sentence: {unsupported}");

    let unreadable = proposed(&saved, &format!("http://bob:{SECRET}@"), "").unwrap_err();
    assert!(!unreadable.contains(SECRET), "{unreadable}");

    let entry = proposed(&saved, "http://proxy:3128", "a.example, b c").unwrap_err();
    assert!(entry.contains("No-proxy entry 2"), "{entry}");

    let ok = proposed(&saved, " http://proxy:3128 ", "a.example,,10.0.0.0/8").unwrap();
    assert_eq!(ok.url.as_deref(), Some("http://proxy:3128"));
    assert_eq!(ok.no_proxy, ["a.example", "10.0.0.0/8"]);
}

#[test]
fn the_confirmation_says_what_reconnects_and_what_ends() {
    let text = impact_text(&with_password(), 2, LiveSessions { shells: 1, logs: 3 });
    assert!(text.contains("Every cluster connects again"), "{text}");
    assert!(
        text.contains("By default they go through http://proxy.example:3128, except servers matching localhost, .corp"),
        "{text}"
    );
    assert!(
        text.contains("writes waiting for review are refused"),
        "{text}"
    );
    assert!(
        text.contains("2 port-forwards keep their local ports and reconnect"),
        "{text}"
    );
    assert!(text.contains("1 shell and 3 log streams open"), "{text}");
    assert!(!text.contains(SECRET), "{text}");

    let none = impact_text(&ProxySetting::default(), 0, LiveSessions::default());
    assert!(none.contains("without a proxy of Flint's own"), "{none}");
    assert!(!none.contains("port-forward"), "{none}");
}

#[test]
fn the_report_names_what_reconnected_and_what_did_not() {
    let waiting = Progress::Reconnecting {
        waiting: vec!["kind-alpha".into()],
        forwards: 1,
    };
    assert_eq!(
        waiting.lines(),
        [
            "Applied. Reconnecting: kind-alpha…",
            "Reconnecting 1 port-forward on its local port…"
        ]
    );
    let forward = |cluster: &str, ended: Option<&str>| ForwardReconnect {
        cluster: cluster.into(),
        target: "service/web port 80 in demo".into(),
        local_port: Some(8080),
        ended: ended.map(str::to_owned),
    };
    let done = Progress::Done(ProxyReconnectReport {
        rerouted: vec!["kind-alpha".into()],
        reconnected: vec!["kind-alpha".into()],
        failed: vec![("kind-beta".into(), "the proxy refused".into())],
        forwards: vec![
            forward("kind-alpha", None),
            forward("kind-beta", Some("kind-beta did not reconnect")),
        ],
    });
    assert!(done.failed());
    assert_eq!(
        done.lines(),
        [
            "Reconnected: kind-alpha.",
            "Not reconnected: kind-beta: the proxy refused.",
            "Now reached another way: kind-alpha. Unlock, trust and keep watching them again if \
             they were.",
            "Port-forward reconnected: service/web port 80 in demo of kind-alpha on local port \
             8080.",
            "Port-forward ended: service/web port 80 in demo of kind-beta on local port 8080: \
             kind-beta did not reconnect."
        ]
    );
    let forward_ended = Progress::Done(ProxyReconnectReport {
        reconnected: vec!["kind-alpha".into()],
        forwards: vec![forward("kind-alpha", Some("kind-alpha is protected"))],
        ..ProxyReconnectReport::default()
    });
    assert!(
        forward_ended.failed(),
        "an ended forward counts as a failure"
    );
    let nothing = Progress::Done(ProxyReconnectReport::default());
    assert!(!nothing.failed());
    assert!(nothing.lines()[0].contains("No cluster was connected"));
}

#[test]
fn the_keyboard_reaches_the_fields_and_apply() {
    let ids: Vec<String> = stops(Section::Network, false, Default::default(), 0)
        .into_iter()
        .map(|stop| stop.id().to_string())
        .collect();
    for id in [
        "settings-proxy-url",
        "settings-no-proxy",
        "settings-proxy-apply",
    ] {
        assert!(ids.iter().any(|i| i == id), "{id}: {ids:?}");
    }
}

fn state(cx: &mut TestAppContext, t: &TestWorkspace) -> AppState {
    let workspace = t.workspace.clone();
    cx.update_window(t.window, |_, window, cx| {
        workspace.update(cx, |ws, cx| ws.view_state(window, cx))
    })
    .unwrap()
}

/// Opens Settings on its Network section.
fn show_network(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.update_window(t.window, |_, window, _| window.activate_window())
        .unwrap();
    t.focus_workspace(cx);
    t.press_key_of(cx, "flint::OpenSettings");
    t.dispatch(
        cx,
        "settings::ShowSettingsSection",
        json!({ "section": "network" }),
    )
    .unwrap();
}

fn field(cx: &mut TestAppContext, t: &TestWorkspace, stop: NetworkStop) -> String {
    t.workspace
        .read_with(cx, |ws, cx| ws.network_field_text(stop, cx))
}

async fn shown_dialog(cx: &mut TestAppContext, t: &TestWorkspace) -> DialogState {
    cx.wait_for(t.window, WAIT, |window, cx| {
        super::confirm::any_open(window, cx)
    })
    .await;
    state(cx, t).dialog.expect("a dialog is open")
}

async fn no_dialog(cx: &mut TestAppContext, t: &TestWorkspace) {
    cx.wait_for(t.window, WAIT, |window, cx| {
        !super::confirm::any_open(window, cx)
    })
    .await;
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(1));
    cx.run_until_parked();
}

fn write_settings(dir: &Path) {
    std::fs::write(
        dir.join("settings.toml"),
        format!(
            "[proxy]\nurl = \"http://alice:{SECRET}@proxy.example:3128\"\nno_proxy = \
             [\"localhost\", \".corp\"]\n"
        ),
    )
    .unwrap();
}

// The saved URL's password shows nowhere: not in the field, the section, the confirmation or
// what automation reads.
#[gpui_kit::test]
async fn the_saved_proxy_is_shown_masked(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    write_settings(dir.path());
    let t = open_workspace(cx, dir.path()).await;
    show_network(cx, &t);

    let masked = format!("http://{MASK}@proxy.example:3128");
    assert_eq!(field(cx, &t, NetworkStop::Url), masked);
    // Its text already hides the password, so the field shows it.
    assert!(
        !t.workspace
            .read_with(cx, |ws, cx| ws.network_url_masked(cx))
    );
    assert_eq!(field(cx, &t, NetworkStop::NoProxy), "localhost, .corp");
    assert_eq!(
        t.label(cx, "settings-proxy-saved").as_deref(),
        Some("Saved: http://proxy.example:3128 (credentials hidden)")
    );
    let s = state(cx, &t);
    assert_eq!(s.settings.network.saved_url, masked);
    assert!(!s.settings.network.can_apply, "nothing changed");
    assert!(
        !t.available(cx).contains(&"settings::ApplyProxy"),
        "Apply is not offered for the saved proxy"
    );

    // Only the no-proxy list changes: the saved URL, password included, is what applies.
    t.dispatch(
        cx,
        "settings::EditNoProxy",
        json!({ "no_proxy": "localhost" }),
    )
    .unwrap();
    assert!(state(cx, &t).settings.network.can_apply);
    t.click(cx, "settings-proxy-apply");
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, APPLY_TITLE);
    assert!(
        dialog.text.contains("through http://proxy.example:3128"),
        "{}",
        dialog.text
    );
    let everything = serde_json::to_string(&state(cx, &t)).unwrap();
    assert!(!everything.contains(SECRET), "{everything}");
    assert!(!everything.contains("alice"), "{everything}");
    t.click(cx, "cancel");
    no_dialog(cx, &t).await;
}

// A URL that cannot be used says why under the field, and Apply is not offered; a valid one
// asks first, and Enter or Cancel apply nothing.
#[gpui_kit::test]
async fn apply_asks_first_and_cancel_applies_nothing(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_network(cx, &t);
    assert_eq!(field(cx, &t, NetworkStop::Url), "");

    t.dispatch(
        cx,
        "settings::EditProxyUrl",
        json!({ "url": format!("ftp://bob:{SECRET}@x.example:21") }),
    )
    .unwrap();
    let s = state(cx, &t);
    let why = s.settings.network.error.clone().expect("an error");
    assert!(why.contains("does not support"), "{why}");
    assert!(!why.contains(SECRET));
    assert!(!s.settings.network.can_apply);
    let shown = t
        .label(cx, "settings-proxy-error")
        .expect("the error is drawn");
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(!t.available(cx).contains(&"settings::ApplyProxy"));
    // Nothing applies around the guard either.
    t.dispatch(cx, "settings::ApplyProxy", Value::Null).ok();
    assert!(!t.has_confirmation(cx));

    t.dispatch(
        cx,
        "settings::EditProxyUrl",
        json!({ "url": format!("http://bob:{SECRET}@127.0.0.1:9") }),
    )
    .unwrap();
    let s = state(cx, &t);
    assert_eq!(s.settings.network.error, None);
    assert!(s.settings.network.can_apply && s.settings.network.typed);
    // A password typed in is masked in the field.
    assert!(
        t.workspace
            .read_with(cx, |ws, cx| ws.network_url_masked(cx))
    );
    // The Settings draft is not involved: nothing to Save.
    assert!(!s.settings.dirty);

    let before = std::fs::read_to_string(dir.path().join("settings.toml")).unwrap_or_default();
    t.dispatch(cx, "settings::ApplyProxy", Value::Null).unwrap();
    let dialog = shown_dialog(cx, &t).await;
    assert_eq!(dialog.title, APPLY_TITLE);
    assert!(
        dialog.text.contains("through http://127.0.0.1:9"),
        "{}",
        dialog.text
    );
    assert!(!dialog.text.contains(SECRET), "{}", dialog.text);
    // Enter cancels this one.
    t.press(cx, "enter");
    no_dialog(cx, &t).await;
    assert_eq!(state(cx, &t).settings.network.report, Vec::<String>::new());

    t.click(cx, "settings-proxy-apply");
    shown_dialog(cx, &t).await;
    t.click(cx, "cancel");
    no_dialog(cx, &t).await;
    let after = std::fs::read_to_string(dir.path().join("settings.toml")).unwrap_or_default();
    assert_eq!(after, before, "Cancel applies nothing");
    assert!(
        t.workspace
            .read_with(cx, |ws, _| ws.network_progress())
            .is_none()
    );

    // Revert drops what was typed, with no other edit.
    t.dispatch(cx, "settings::RevertSettings", Value::Null)
        .unwrap();
    cx.run_until_parked();
    assert_eq!(field(cx, &t, NetworkStop::Url), "");
    assert!(!state(cx, &t).settings.network.typed);
    assert!(
        !t.workspace
            .read_with(cx, |ws, cx| ws.network_url_masked(cx))
    );
}

// covers #857: a proxy saved on disk while the confirmation was open, and not reported yet, is
// not overwritten: Apply is refused, nothing reconnects, and the fields show the file's proxy.
#[gpui_kit::test]
async fn apply_over_a_proxy_changed_on_disk_is_refused_and_shows_it(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    show_network(cx, &t);
    t.dispatch(
        cx,
        "settings::EditProxyUrl",
        json!({ "url": "http://127.0.0.1:9" }),
    )
    .unwrap();
    t.dispatch(cx, "settings::ApplyProxy", Value::Null).unwrap();
    shown_dialog(cx, &t).await;

    // Saved meanwhile, outside this window, which has not heard of it.
    write_settings(dir.path());
    let newer = t.settings_toml();
    t.click(cx, "ok");
    no_dialog(cx, &t).await;

    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        matches!(
            workspace.read(cx).network_progress(),
            Some(Progress::Failed(_))
        )
    })
    .await;
    let Some(Progress::Failed(why)) = t.workspace.read_with(cx, |ws, _| ws.network_progress())
    else {
        unreachable!("waited for a refusal")
    };
    assert!(
        why.contains(flint_core::settings::PROXY_CHANGED_SINCE_REVIEW),
        "{why}"
    );
    assert!(!why.contains(SECRET), "{why}");
    assert_eq!(t.settings_toml(), newer, "nothing was written");

    let workspace = t.workspace.clone();
    let shown = url_text(&with_password());
    let want = shown.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).network_field_text(NetworkStop::Url, cx) == want
    })
    .await;
    assert_eq!(field(cx, &t, NetworkStop::Url), shown);
    let s = state(cx, &t);
    assert!(!s.settings.network.typed && !s.settings.network.can_apply);
    let everything = serde_json::to_string(&s).unwrap();
    assert!(!everything.contains(SECRET), "{everything}");
}

const APPLY_CHILD: &str = "FLINT_TEST_PROXY_APPLY_CHILD";

// Apply saves the proxy, reconnects the selected cluster on it and reports how it came back;
// the field then shows the URL masked. Run in a child: the proxy is the whole process's.
#[test]
fn apply_reconnects_on_the_new_proxy_and_reports() {
    let (_, module) = module_path!().split_once("::").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([&format!("{module}::apply_child"), "--exact", "--nocapture"])
        .env(APPLY_CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout.contains("1 passed"),
        "the child ran no test: {stdout}"
    );
}

#[gpui_kit::test]
async fn apply_child(cx: &mut TestAppContext) {
    if std::env::var_os(APPLY_CHILD).is_none() {
        return;
    }
    // A port nothing listens on: the proxy refuses every connection.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = tempfile::tempdir().unwrap();
    let t = open_workspace(cx, dir.path()).await;
    t.activate(cx, "kind-alpha").await;
    show_network(cx, &t);

    let url = format!("http://user:{SECRET}@127.0.0.1:{port}");
    t.dispatch(cx, "settings::EditProxyUrl", json!({ "url": url }))
        .unwrap();
    t.dispatch(cx, "settings::ApplyProxy", Value::Null).unwrap();
    shown_dialog(cx, &t).await;
    t.click(cx, "ok");
    no_dialog(cx, &t).await;

    let workspace = t.workspace.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        matches!(
            workspace.read(cx).network_progress(),
            Some(Progress::Done { .. } | Progress::Failed(_))
        )
    })
    .await;
    let Some(Progress::Done(report)) = t.workspace.read_with(cx, |ws, _| ws.network_progress())
    else {
        panic!("applied and reported");
    };
    // kind-alpha was selected: it reconnected through the proxy, which refused it.
    assert!(report.reconnected.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].0, "kind-alpha");
    assert!(
        report.rerouted.iter().any(|c| c == "kind-alpha"),
        "{report:?}"
    );

    let stored = t.settings_toml();
    assert!(stored.contains(&format!("127.0.0.1:{port}")), "{stored}");
    let masked = format!("http://{MASK}@127.0.0.1:{port}");
    let workspace = t.workspace.clone();
    let want = masked.clone();
    cx.wait_for(t.window, WAIT, move |_, cx| {
        workspace.read(cx).network_field_text(NetworkStop::Url, cx) == want
    })
    .await;
    let s = state(cx, &t);
    assert!(!s.settings.network.can_apply && !s.settings.network.typed);
    assert!(
        s.settings
            .network
            .report
            .iter()
            .any(|line| line.starts_with("Not reconnected: kind-alpha")),
        "{:?}",
        s.settings.network.report
    );
    let everything = serde_json::to_string(&s).unwrap();
    assert!(!everything.contains(SECRET), "{everything}");
}

// A proxy applied while the draft holds edits is not a change on disk under the draft: Save
// leaves the proxy alone.
#[test]
fn an_applied_proxy_does_not_show_the_on_disk_banner() {
    use std::sync::Arc;

    use super::settings_draft::{Change, SettingsDraft};

    let file = Arc::new(flint_core::Settings::default());
    let mut draft = SettingsDraft::default();
    draft.edit(Change::LogBufferMib(32), &file);
    let mut applied = (*file).clone();
    applied.proxy = with_password();
    assert!(!draft.changed_on_disk(&applied));
    applied.desktop_notifications = !applied.desktop_notifications;
    assert!(draft.changed_on_disk(&applied));
}
