use kube::api::ApiResource;
use serde_json::json;

use super::*;

fn target() -> WriteTarget {
    WriteTarget {
        cluster: "dev".into(),
        server: Some("https://example.invalid:6443".to_owned()),
        kind: KindId::new("apps", "deployments"),
        namespace: Some("demo".to_owned()),
        name: "web".to_owned(),
        uid: Some("uid-1".to_owned()),
        resource_version: Some("10".to_owned()),
    }
}

fn kind() -> KindInfo {
    KindInfo {
        resource: ApiResource {
            group: "apps".into(),
            version: "v1".into(),
            api_version: "apps/v1".into(),
            kind: "Deployment".into(),
            plural: "deployments".into(),
        },
        singular: "deployment".into(),
        short_names: Vec::new(),
        scale: Default::default(),
        namespaced: true,
        verbs: Vec::new(),
    }
}

const EDIT: &str =
    "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: 2\n";

#[test]
fn an_edit_is_pinned_to_the_reviewed_object_and_version() {
    let body = edit_body(EDIT, &kind(), &target()).unwrap();
    assert_eq!(body["metadata"]["namespace"], "demo");
    assert_eq!(body["metadata"]["uid"], "uid-1");
    assert_eq!(body["metadata"]["resourceVersion"], "10");
    assert_eq!(body["spec"]["replicas"], 2);
}

#[test]
fn an_edit_cannot_change_which_object_it_is() {
    for (from, to, field) in [
        ("name: web", "name: other", "name"),
        ("kind: Deployment", "kind: StatefulSet", "kind"),
        ("apps/v1", "apps/v2", "apiVersion"),
        ("name: web", "name: web\n  namespace: prod", "namespace"),
        ("name: web", "name: web\n  uid: uid-2", "uid"),
    ] {
        let yaml = EDIT.replace(from, to);
        assert_eq!(
            edit_body(&yaml, &kind(), &target()).unwrap_err(),
            WriteError::IdentityChanged(field),
            "{field}"
        );
    }
}

#[test]
fn an_edit_at_another_version_is_a_conflict() {
    let yaml = EDIT.replace("name: web", "name: web\n  resourceVersion: \"9\"");
    assert_eq!(
        edit_body(&yaml, &kind(), &target()).unwrap_err(),
        WriteError::Conflict
    );
}

#[test]
fn text_that_is_not_an_object_is_refused() {
    for yaml in ["- a\n- b\n", "just text", "a: [unclosed"] {
        assert!(
            matches!(
                edit_body(yaml, &kind(), &target()),
                Err(WriteError::InvalidYaml(_) | WriteError::IdentityChanged(_))
            ),
            "{yaml}"
        );
    }
}

#[test]
fn a_review_binds_to_the_live_object() {
    let live = json!({"metadata": {"uid": "uid-1", "resourceVersion": "10"}});
    let unset = WriteTarget {
        uid: None,
        resource_version: None,
        ..target()
    };
    assert_eq!(bound_to_live(unset, &live).unwrap(), target());
    let replaced = json!({"metadata": {"uid": "uid-2", "resourceVersion": "10"}});
    assert_eq!(
        bound_to_live(target(), &replaced).unwrap_err(),
        WriteError::Replaced
    );
    let newer = json!({"metadata": {"uid": "uid-1", "resourceVersion": "11"}});
    assert_eq!(
        bound_to_live(target(), &newer).unwrap_err(),
        WriteError::Conflict
    );
}

fn reviewed() -> ReviewedEdit {
    ReviewedEdit {
        bound: target(),
        pin: crate::recheck::ContextHistory::default().pin("dev", None),
        credential: crate::credential::CredentialBinding::for_tests(),
        body: json!({"data": {"password": "aHVudGVyMg=="}}),
        before: String::new(),
        after: String::new(),
    }
}

#[test]
fn a_review_is_refused_for_any_other_target() {
    let intent = |target| WriteIntent {
        target,
        action: WriteAction::ApplyYaml(reviewed()),
    };
    assert_eq!(check_bound(&intent(target())), Ok(()));
    let others = [
        WriteTarget {
            cluster: "prod".into(),
            ..target()
        },
        WriteTarget {
            server: Some("https://other.invalid".to_owned()),
            ..target()
        },
        WriteTarget {
            kind: KindId::new("apps", "statefulsets"),
            ..target()
        },
        WriteTarget {
            namespace: Some("prod".to_owned()),
            ..target()
        },
        WriteTarget {
            name: "other".to_owned(),
            ..target()
        },
        WriteTarget {
            uid: Some("uid-2".to_owned()),
            ..target()
        },
        WriteTarget {
            resource_version: Some("11".to_owned()),
            ..target()
        },
    ];
    for other in others {
        assert_eq!(
            check_bound(&intent(other.clone())),
            Err(WriteError::ReviewMismatch),
            "{other:?}"
        );
    }
}

#[test]
fn a_reviewed_edit_does_not_print_its_body() {
    let text = format!("{:?}", reviewed());
    assert!(!text.contains("aHVudGVyMg"), "{text}");
}

#[test]
fn a_review_shows_no_secret_values_status_or_managed_fields() {
    let secret = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": "s", "managedFields": [{"manager": "x"}]},
        "data": {"password": "aHVudGVyMg=="},
        "status": {"phase": "x"},
    });
    let shown = shown_yaml(&secret).unwrap();
    assert!(!shown.contains("aHVudGVyMg"), "{shown}");
    assert!(shown.contains("password"), "{shown}");
    assert!(
        !shown.contains("managedFields") && !shown.contains("status"),
        "{shown}"
    );
}

#[test]
fn a_rotated_secret_value_shows_as_changed_without_its_value() {
    let secret = |password: &str| {
        json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": {"name": "s"},
            "data": {"password": password, "user": "YWRtaW4="},
        })
    };
    let shown = shown_after(&secret("b2xk"), &secret("bmV3")).unwrap();
    assert!(shown.contains(HIDDEN_CHANGED), "{shown}");
    assert_eq!(shown.matches(HIDDEN_CHANGED).count(), 1, "{shown}");
    assert!(
        !shown.contains("bmV3") && !shown.contains("b2xk"),
        "{shown}"
    );
}

#[test]
fn an_edit_without_status_keeps_the_live_status() {
    let live = json!({"status": {"phase": "Ready"}});
    let mut body = json!({"spec": {}});
    keep_live_status(&mut body, &live);
    assert_eq!(body["status"]["phase"], "Ready");
}

#[test]
fn invalid_yaml_does_not_repeat_the_text() {
    let yaml = "apiVersion: v1\nkind: Secret\nstringData:\n  password: \"hunter2\n";
    let error = edit_body(yaml, &kind(), &target()).unwrap_err().to_string();
    assert!(!error.contains("hunter2"), "{error}");
}

#[test]
fn the_gitops_warning_comes_from_the_live_object() {
    let managed = json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": "web",
            "namespace": "demo",
            "labels": {"kustomize.toolkit.fluxcd.io/name": "apps",
                       "kustomize.toolkit.fluxcd.io/namespace": "flux-system"},
        },
    });
    let warnings = warnings_for(&managed);
    assert!(
        matches!(&warnings[..], [WriteWarning::LikelyReverted(m)] if m.name == "apps"),
        "{warnings:?}"
    );
    assert!(warnings_for(&json!({"metadata": {"name": "web"}})).is_empty());
}

#[path = "write_timeout_tests.rs"]
mod timeout;

#[test]
fn a_write_error_shows_reduced_server_text() {
    let message = "denied: password \"hunter2-secret\" Authorization: Bearer abcdefghijklmnop";
    let status = serde_json::from_value(json!({
        "apiVersion": "v1", "kind": "Status", "status": "Failure",
        "code": 403, "reason": "Forbidden", "message": message,
    }))
    .unwrap();
    let text = write_error(kube::Error::Api(status)).to_string();
    assert!(!text.contains("hunter2-secret"), "{text}");
    assert!(!text.contains("abcdefghijklmnop"), "{text}");
}

#[test]
fn a_masked_looking_tracking_id_on_a_live_object_warns() {
    let live = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {
        "name": "web", "namespace": "demo",
        "annotations": {"argocd.argoproj.io/tracking-id": "shop\u{2022}:apps/Deployment:demo/web"},
    }});
    assert_eq!(warnings_for(&live).len(), 1);
}

// A disconnect asked for while a write runs stops its next request at once, before the core
// has taken the command.
#[tokio::test]
async fn a_disconnect_asked_for_stops_the_next_request_before_the_core_runs_it() {
    let dir = tempfile::tempdir().unwrap();
    let core = CoreHandle::start(crate::CoreConfig {
        config_dir: dir.path().to_owned(),
        settings: crate::settings::Settings::default(),
        settings_error: None,
        kubeconfig_base: Vec::new(),
        home: None,
        watch_files: false,
        watch_timing: Default::default(),
    })
    .unwrap();
    let mut gate = RequestGate::new(core.clone(), &target());
    assert_eq!(gate.check_disconnects(), Ok(()));

    core.ask_disconnect_for_test("dev");

    let refused = WriteError::from(WriteRefusal::NotConnected {
        cluster: "dev".to_owned(),
    });
    assert_eq!(gate.check_disconnects(), Err(refused.clone()));
    assert_eq!(gate.client().await.err(), Some(refused.clone()));
    // A write started now waits for the core to run the disconnect, too.
    let later = RequestGate::new(core.clone(), &target());
    assert_eq!(later.check_disconnects(), Err(refused));
}
