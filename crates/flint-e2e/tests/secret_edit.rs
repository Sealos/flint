//! Editing a Secret whose values the editor shows masked (#394), against the fake API server.
//! A mask left in place keeps the value on the cluster: the write is a merge patch of only what
//! changed, bound to the reviewed `uid` and `resourceVersion`, and never carries a mask.

use std::time::Duration;

use flint_core::KindId;
use flint_core::Settings;
use flint_core::WatchMode;
use flint_core::WatchTiming;
use flint_core::handle::{Command, ConnectionState, CoreConfig, CoreHandle, Event};
use flint_core::secret_edit::MASK;
use flint_core::write::{WriteAction, WriteError, WriteProposal, WriteTarget};
use flint_e2e::fake_api::{CONTEXT, FakeApiServer, Fault, Gvr, Verb, WriteRecord};
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(30);
const NAME: &str = "db";
const NAMESPACE: &str = "demo";

const OLD: &str = "b2xkLXNlY3JldA=="; // old-secret
const KEEP: &str = "a2VlcC1tZQ=="; // keep-me
const GONE: &str = "Z29uZQ=="; // gone
const NEW: &str = "bmV3LXNlY3JldA=="; // new-secret
const TOKEN: &str = "dG9rZW4tdmFsdWU="; // token-value
const COPY: &str = "{\"data\":{\"password\":\"b2xkLXNlY3JldA==\"}}";

struct Core {
    handle: CoreHandle,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(server: &FakeApiServer) -> Core {
        let dir = tempfile::tempdir().unwrap();
        let kubeconfig = dir.path().join("kubeconfig");
        server
            .write_kubeconfig(&kubeconfig, Some(NAMESPACE))
            .unwrap();
        let handle = CoreHandle::start(CoreConfig {
            config_dir: dir.path().join("config"),
            settings: Settings {
                watch_mode: WatchMode::OnView,
                ..Settings::default()
            },
            settings_error: None,
            kubeconfig_base: vec![kubeconfig],
            home: None,
            watch_files: false,
            watch_timing: WatchTiming {
                backoff_min: Duration::from_millis(10),
                backoff_max: Duration::from_millis(50),
                denied_backoff_min: Duration::from_millis(10),
            },
        })
        .unwrap();
        let events = handle.events();
        handle.send(Command::Activate(CONTEXT.to_owned()));
        tokio::time::timeout(WAIT, async {
            loop {
                match events.recv().await.expect("core running") {
                    Event::ConnectionChanged {
                        state: ConnectionState::Failed(e),
                        ..
                    } => panic!("connection failed: {e}"),
                    Event::DiscoveryChanged {
                        discovery: Some(_), ..
                    } => return,
                    _ => {}
                }
            }
        })
        .await
        .expect("discovery in time");
        Core { handle, _dir: dir }
    }
}

fn secrets() -> Gvr {
    Gvr::core("secrets")
}

/// The Secret as the detail pane opened it.
fn target(server: &FakeApiServer) -> WriteTarget {
    WriteTarget {
        cluster: CONTEXT.into(),
        server: Some(server.url()),
        kind: KindId::new("", "secrets"),
        namespace: Some(NAMESPACE.to_owned()),
        name: NAME.to_owned(),
        uid: Some(meta(server, "uid")),
        resource_version: Some(meta(server, "resourceVersion")),
    }
}

/// The stored Secret's `metadata.<field>`.
fn meta(server: &FakeApiServer, field: &str) -> String {
    stored(server)["metadata"][field]
        .as_str()
        .expect("set")
        .to_owned()
}

fn live() -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": NAME,
            "namespace": NAMESPACE,
            "uid": "uid-db",
            "resourceVersion": "5000",
            "annotations": {"kubectl.kubernetes.io/last-applied-configuration": COPY},
        },
        "type": "Opaque",
        "data": {"password": OLD, "user": KEEP, "old": GONE},
    })
}

/// The editor's text as opened, every value masked. The core binds it to the target's version.
fn opened() -> String {
    format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {NAME}\n  namespace: {NAMESPACE}\n  \
         annotations:\n    \
         kubectl.kubernetes.io/last-applied-configuration: {MASK}\ntype: Opaque\ndata:\n  \
         password: {MASK}\n  user: {MASK}\n  old: {MASK}\n"
    )
}

/// Password typed over, `token` added, `old` removed; `user` and the annotation left masked.
fn edited() -> String {
    opened()
        .replace(&format!("password: {MASK}"), &format!("password: {NEW}"))
        .replace(&format!("  old: {MASK}\n"), &format!("  token: {TOKEN}\n"))
}

fn stored(server: &FakeApiServer) -> Value {
    server
        .object(&secrets(), Some(NAMESPACE), NAME)
        .expect("the Secret")
}

fn secret_writes(server: &FakeApiServer) -> Vec<WriteRecord> {
    server
        .writes()
        .into_iter()
        .filter(|w| w.is_about(&secrets()))
        .collect()
}

/// No write carries a mask, a review placeholder or a value the edit did not change.
fn assert_clean(writes: &[WriteRecord]) {
    for write in writes {
        let body = String::from_utf8_lossy(&write.body);
        for hidden in [MASK, "<hidden", KEEP, OLD, GONE, "last-applied"] {
            assert!(!body.contains(hidden), "{hidden} sent in {body}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edit_sends_only_what_changed_and_keeps_masked_values() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&secrets(), live());
    let core = Core::start(&server).await;
    let (uid, version) = (meta(&server, "uid"), meta(&server, "resourceVersion"));

    let review = core
        .handle
        .review_write(target(&server), WriteProposal::ApplyYaml { yaml: edited() })
        .await
        .unwrap();
    let WriteAction::ApplyYaml(edit) = review.action() else {
        panic!("not an edit");
    };
    for text in [edit.before(), edit.after()] {
        for value in [OLD, KEEP, GONE, NEW, TOKEN, MASK, "b2xk"] {
            assert!(!text.contains(value), "{value} shown in {text}");
        }
    }
    assert!(edit.after().contains("token"), "{}", edit.after());
    assert!(
        edit.after().contains("password: <hidden, changed>"),
        "{}",
        edit.after()
    );
    assert!(!edit.after().contains("old:"), "{}", edit.after());
    core.handle.run_write(review.into_intent()).await.unwrap();

    let writes = secret_writes(&server);
    assert_clean(&writes);
    assert!(writes.iter().all(|w| w.method == http::Method::PATCH));
    assert!(
        writes
            .iter()
            .all(|w| w.content_type == "application/merge-patch+json")
    );
    let sent: Vec<Value> = writes
        .iter()
        .filter(|w| !w.is_dry_run())
        .map(|w| w.json().unwrap())
        .collect();
    assert_eq!(
        sent,
        vec![json!({
            "metadata": {"uid": uid, "resourceVersion": version},
            "data": {"password": NEW, "token": TOKEN, "old": null},
        })]
    );
    let written = stored(&server);
    assert_eq!(
        written["data"],
        json!({"password": NEW, "user": KEEP, "token": TOKEN})
    );
    assert_eq!(
        written["metadata"]["annotations"]["kubectl.kubernetes.io/last-applied-configuration"],
        COPY
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_changed_after_review_is_a_conflict_and_nothing_is_written() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&secrets(), live());
    let core = Core::start(&server).await;
    let review = core
        .handle
        .review_write(target(&server), WriteProposal::ApplyYaml { yaml: edited() })
        .await
        .unwrap();
    let reviewed = meta(&server, "resourceVersion");
    let mut rotated = stored(&server);
    rotated["metadata"]["resourceVersion"] = format!("{reviewed}9").into();
    rotated["data"]["user"] = "cm90YXRlZA==".into();
    server.set_object(&secrets(), rotated.clone());
    assert_ne!(meta(&server, "resourceVersion"), reviewed);

    let error = core
        .handle
        .run_write(review.into_intent())
        .await
        .unwrap_err();

    assert_eq!(error, WriteError::Conflict);
    let writes = secret_writes(&server);
    assert!(writes.iter().all(WriteRecord::is_dry_run), "{writes:?}");
    assert_eq!(stored(&server)["data"], rotated["data"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mask_with_no_value_to_keep_is_refused_before_any_request() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&secrets(), live());
    let core = Core::start(&server).await;
    let yaml = opened().replace(
        &format!("  old: {MASK}\n"),
        &format!("  old: {MASK}\n  renamed: {MASK}\n"),
    );

    let error = core
        .handle
        .review_write(target(&server), WriteProposal::ApplyYaml { yaml })
        .await
        .unwrap_err();

    let WriteError::InvalidYaml(why) = &error else {
        panic!("not refused: {error:?}");
    };
    assert!(why.contains("renamed"), "{why}");
    assert!(secret_writes(&server).is_empty());
    assert_eq!(stored(&server)["data"], live()["data"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_value_is_never_quoted() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&secrets(), live());
    let core = Core::start(&server).await;
    let yaml = opened().replace(&format!("password: {MASK}"), "password: plain-text-secret");

    let error = core
        .handle
        .review_write(target(&server), WriteProposal::ApplyYaml { yaml })
        .await
        .unwrap_err();

    let text = error.to_string();
    assert!(text.contains("base64"), "{text}");
    assert!(!text.contains("plain-text-secret"), "{text}");
    assert!(secret_writes(&server).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_webhook_echoing_a_value_is_reported_without_its_message() {
    let server = FakeApiServer::start().await.unwrap();
    server.set_object(&secrets(), live());
    let core = Core::start(&server).await;
    server.inject(
        Fault::admission_denied(
            "policy.example.com",
            "password new-secret and token-value are not allowed",
        )
        .on(&secrets())
        .verb(Verb::Other)
        .times(1),
    );

    let error = core
        .handle
        .review_write(target(&server), WriteProposal::ApplyYaml { yaml: edited() })
        .await
        .unwrap_err();

    let text = error.to_string();
    assert!(matches!(error, WriteError::Rejected(_)), "{error:?}");
    assert!(text.contains("\"policy.example.com\""), "{text}");
    for value in ["new-secret", "token-value", "not allowed", NEW, TOKEN] {
        assert!(!text.contains(value), "{value} in {text}");
    }
    assert!(secret_writes(&server).iter().all(WriteRecord::is_dry_run));
    assert_eq!(stored(&server)["data"], live()["data"]);
}
