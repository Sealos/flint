//! UP-15: credentials and Secret values in what a full MCP call returns.

use anyhow::Result;
use flint_e2e::fake_api::Gvr;
use serde_json::{Value, json};

use crate::support::{EXPOSED, NAMESPACE, POD, Setup};

const TOKEN: &str = "placeholder-token-value";
const PASSWORD: &str = "placeholder-password";
const SECRET_VALUE: &str = "placeholder-secret-value";
const KEY_BODY: &str = "PlaceholderKeyBody0123456789abcdefPlaceholderKeyBody0123456789ab";

/// Built at run time, so the source holds nothing shaped like an access key id.
fn cloud_key() -> String {
    format!("{}{}", "AKIA", "PLACEHOLDER00000")
}

fn private_key(body_lines: usize) -> String {
    let mut key = "-----BEGIN PRIVATE KEY-----\n".to_owned();
    for _ in 0..body_lines {
        key.push_str(KEY_BODY);
        key.push('\n');
    }
    key.push_str("-----END PRIVATE KEY-----\n");
    key
}

/// A line each for a token, a password pair and a cloud key, then a private key.
fn credential_lines() -> String {
    format!(
        "GET /api Authorization: Bearer {TOKEN} from 10.0.0.1\n\
         login password={PASSWORD} user=app\n\
         using key {} for s3\n\
         {}",
        cloud_key(),
        private_key(3)
    )
}

const REDACTED_LINES: &str = "GET /api Authorization: Bearer [REDACTED:authorization] from 10.0.0.1\n\
     login password=[REDACTED:password] user=app\n\
     using key [REDACTED:aws-access-key-id] for s3\n\
     -----BEGIN PRIVATE KEY-----\n\
     [REDACTED:private-key]\n\
     -----END PRIVATE KEY-----\n";

fn assert_no_credential(text: &str) {
    for value in [TOKEN, PASSWORD, SECRET_VALUE, KEY_BODY, &cloud_key()] {
        assert!(!text.contains(value), "a placeholder was returned: {text}");
    }
}

fn logs(extra: Value) -> Value {
    let mut args = json!({"cluster": EXPOSED, "namespace": NAMESPACE, "pod": POD});
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    args
}

/// Adds an Event with `message`, as a table row and as the object behind it.
fn add_event(setup: &Setup, events: &Gvr, name: &str, message: &str) {
    let row = setup
        .api
        .new_row(events, Some(NAMESPACE), name, &[("Message", message)]);
    setup.api.set_row(events, row);
    setup.api.set_object(
        events,
        json!({
            "apiVersion": "v1",
            "kind": "Event",
            "metadata": {"name": name, "namespace": NAMESPACE},
            "involvedObject": {"kind": "Pod", "name": POD, "namespace": NAMESPACE},
            "reason": "Failed",
            "message": message,
            "type": "Warning",
            "count": 1,
        }),
    );
}

/// The first message in a `get_events` reply that starts with `message_start`.
fn event_message<'a>(reply: &'a Value, message_start: &str) -> &'a str {
    reply["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["message"].as_str())
        .find(|m| m.starts_with(message_start))
        .unwrap_or_else(|| panic!("no message starting {message_start:?}: {reply}"))
}

/// The cells of the row `name` in a `list_resources` reply, joined.
fn row_text(reply: &Value, name: &str) -> String {
    let row = reply["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("no row {name}: {reply}"));
    row["cells"].to_string()
}

// covers UP-15: a token, a password pair, a cloud key and a private key in a log are each
// replaced and counted, and the rest of each line is unchanged.
#[test]
fn up_15_credentials_in_a_log_are_replaced_and_counted() -> Result<()> {
    let setup = Setup::exposed()?;
    let log = format!("starting\n{}listening on :80\n", credential_lines());
    setup.api.set_log(NAMESPACE, POD, &log);
    let mut mcp = setup.mcp()?;

    let got = mcp.call_json("get_pod_logs", logs(json!({})))?;
    assert_eq!(
        got["log"],
        format!("starting\n{REDACTED_LINES}listening on :80\n"),
        "{got}"
    );
    assert_eq!(got["redacted"], 4, "{got}");
    assert_eq!(got["truncated"], false, "{got}");
    assert_no_credential(&got.to_string());
    Ok(())
}

// covers UP-15: the same credentials in an Event, through each tool that can return it.
#[test]
fn up_15_credentials_in_an_event_are_replaced_and_counted() -> Result<()> {
    let setup = Setup::exposed()?;
    let events = setup.api.gvr("events");
    let message = format!("pull failed\n{}", credential_lines());
    add_event(&setup, &events, "web.creds", &message);
    let expected = format!("pull failed\n{REDACTED_LINES}");
    let mut mcp = setup.mcp()?;

    let got = mcp.call_json(
        "get_events",
        json!({"cluster": EXPOSED, "namespace": NAMESPACE, "object_name": POD}),
    )?;
    assert_eq!(event_message(&got, "pull failed"), expected, "{got}");
    assert_eq!(got["redacted"], 4, "{got}");
    assert_no_credential(&got.to_string());

    let listed = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "events", "namespace": NAMESPACE}),
    )?;
    assert_eq!(listed["redacted"], 4, "{listed}");
    assert!(
        row_text(&listed, "web.creds").contains("[REDACTED:private-key]"),
        "{listed}"
    );
    assert_no_credential(&listed.to_string());

    let object = mcp.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "event", "namespace": NAMESPACE, "name": "web.creds"}),
    )?;
    assert!(!object.is_error, "{}", object.text);
    assert!(object.text.starts_with("# redacted=4"), "{}", object.text);
    assert_no_credential(&object.text);
    Ok(())
}

/// A Secret manifest as JSON and as YAML.
fn secret_manifests() -> [String; 2] {
    [
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": "held"},
            "data": {"value": SECRET_VALUE},
        })
        .to_string(),
        format!(
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: held\ndata:\n  value: {SECRET_VALUE}\n"
        ),
    ]
}

// covers UP-15: a Secret held as JSON or YAML in a list cell is masked, and the cells beside it
// are unchanged.
#[test]
fn up_15_a_secret_held_in_a_list_cell_is_masked() -> Result<()> {
    let setup = Setup::exposed()?;
    let events = setup.api.gvr("events");
    let plain = "Back-off pulling image example.io/app:1.0";
    add_event(&setup, &events, "plain", plain);
    let [as_json, as_yaml] = secret_manifests();
    add_event(&setup, &events, "held-json", &as_json);
    add_event(&setup, &events, "held-yaml", &as_yaml);
    let mut mcp = setup.mcp()?;

    let listed = mcp.call_json(
        "list_resources",
        json!({"cluster": EXPOSED, "kind": "events", "namespace": NAMESPACE}),
    )?;
    assert_no_credential(&listed.to_string());
    assert!(row_text(&listed, "plain").contains(plain), "{listed}");
    assert!(
        row_text(&listed, "held-json").contains("Secret"),
        "{listed}"
    );
    Ok(())
}

// covers UP-15: a Secret held as JSON or YAML in an event message is masked, and other messages
// are unchanged.
#[test]
fn up_15_a_secret_held_in_an_event_message_is_masked() -> Result<()> {
    let setup = Setup::exposed()?;
    let events = setup.api.gvr("events");
    let plain = "Back-off pulling image example.io/app:1.0";
    add_event(&setup, &events, "plain", plain);
    let [as_json, as_yaml] = secret_manifests();
    add_event(&setup, &events, "held-json", &as_json);
    add_event(&setup, &events, "held-yaml", &as_yaml);
    let mut mcp = setup.mcp()?;

    let got = mcp.call_json(
        "get_events",
        json!({"cluster": EXPOSED, "namespace": NAMESPACE, "object_name": POD}),
    )?;
    assert_no_credential(&got.to_string());
    assert_eq!(event_message(&got, "Back-off"), plain, "{got}");
    let masked = got["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["message"].as_str().unwrap_or("").contains("<redacted>"))
        .count();
    assert_eq!(masked, 2, "{got}");
    Ok(())
}

// covers UP-15: a Secret nested in another object, or held as text in its annotations, is masked
// through `get_resource`.
#[test]
fn up_15_a_secret_nested_in_an_object_is_masked() -> Result<()> {
    let setup = Setup::exposed()?;
    let [as_json, as_yaml] = secret_manifests();
    setup.api.set_object(
        &setup.api.gvr("pods"),
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "holder",
                "namespace": NAMESPACE,
                "annotations": {
                    "example.io/json": as_json,
                    "example.io/yaml": as_yaml,
                    "example.io/plain": "kept as written",
                },
            },
            "spec": {
                "containers": [{"name": "web", "image": "example.io/app:1.0"}],
                "nested": {"resources": [{
                    "apiVersion": "v1",
                    "kind": "Secret",
                    "data": {"value": SECRET_VALUE},
                }]},
            },
        }),
    );
    let mut mcp = setup.mcp()?;

    let got = mcp.call(
        "get_resource",
        json!({"cluster": EXPOSED, "kind": "pod", "namespace": NAMESPACE, "name": "holder"}),
    )?;
    assert!(!got.is_error, "{}", got.text);
    assert_no_credential(&got.text);
    assert!(got.text.contains("kept as written"), "{}", got.text);
    assert!(got.text.contains("example.io/app:1.0"), "{}", got.text);
    Ok(())
}

// covers UP-15: a private key that opens before the requested tail is redacted as a block, and
// the count covers only the lines returned.
#[test]
fn up_15_a_key_opening_before_the_tail_stays_redacted() -> Result<()> {
    let setup = Setup::exposed()?;
    let log = format!(
        "login password={PASSWORD}\n{}after one\nafter two\n",
        private_key(8)
    );
    setup.api.set_log(NAMESPACE, POD, &log);
    let mut mcp = setup.mcp()?;

    let got = mcp.call_json("get_pod_logs", logs(json!({"tail_lines": 5})))?;
    let text = got["log"].as_str().unwrap();
    assert_no_credential(text);
    assert!(!text.contains("BEGIN PRIVATE KEY"), "{text}");
    assert!(text.ends_with("after one\nafter two\n"), "{text}");
    assert_eq!(got["truncated"], true, "{got}");
    assert_eq!(got["redacted"], 1, "{got}");
    Ok(())
}

// covers UP-15: when a key opens further back than the server looks, its body is left out rather
// than returned without its label.
#[test]
fn up_15_a_key_opening_past_the_look_behind_is_left_out() -> Result<()> {
    let setup = Setup::exposed()?;
    let log = format!("{}after one\nafter two\n", private_key(400));
    setup.api.set_log(NAMESPACE, POD, &log);
    let mut mcp = setup.mcp()?;

    let got = mcp.call_json("get_pod_logs", logs(json!({"tail_lines": 10})))?;
    let text = got["log"].as_str().unwrap();
    assert_no_credential(text);
    assert!(text.ends_with("after one\nafter two\n"), "{text}");
    assert_eq!(got["truncated"], true, "{got}");
    Ok(())
}

// covers UP-15: the items of a cookie array that opens further back than the server looks are
// left out too.
#[test]
fn up_15_cookie_items_past_the_look_behind_are_left_out() -> Result<()> {
    let setup = Setup::exposed()?;
    let mut log = "\"set-cookie\": [\n".to_owned();
    for i in 0..400 {
        log.push_str(&format!("  \"item{i}={SECRET_VALUE}; Path=/\",\n"));
    }
    log.push_str(&format!("  \"last={SECRET_VALUE}\"\n],\nafter\n"));
    setup.api.set_log(NAMESPACE, POD, &log);
    let mut mcp = setup.mcp()?;

    let got = mcp.call_json("get_pod_logs", logs(json!({"tail_lines": 10})))?;
    let text = got["log"].as_str().unwrap();
    assert_no_credential(text);
    assert!(text.ends_with("after\n"), "{text}");
    assert_eq!(got["truncated"], true, "{got}");
    Ok(())
}
