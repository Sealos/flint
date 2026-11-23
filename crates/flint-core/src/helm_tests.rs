use serde_json::json;

use super::*;
use crate::credential::{CredentialSource, hold_credential_files};

const CONFIG: &str = "apiVersion: v1\nkind: Config\nclusters:\n- name: c\n  cluster:\n    server: \
                      https://pinned:6443\n- name: other\n  cluster:\n    server: https://other\n\
                      users:\n- name: u\n  user:\n    token: sekret-token\n- name: x\n  user:\n    \
                      token: other-token\ncontexts:\n- name: dev\n  context:\n    cluster: c\n    \
                      user: u\n- name: prod\n  context:\n    cluster: other\n    user: x\n\
                      current-context: prod\n";

fn config(yaml: &str) -> Kubeconfig {
    serde_saphyr::from_str(yaml).unwrap()
}

fn credential(source: CredentialSource) -> WriteCredential {
    WriteCredential::new(Vec::new(), source, None, None)
}

fn pin_for(kubeconfig: &Kubeconfig) -> ContextPin {
    let mut history = crate::recheck::ContextHistory::default();
    let loaded = crate::kubeconfig::LoadedKubeconfig {
        merged: kubeconfig.clone(),
        contexts: vec![crate::kubeconfig::ContextInfo {
            name: "dev".into(),
            cluster: Some("c".into()),
            user: Some("u".into()),
            namespace: None,
            server: Some("https://pinned:6443".into()),
            proxy_url: None,
            source: "/k".into(),
        }],
        ..Default::default()
    };
    history.record(&loaded);
    history.pin("dev", Some("https://pinned:6443"))
}

fn target() -> HelmTarget {
    let merged = config(CONFIG);
    let pin = pin_for(&merged);
    HelmTarget::new(
        &merged,
        "dev",
        pin,
        credential(CredentialSource::Definition),
        None,
    )
    .unwrap()
}

/// A fake `helm` that records its arguments, environment and kubeconfig under `dir`, then runs
/// `body`. Uses absolute paths only: it gets no `PATH`.
fn fake_helm(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("helm");
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{d}/args'\n/usr/bin/env > \
         '{d}/env'\n/bin/cat \"$KUBECONFIG\" > '{d}/kubeconfig'\n/bin/pwd > '{d}/cwd'\n{body}\n",
        d = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn release() -> ReleaseRef {
    ReleaseRef::new("web", "apps").unwrap()
}

// A run gets a list of arguments, nothing of Flint's environment, and a kubeconfig holding only
// the pinned context, whatever the person's current context is.
#[tokio::test]
async fn a_run_is_pinned_and_gets_a_minimal_environment() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(
        dir.path(),
        "printf '[{\"name\":\"web\",\"namespace\":\"apps\",\"revision\":\"3\",\"status\":\"deployed\",\"chart\":\"web-1.0\",\"app_version\":\"2\",\"updated\":\"now\"}]'",
    );
    let output = run_at(&helm, &target(), &HelmQuery::List).await.unwrap();
    let HelmOutput::Releases {
        releases,
        truncated,
    } = output
    else {
        panic!("a list");
    };
    assert!(!truncated);
    assert_eq!(releases[0].revision, 3);

    let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(
        args[..4],
        ["list", "--all-namespaces", "--max=1000", "--output=json"]
    );
    assert_eq!(args[4], "--kube-context=dev");
    assert!(args[5].starts_with("--kubeconfig="));

    let env = std::fs::read_to_string(dir.path().join("env")).unwrap();
    let names: std::collections::BTreeSet<&str> =
        env.lines().filter_map(|l| l.split('=').next()).collect();
    for name in names {
        assert!(
            [
                "KUBECONFIG",
                "HOME",
                "HELM_CACHE_HOME",
                "HELM_CONFIG_HOME",
                "HELM_DATA_HOME",
                "HELM_DRIVER",
                "PWD",
                "OLDPWD",
                "SHLVL",
                "_",
                "PATH"
            ]
            .contains(&name),
            "unexpected variable {name}"
        );
    }
    assert!(
        !env.lines()
            .any(|l| l.starts_with("PATH=") && l.contains("homebrew")),
        "Flint's PATH is not passed"
    );

    let written: Kubeconfig =
        serde_json::from_slice(&std::fs::read(dir.path().join("kubeconfig")).unwrap()).unwrap();
    assert_eq!(written.current_context.as_deref(), Some("dev"));
    assert_eq!(written.contexts.len(), 1);
    assert_eq!(written.clusters.len(), 1);
    assert_eq!(written.auth_infos.len(), 1);
    assert_eq!(
        written.clusters[0]
            .cluster
            .as_ref()
            .unwrap()
            .server
            .as_deref(),
        Some("https://pinned:6443")
    );
    let private = std::fs::read_to_string(dir.path().join("cwd")).unwrap();
    assert!(
        !Path::new(private.trim()).exists(),
        "the private directory is removed after the run"
    );
}

// An exec plugin or auth provider is refused, even behind a token that takes precedence in kube.
#[test]
fn a_credential_that_runs_a_program_is_refused() {
    let with_exec = CONFIG.replace(
        "token: sekret-token",
        "token: t\n    exec:\n      apiVersion: client.authentication.k8s.io/v1\n      command: /bin/evil",
    );
    let merged = config(&with_exec);
    let refused = HelmTarget::new(
        &merged,
        "dev",
        pin_for(&merged),
        credential(CredentialSource::Definition),
        None,
    );
    assert_eq!(refused.err(), Some(HelmError::CredentialRunsCommand));

    let merged = config(CONFIG);
    let refused = HelmTarget::new(
        &merged,
        "dev",
        pin_for(&merged),
        credential(CredentialSource::Plugin),
        None,
    );
    assert_eq!(refused.err(), Some(HelmError::CredentialRunsCommand));
}

// A context whose server is not the pinned one is refused.
#[test]
fn a_repointed_context_is_refused() {
    let merged = config(CONFIG);
    let pin = pin_for(&merged);
    let moved = config(&CONFIG.replace("https://pinned:6443", "https://attacker:6443"));
    let refused = HelmTarget::new(
        &moved,
        "dev",
        pin,
        credential(CredentialSource::Definition),
        None,
    );
    assert_eq!(refused.err(), Some(HelmError::Changed));
}

// A certificate file goes in as the bytes the connection was built from; replaced, it refuses.
#[tokio::test]
async fn a_replaced_certificate_file_refuses_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("ca.crt");
    std::fs::write(&ca, b"held-bytes").unwrap();
    let yaml = CONFIG.replace(
        "server: https://pinned:6443",
        &format!(
            "server: https://pinned:6443\n    certificate-authority: {}",
            ca.display()
        ),
    );
    let merged = config(&yaml);
    let mut for_client = merged.clone();
    let files = hold_credential_files(&mut for_client, "dev");
    let held = WriteCredential::new(files, CredentialSource::Definition, None, None);
    let target = HelmTarget::new(&merged, "dev", pin_for(&merged), held, None).unwrap();

    let bytes = target.pinned_kubeconfig().unwrap();
    let written: Kubeconfig = serde_json::from_slice(&bytes).unwrap();
    let cluster = written.clusters[0].cluster.as_ref().unwrap();
    assert!(cluster.certificate_authority.is_none());
    assert_eq!(
        cluster.certificate_authority_data.as_deref(),
        Some(
            base64::engine::general_purpose::STANDARD
                .encode(b"held-bytes")
                .as_str()
        )
    );

    std::fs::write(&ca, b"swapped").unwrap();
    assert_eq!(
        target.pinned_kubeconfig().err(),
        Some(HelmError::CredentialFileChanged)
    );
}

#[test]
fn a_missing_helm_says_so_and_only_absolute_paths_count() {
    let dir = tempfile::tempdir().unwrap();
    let empty = std::env::join_paths([dir.path()]).unwrap();
    assert_eq!(find_helm(None, Some(&empty)), Err(HelmError::NotInstalled));
    assert_eq!(find_helm(None, None), Err(HelmError::NotInstalled));
    assert!(HelmError::NotInstalled.to_string().contains("Install"));

    let helm = fake_helm(dir.path(), "");
    assert_eq!(find_helm(None, Some(&empty)).unwrap(), helm);
    assert_eq!(find_helm(Some(&helm), None).unwrap(), helm);
    assert_eq!(
        find_helm(Some(Path::new("helm")), None),
        Err(HelmError::BadConfiguredPath)
    );
    assert_eq!(
        find_helm(None, Some(OsStr::new("relative/bin"))),
        Err(HelmError::NotInstalled)
    );
}

// What helm prints on failure never reaches an error's text.
#[tokio::test]
async fn a_failure_is_classified_and_never_quotes_helm() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(
        dir.path(),
        "printf 'Error: secrets is forbidden: leaked-value' >&2; exit 1",
    );
    let error = run_at(&helm, &target(), &HelmQuery::History(release()))
        .await
        .unwrap_err();
    assert_eq!(error, HelmError::Forbidden);
    assert!(!error.to_string().contains("leaked-value"));
    assert_eq!(classify_failure(3, b"something else"), HelmError::Failed(3));
    assert_eq!(
        classify_failure(1, b"Error: release: not found"),
        HelmError::NotFound
    );
    assert_eq!(
        classify_failure(
            1,
            b"Kubernetes cluster unreachable: dial tcp: connection refused"
        ),
        HelmError::Unreachable
    );
}

// Dropping the read ends helm and what it started, and removes the private kubeconfig.
#[tokio::test]
async fn dropping_a_run_kills_it_and_removes_the_kubeconfig() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(
        dir.path(),
        &format!(
            "printf '%s' \"$$\" > '{d}/pid'\n/bin/sleep 60 &\nprintf '%s' \"$!\" > '{d}/child'\nwait",
            d = dir.path().display()
        ),
    );
    let target = target();
    let run = run_at(&helm, &target, &HelmQuery::List);
    let pid_file = dir.path().join("child");
    let waited = tokio::time::timeout(Duration::from_secs(5), run);
    let wait_for_pid = async {
        while !pid_file.exists() || std::fs::read_to_string(&pid_file).unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        _ = waited => panic!("the run should still be going"),
        () = wait_for_pid => {}
    }
    let child: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    let private = std::fs::read_to_string(dir.path().join("cwd")).unwrap();
    let mut gone = false;
    for _ in 0..200 {
        // SAFETY: signal 0 only asks whether the process exists.
        if unsafe { libc::kill(child, 0) } != 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(gone, "what helm started ends with it");
    assert!(!Path::new(private.trim()).exists());
}

#[tokio::test]
async fn output_past_the_cap_is_refused() {
    let mut reader: &[u8] = &[b'x'; 11];
    assert_eq!(
        read_capped(&mut reader, 10).await,
        Err(HelmError::OutputTooLarge)
    );
    let mut reader: &[u8] = &[b'x'; 10];
    assert_eq!(read_capped(&mut reader, 10).await.unwrap().len(), 10);
}

// A name that could be read as an option never becomes an argument.
#[test]
fn release_names_cannot_be_options() {
    for (name, namespace) in [
        ("--kubeconfig=/evil", "apps"),
        ("web", "--all"),
        ("web/x", "apps"),
        ("", "apps"),
        (&*"a".repeat(54), "apps"),
    ] {
        assert_eq!(
            ReleaseRef::new(name, namespace),
            Err(HelmError::InvalidRelease),
            "{name} {namespace}"
        );
    }
}

// Every query only reads.
#[test]
fn every_query_uses_a_read_only_subcommand() {
    let r = release();
    for query in [
        HelmQuery::List,
        HelmQuery::Details(r.clone()),
        HelmQuery::History(r.clone()),
        HelmQuery::Values(r.clone()),
        HelmQuery::Manifest(r),
    ] {
        let args = query.args();
        assert!(
            ["list", "status", "history", "get"].contains(&args[0].as_str()),
            "{args:?}"
        );
    }
}

// Details keep what they show and read neither the notes nor the stored values.
#[test]
fn details_leave_out_notes_and_values() {
    let out = json!({
        "name": "web", "namespace": "apps", "version": 4,
        "info": {"status": "deployed", "description": "Upgrade complete",
                 "first_deployed": "a", "last_deployed": "b",
                 "notes": "password is hunter2"},
        "chart": {"metadata": {"name": "web", "version": "1.2.3", "appVersion": "9"}},
        "config": {"password": "hunter2"},
        "manifest": "secret"
    });
    let HelmOutput::Details(details) =
        parse(&HelmQuery::Details(release()), out.to_string().as_bytes()).unwrap()
    else {
        panic!("details");
    };
    assert_eq!(details.revision, 4);
    assert_eq!(details.chart_version, "1.2.3");
    assert!(!format!("{details:?}").contains("hunter2"));
}

#[test]
fn history_is_newest_first_with_descriptions_masked() {
    let out = json!([
        {"revision": 1, "status": "superseded", "description": "Install complete"},
        {"revision": 2, "status": "failed",
         "description": "failed: password=hunter2"}
    ]);
    let HelmOutput::History(history) =
        parse(&HelmQuery::History(release()), out.to_string().as_bytes()).unwrap()
    else {
        panic!("history");
    };
    assert_eq!(history[0].revision, 2);
    assert!(!history[0].description.contains("hunter2"));
}

#[test]
fn an_empty_list_and_bad_output() {
    assert!(matches!(
        parse(&HelmQuery::List, b"  \n"),
        Ok(HelmOutput::Releases { releases, .. }) if releases.is_empty()
    ));
    assert_eq!(
        parse(&HelmQuery::List, b"not json"),
        Err(HelmError::BadOutput)
    );
}

#[test]
fn values_hide_sensitive_keys_and_credential_shapes() {
    let masked = mask_values(json!({
        "image": {"tag": "1.2"},
        "auth": {"password": "hunter2", "enabled": true, "rootPassword": null},
        "api-key": 12345,
        "db": {"secret": {"nested": "deep-value"}},
        "url": "postgres://me:hunter3@db:5432/app",
        "tolerations": [{"key": "dedicated", "value": "web"}],
        "env": [{"name": "DB_PASSWORD", "value": "hunter4"}, {"name": "MODE", "value": "fast"}],
    }))
    .unwrap();
    for leaked in ["hunter2", "hunter3", "hunter4", "12345", "deep-value"] {
        assert!(!masked.contains(leaked), "{leaked} in {masked}");
    }
    for kept in ["1.2", "dedicated", "fast", "enabled: true"] {
        assert!(masked.contains(kept), "{kept} in {masked}");
    }
    assert_eq!(mask_values(serde_json::Value::Null).unwrap(), "");
}

#[test]
fn a_manifest_hides_secrets_and_withholds_what_it_cannot_parse() {
    let manifest = "---\n# Source: web/templates/secret.yaml\napiVersion: v1\nkind: Secret\n\
                    metadata:\n  name: web\n  annotations:\n    note: inner-copy\ndata:\n  \
                    password: aHVudGVyMg==\nstringData:\n  plain: hunter5\n---\n# Source: \
                    web/templates/deploy.yaml\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  \
                    name: web\nspec:\n  template:\n    spec:\n      volumes:\n      - secret:\n          \
                    secretName: web-tls\n      containers:\n      - name: app\n        env:\n        \
                    - name: API_TOKEN\n          value: hunter6\n        - name: MODE\n          \
                    value: fast\n---\n# Source: web/templates/config.yaml\napiVersion: v1\nkind: \
                    ConfigMap\nmetadata:\n  name: c\ndata:\n  db-password: hunter7\n  mode: slow\n\
                    ---\n# Source: web/templates/broken.yaml\nkey: [unclosed hunter8\n";
    let masked = mask_manifest(manifest);
    for leaked in [
        "aHVudGVyMg",
        "hunter5",
        "inner-copy",
        "hunter6",
        "hunter7",
        "hunter8",
    ] {
        assert!(!masked.contains(leaked), "{leaked} in {masked}");
    }
    for kept in [
        "# Source: web/templates/secret.yaml",
        "secretName: web-tls",
        "MODE",
        "fast",
        "mode: slow",
        "# Withheld",
        "# Source: web/templates/broken.yaml",
    ] {
        assert!(masked.contains(kept), "{kept} in {masked}");
    }
}

// A custom resource can hold a credential in any field; a reference to a Secret stays readable.
#[test]
fn a_custom_resource_hides_credential_fields_and_keeps_references() {
    let masked = mask_manifest(
        "apiVersion: example.com/v1\nkind: Database\nmetadata:\n  name: d\nspec:\n  \
         adminPassword: hunter10\n  apiToken: 998877\n  passwordSecretRef:\n    name: db-auth\n  \
         tokenSecretName: db-token\n  automountServiceAccountToken: true\n",
    );
    for leaked in ["hunter10", "998877"] {
        assert!(!masked.contains(leaked), "{leaked} in {masked}");
    }
    for kept in ["db-auth", "db-token", "automountServiceAccountToken: true"] {
        assert!(masked.contains(kept), "{kept} in {masked}");
    }
}

#[test]
fn a_secret_inside_a_list_is_masked() {
    let masked = mask_manifest(
        "apiVersion: v1\nkind: List\nitems:\n- apiVersion: v1\n  kind: Secret\n  metadata:\n    \
         name: s\n  data:\n    anything: aHVudGVyOQ==\n",
    );
    assert!(!masked.contains("aHVudGVyOQ"), "{masked}");
}
