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

/// For a target whose run never starts.
fn unused_root() -> PathBuf {
    PathBuf::from("/nonexistent/helm-runs")
}

/// A target whose runs keep their files under `dir`.
fn target(dir: &Path) -> HelmTarget {
    let merged = config(CONFIG);
    let pin = pin_for(&merged);
    HelmTarget::new(
        &merged,
        "dev",
        pin,
        credential(CredentialSource::Definition),
        None,
        runs_root(dir),
    )
    .unwrap()
}

fn find_helm(configured: Option<&Path>, path: Option<&OsStr>) -> Result<PathBuf, HelmError> {
    locate_helm(configured, path, admin_group()).map(|found| found.path)
}

async fn run_at(
    binary: &Path,
    target: &HelmTarget,
    query: &HelmQuery,
) -> Result<HelmOutput, HelmError> {
    run_with(binary, target, query, admin_group()).await
}

/// This process's group, standing in for the administrators' group on any machine.
fn own_group() -> u32 {
    // SAFETY: takes nothing and cannot fail.
    unsafe { libc::getegid() }
}

/// A Homebrew layout under `dir`: `bin/helm` links into `Cellar`, and both are group-writable.
fn brew_layout(dir: &Path) -> (PathBuf, PathBuf) {
    let bin = dir.join("bin");
    let cellar = dir.join("Cellar");
    let keg = cellar.join("helm/1.0/bin");
    std::fs::create_dir_all(&keg).unwrap();
    std::fs::create_dir(&bin).unwrap();
    let real = fake_helm(&keg, "printf '[]'");
    std::os::unix::fs::symlink("../Cellar/helm/1.0/bin/helm", bin.join("helm")).unwrap();
    for shared in [&bin, &cellar] {
        std::os::unix::fs::chown(shared, None, Some(own_group())).unwrap();
        std::fs::set_permissions(shared, std::fs::Permissions::from_mode(0o775)).unwrap();
    }
    (bin.join("helm"), real)
}

#[tokio::test]
async fn an_admin_group_install_runs_and_warns() {
    let dir = tempfile::tempdir().unwrap();
    let (link, real) = brew_layout(dir.path());
    let found = locate_helm(Some(&link), None, Some(own_group())).unwrap();
    assert_eq!(found.path, real.canonicalize().unwrap());
    assert!(found.admin_writable);
    let warning = admin_writable_warning(&found.path);
    assert!(warning.contains(&*found.path.to_string_lossy()));
    assert!(warning.contains("administrators"));
    assert!(warning.contains("credentials"));
    run_with(
        &found.path,
        &target(dir.path()),
        &HelmQuery::List,
        Some(own_group()),
    )
    .await
    .unwrap();
}

#[test]
fn a_private_install_does_not_warn() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(dir.path(), "");
    let found = locate_helm(Some(&helm), None, Some(own_group())).unwrap();
    assert!(!found.admin_writable);
}

#[test]
fn another_writable_group_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (link, _) = brew_layout(dir.path());
    assert_eq!(
        locate_helm(Some(&link), None, Some(own_group() + 1)),
        Err(HelmError::UntrustedLocation)
    );
}

#[test]
fn a_world_writable_install_is_refused_even_in_the_admin_group() {
    let dir = tempfile::tempdir().unwrap();
    let (link, _) = brew_layout(dir.path());
    std::fs::set_permissions(
        dir.path().join("Cellar"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert_eq!(
        locate_helm(Some(&link), None, Some(own_group())),
        Err(HelmError::UntrustedLocation)
    );
}

#[test]
fn another_owner_is_refused() {
    use run_dir::{Access, classify};
    let (me, admin) = (501, 80);
    assert_eq!(
        classify((502, admin, 0o755), true, me, Some(admin)),
        Access::Refused
    );
    assert_eq!(
        classify((502, admin, 0o775), true, me, Some(admin)),
        Access::Refused
    );
    assert_eq!(
        classify((me, admin, 0o775), true, me, Some(admin)),
        Access::AdminGroup
    );
    assert_eq!(
        classify((0, admin, 0o775), false, me, Some(admin)),
        Access::AdminGroup
    );
}

#[test]
fn a_link_to_an_unsafe_target_is_refused() {
    let open = tempfile::tempdir().unwrap();
    let helm = fake_helm(open.path(), "");
    std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let safe = tempfile::tempdir().unwrap();
    let link = safe.path().join("helm");
    std::os::unix::fs::symlink(&helm, &link).unwrap();
    assert_eq!(
        locate_helm(Some(&link), None, Some(own_group())),
        Err(HelmError::UntrustedLocation)
    );
}

// A binary that became unsafe after it was found gets no credentials.
#[tokio::test]
async fn a_binary_that_became_unsafe_is_refused_before_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let (link, real) = brew_layout(dir.path());
    let found = locate_helm(Some(&link), None, Some(own_group())).unwrap();
    std::fs::set_permissions(
        dir.path().join("Cellar"),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let refused = run_with(
        &found.path,
        &target(dir.path()),
        &HelmQuery::List,
        Some(own_group()),
    )
    .await;
    assert_eq!(refused, Err(HelmError::UntrustedLocation));
    assert!(!real.with_file_name("args").exists(), "helm never started");
}

/// Output parsed with no values to hide.
fn parse(query: &HelmQuery, out: &[u8]) -> Result<HelmOutput, HelmError> {
    super::parse(query, out, &HiddenValues::default())
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
    let output = run_at(&helm, &target(dir.path()), &HelmQuery::List)
        .await
        .unwrap();
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
        unused_root(),
    );
    assert_eq!(refused.err(), Some(HelmError::CredentialRunsCommand));

    let merged = config(CONFIG);
    let refused = HelmTarget::new(
        &merged,
        "dev",
        pin_for(&merged),
        credential(CredentialSource::Plugin),
        None,
        unused_root(),
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
        unused_root(),
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
    let target =
        HelmTarget::new(&merged, "dev", pin_for(&merged), held, None, unused_root()).unwrap();

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
    let real = helm.canonicalize().unwrap();
    assert_eq!(find_helm(None, Some(&empty)).unwrap(), real);
    assert_eq!(find_helm(Some(&helm), None).unwrap(), real);
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
    let error = run_at(&helm, &target(dir.path()), &HelmQuery::History(release()))
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
    let target = target(dir.path());
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

fn token_config(user: &str) -> Kubeconfig {
    config(&CONFIG.replace("token: sekret-token", user))
}

fn written_user(target: &HelmTarget) -> serde_json::Value {
    let bytes = target.pinned_kubeconfig().unwrap();
    let written: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    written["users"][0]["user"].clone()
}

// With a token and a token file, kube uses the token and client-go the file.
#[test]
fn only_the_admitted_token_reaches_helm() {
    let merged = token_config("token: admitted\n    tokenFile: /other/identity");
    let target = HelmTarget::new(
        &merged,
        "dev",
        pin_for(&merged),
        credential(CredentialSource::Definition),
        None,
        unused_root(),
    )
    .unwrap();
    let user = written_user(&target);
    assert_eq!(user["token"], "admitted");
    assert!(user.get("tokenFile").is_none(), "{user}");
}

// Basic auth comes first in kube's order, so a token beside it is not the admitted credential.
#[test]
fn basic_auth_drops_a_token_beside_it() {
    let merged = token_config("username: me\n    password: pw\n    token: other");
    let target = HelmTarget::new(
        &merged,
        "dev",
        pin_for(&merged),
        credential(CredentialSource::Definition),
        None,
        unused_root(),
    )
    .unwrap();
    let user = written_user(&target);
    assert_eq!(user["username"], "me");
    assert!(user.get("token").is_none(), "{user}");
}

/// The token kube's own auth layer holds for the token file at `path`.
async fn client_token_of(path: &Path) -> Option<SecretString> {
    let mut config = kube::Config::new("https://pinned:6443".parse().unwrap());
    config.auth_info.token_file = Some(path.display().to_string());
    let layer = kube::client::ConfigExt::auth_layer(&config)
        .unwrap()
        .unwrap();
    crate::credential::client_token(&layer).await
}

// Helm gets the token the client was admitted with, whatever the file holds by then.
#[tokio::test]
async fn a_token_file_goes_in_as_the_admitted_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, "admitted").unwrap();
    let token = client_token_of(&file).await;
    std::fs::write(&file, "another-identity").unwrap();
    let merged = token_config(&format!("tokenFile: {}", file.display()));
    let target = HelmTarget::new(
        &merged,
        "dev",
        pin_for(&merged),
        credential(CredentialSource::TokenFile(file)).with_admitted_token(token),
        None,
        unused_root(),
    )
    .unwrap();
    let user = written_user(&target);
    assert_eq!(user["token"], "admitted");
    assert!(user.get("tokenFile").is_none(), "{user}");
}

#[test]
fn a_token_file_without_a_snapshot_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, "readable").unwrap();
    let merged = token_config(&format!("tokenFile: {}", file.display()));
    let target = HelmTarget::new(
        &merged,
        "dev",
        pin_for(&merged),
        credential(CredentialSource::TokenFile(file)),
        None,
        unused_root(),
    )
    .unwrap();
    assert_eq!(
        target.pinned_kubeconfig().err(),
        Some(HelmError::CredentialFileChanged)
    );
}

// The run's directory is the owner's alone, whatever the umask.
#[tokio::test]
async fn the_run_directory_is_private() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(
        dir.path(),
        &format!(
            "/bin/ls -ld \"$PWD\" > '{}/mode'\nprintf '[]'",
            dir.path().display()
        ),
    );
    run_at(&helm, &target(dir.path()), &HelmQuery::List)
        .await
        .unwrap();
    let mode = std::fs::read_to_string(dir.path().join("mode")).unwrap();
    assert!(mode.starts_with("drwx------"), "{mode}");
}

#[test]
fn helm_in_a_folder_others_can_write_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(dir.path(), "");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = std::env::join_paths([dir.path()]).unwrap();
    assert_eq!(
        find_helm(None, Some(&path)),
        Err(HelmError::UntrustedLocation)
    );
    assert_eq!(
        find_helm(Some(&helm), None),
        Err(HelmError::UntrustedLocation)
    );
}

// A link in a writable folder chooses what runs, even when its target is safe.
#[test]
fn a_link_in_a_folder_others_can_write_is_refused() {
    let safe = tempfile::tempdir().unwrap();
    let helm = fake_helm(safe.path(), "");
    let open = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&helm, open.path().join("helm")).unwrap();
    std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let path = std::env::join_paths([open.path()]).unwrap();
    assert_eq!(
        find_helm(None, Some(&path)),
        Err(HelmError::UntrustedLocation)
    );
}

#[test]
fn a_writable_helm_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(dir.path(), "");
    std::fs::set_permissions(&helm, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        find_helm(Some(&helm), None),
        Err(HelmError::UntrustedLocation)
    );
}

// Whatever sits under a credential field is hidden, and a reference to a Secret stays readable.
#[test]
fn a_sensitive_subtree_is_hidden_whole() {
    let masked = mask_manifest(
        "apiVersion: example.com/v1\nkind: Database\nmetadata:\n  name: d\nspec:\n  credentials:\n    \
         primary: hunter11\n    nested:\n    - deep: hunter12\n  imagePullSecrets:\n  - name: \
         pull-secret\n  volumes:\n  - secret:\n      secretName: web-tls\n",
    );
    for leaked in ["hunter11", "hunter12"] {
        assert!(!masked.contains(leaked), "{leaked} in {masked}");
    }
    for kept in ["pull-secret", "web-tls"] {
        assert!(masked.contains(kept), "{kept} in {masked}");
    }
}

// A value hidden in Values stays hidden where a template copied it into the manifest.
#[tokio::test]
async fn a_masked_value_does_not_reappear_in_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode("hunter-cross");
    let helm = fake_helm(
        dir.path(),
        &format!(
            "case \"$2\" in\nweb) printf '{{\"name\":\"web\",\"version\":1}}' ;;\n\
             values) printf '{{\"auth\":{{\"password\":\"hunter-cross\"}}}}' ;;\n\
             *) printf 'apiVersion: v1\\nkind: Pod\\nmetadata:\\n  name: p\\n  labels:\\n    \
             hunter-cross: x\\nspec:\\n  args: [\"--login=hunter-cross\", \"{encoded}\"]\\n' ;;\nesac"
        ),
    );
    let output = run_at(&helm, &target(dir.path()), &HelmQuery::Manifest(release()))
        .await
        .unwrap();
    let HelmOutput::Text(text) = output else {
        panic!("text");
    };
    assert!(!text.contains("hunter-cross"), "{text}");
    assert!(!text.contains(&encoded), "{text}");
}

// A stored failure can quote a credential in any format, so its text is never shown.
#[test]
fn details_and_history_show_fixed_status_text() {
    let opaque = "Upgrade failed: admission denied value q9-opaque-credential";
    let out = json!({"name": "web", "info": {"status": "failed", "description": opaque}});
    let HelmOutput::Details(details) =
        parse(&HelmQuery::Details(release()), out.to_string().as_bytes()).unwrap()
    else {
        panic!("details");
    };
    assert!(!details.description.contains("q9-opaque"), "{details:?}");
    assert!(!details.description.is_empty());

    let out = json!([{"revision": 1, "status": "failed", "description": opaque}]);
    let HelmOutput::History(history) =
        parse(&HelmQuery::History(release()), out.to_string().as_bytes()).unwrap()
    else {
        panic!("history");
    };
    assert!(!history[0].description.contains("q9-opaque"), "{history:?}");
}

// What a crash left is removed; a run still going is not.
#[test]
fn a_new_start_reaps_only_dead_runs() {
    let dir = tempfile::tempdir().unwrap();
    let root = runs_root(dir.path());
    let live = RunDir::create(&root).unwrap();
    // A crash: the directory stays, and its lock ended with the process.
    let dead = root.canonicalize().unwrap().join("run-crashed");
    std::fs::create_dir(&dead).unwrap();
    std::fs::write(dead.join(".lock"), "").unwrap();
    std::fs::write(dead.join("kubeconfig"), "credential").unwrap();

    reap_stale(&root);

    assert!(!dead.exists(), "a dead run is removed");
    assert!(
        live.path().join(".lock").exists(),
        "a live run is left alone"
    );
    let live_path = live.path().to_owned();
    drop(live);
    assert!(!live_path.exists(), "a run removes its own directory");
}

#[test]
fn a_run_root_others_can_enter_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = runs_root(dir.path());
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert_eq!(
        RunDir::create(&root).err().map(|e| e.to_string()),
        Some(HelmError::UnsafeRunDirectory.to_string())
    );
}

#[test]
fn a_run_root_that_is_a_link_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let root = runs_root(dir.path());
    std::os::unix::fs::symlink(elsewhere.path(), &root).unwrap();
    assert_eq!(
        RunDir::create(&root).err().map(|e| e.to_string()),
        Some(HelmError::UnsafeRunDirectory.to_string())
    );
}

#[test]
fn a_secret_inside_a_list_is_masked() {
    let masked = mask_manifest(
        "apiVersion: v1\nkind: List\nitems:\n- apiVersion: v1\n  kind: Secret\n  metadata:\n    \
         name: s\n  data:\n    anything: aHVudGVyOQ==\n",
    );
    assert!(!masked.contains("aHVudGVyOQ"), "{masked}");
}

/// Writes an executable script at `path` with the first line `shebang`.
fn script(path: &Path, shebang: &str) {
    std::fs::write(path, format!("{shebang}\nexit 0\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_wrapper_runs_only_on_a_private_interpreter() {
    let dir = tempfile::tempdir().unwrap();
    let helm = dir.path().join("helm");

    script(&helm, "#!/bin/sh");
    assert!(locate_helm(Some(&helm), None, Some(own_group())).is_ok());

    // Replaceable by the administrators' group: it would receive the kubeconfig.
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    let interpreter = shared.join("interpreter");
    std::fs::copy("/bin/sh", &interpreter).unwrap();
    std::os::unix::fs::chown(&shared, None, Some(own_group())).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o775)).unwrap();
    for refused in [
        format!("#!{}", interpreter.display()),
        // What `env` starts is chosen when it runs, so no check here can cover it.
        "#!/usr/bin/env sh".to_owned(),
        "#!relative/sh".to_owned(),
        "#!/nonexistent/sh".to_owned(),
        "#!".to_owned(),
    ] {
        script(&helm, &refused);
        assert_eq!(
            locate_helm(Some(&helm), None, Some(own_group())),
            Err(HelmError::UntrustedLocation),
            "{refused}"
        );
    }

    // An interpreter that is itself a script.
    let nested = dir.path().join("nested");
    script(&nested, "#!/bin/sh");
    script(&helm, &format!("#!{}", nested.display()));
    assert_eq!(
        locate_helm(Some(&helm), None, Some(own_group())),
        Err(HelmError::UntrustedLocation)
    );
}

// After a crash the child still holds the lock, so its kubeconfig is not reaped under it.
#[tokio::test]
async fn a_live_child_keeps_its_run_after_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let root = runs_root(dir.path());
    let run = RunDir::create(&root).unwrap();
    let mut command = tokio::process::Command::new("/bin/sleep");
    command.arg("30").kill_on_drop(true);
    run.hand_lock_to(&mut command);
    let mut child = command.spawn().unwrap();
    let path = run.crash();

    reap_stale(&root);
    assert!(path.exists(), "a live child's run stays");

    child.kill().await.unwrap();
    reap_stale(&root);
    assert!(!path.exists(), "removed once the child has gone");
}

/// A load of `yaml` as the only kubeconfig file.
fn record(history: &mut crate::recheck::ContextHistory, yaml: &str) {
    let merged = config(yaml);
    history.record(&crate::kubeconfig::LoadedKubeconfig {
        contexts: vec![crate::kubeconfig::ContextInfo {
            name: "dev".into(),
            cluster: Some("c".into()),
            user: Some("u".into()),
            namespace: None,
            server: Some("https://pinned:6443".into()),
            proxy_url: None,
            source: "/k".into(),
        }],
        merged,
        ..Default::default()
    });
}

// A credential edit on the same server, even one undone later, never gives the old pin back.
#[test]
fn a_definition_changed_and_put_back_is_another_revision() {
    let mut history = crate::recheck::ContextHistory::default();
    record(&mut history, CONFIG);
    let opened = history.definition_revision("dev");

    record(&mut history, CONFIG);
    assert_eq!(
        history.definition_revision("dev"),
        opened,
        "nothing changed"
    );

    record(&mut history, &CONFIG.replace("sekret-token", "another"));
    let edited = history.definition_revision("dev");
    assert_ne!(edited, opened);

    record(&mut history, CONFIG);
    let restored = history.definition_revision("dev");
    assert_ne!(restored, opened, "A to B to A is not A");
    assert_ne!(restored, edited);
}

/// A fake `helm` whose release moves on: only `--revision=7` gives revision 7's data.
const MOVING_RELEASE: &str = r#"case "$1 $2" in
"status web") printf '{"name":"web","version":7,"info":{"status":"deployed"},"chart":{"metadata":{"name":"web","version":"1.0.0","appVersion":"hunter-meta"}}}' ;;
"get values") case "$*" in
  *--revision=7*) printf '{"auth":{"password":"hunter-meta"},"apiTokenName":"neutral-name"}' ;;
  *) printf '{"auth":{"password":"unrelated"}}' ;;
  esac ;;
"get manifest") case "$*" in
  *--revision=7*) printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\ndata:\n  plain: hunter-meta\n  other: neutral-name\n' ;;
  *) printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\ndata:\n  plain: from-another-revision\n' ;;
  esac ;;
esac"#;

#[tokio::test]
async fn values_and_manifest_come_from_one_revision() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(dir.path(), MOVING_RELEASE);
    let output = run_at(&helm, &target(dir.path()), &HelmQuery::Manifest(release()))
        .await
        .unwrap();
    let HelmOutput::Text(text) = output else {
        panic!("text");
    };
    assert!(!text.contains("from-another-revision"), "{text}");
    assert!(!text.contains("hunter-meta"), "{text}");
    assert!(!text.contains("neutral-name"), "{text}");
}

// Everything Values hides is hidden elsewhere, a field that looks like a reference included.
#[test]
fn what_values_mask_is_what_a_manifest_hides() {
    let out = br#"{"apiTokenName":"neutral-name","url":"postgres://me:hunter3@db:5432/app"}"#;
    let values: serde_json::Value = serde_json::from_slice(out).unwrap();
    let shown = mask_values(values).unwrap();
    assert!(!shown.contains("neutral-name"), "{shown}");

    let hidden = HiddenValues::from_output(out).unwrap();
    let masked = hidden.mask_manifest(
        "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\ndata:\n  plain: neutral-name\n  \
         copy: postgres://me:hunter3@db:5432/app\n",
    );
    assert!(!masked.contains("neutral-name"), "{masked}");
    assert!(!masked.contains("hunter3"), "{masked}");
}

#[tokio::test]
async fn details_scrub_metadata_against_their_revisions_values() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(dir.path(), MOVING_RELEASE);
    let output = run_at(&helm, &target(dir.path()), &HelmQuery::Details(release()))
        .await
        .unwrap();
    let HelmOutput::Details(details) = output else {
        panic!("details");
    };
    assert_eq!(details.revision, 7);
    assert_eq!(details.app_version, MASK);
    assert_eq!(details.chart_version, "1.0.0");
}

// A list or history has no one revision's values to scrub against, so it does not keep this.
#[test]
fn lists_and_history_do_not_keep_the_app_version() {
    let row = json!({"name": "web", "namespace": "apps", "revision": 1, "status": "deployed",
                     "app_version": "hunter-meta"});
    let list = parse(&HelmQuery::List, json!([row]).to_string().as_bytes()).unwrap();
    assert!(!format!("{list:?}").contains("hunter-meta"), "{list:?}");
    let history = parse(
        &HelmQuery::History(release()),
        json!([row]).to_string().as_bytes(),
    )
    .unwrap();
    assert!(
        !format!("{history:?}").contains("hunter-meta"),
        "{history:?}"
    );
}

/// Adds the ACL entry `entry` to `path`.
#[cfg(target_os = "macos")]
fn add_acl(path: &Path, entry: &str) {
    let status = std::process::Command::new("/bin/chmod")
        .args(["+a", entry])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "{entry}");
}

#[cfg(target_os = "macos")]
#[test]
fn an_acl_letting_another_account_change_the_path_is_refused() {
    for entry in [
        "user:nobody allow add_file",
        "user:nobody allow add_subdirectory",
        "user:nobody allow delete_child",
        "group:everyone allow writesecurity",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let helm = fake_helm(dir.path(), "");
        add_acl(dir.path(), entry);
        assert_eq!(
            locate_helm(Some(&helm), None, Some(own_group())),
            Err(HelmError::UntrustedLocation),
            "{entry}"
        );
    }
    for entry in ["user:nobody allow write", "user:nobody allow delete"] {
        let dir = tempfile::tempdir().unwrap();
        let helm = fake_helm(dir.path(), "");
        add_acl(&helm, entry);
        assert_eq!(
            locate_helm(Some(&helm), None, Some(own_group())),
            Err(HelmError::UntrustedLocation),
            "{entry}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn ordinary_acls_on_the_path_are_not_refused() {
    let dir = tempfile::tempdir().unwrap();
    let helm = fake_helm(dir.path(), "");
    // What macOS puts on home folders, and a grant that changes nothing.
    add_acl(dir.path(), "group:everyone deny delete");
    add_acl(dir.path(), "user:nobody allow list,search,readattr");
    add_acl(&helm, "group:everyone deny write,delete");
    assert!(locate_helm(Some(&helm), None, Some(own_group())).is_ok());
}

#[cfg(target_os = "macos")]
#[test]
fn an_acl_letting_another_account_read_the_run_root_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = runs_root(dir.path());
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    add_acl(&root, "user:nobody allow list,search,read");
    assert_eq!(
        RunDir::create(&root).err(),
        Some(HelmError::UnsafeRunDirectory)
    );
}

// An entry the app's folder passes down never reaches a run's directory or kubeconfig.
#[cfg(target_os = "macos")]
#[test]
fn a_run_directory_inherits_no_access() {
    let dir = tempfile::tempdir().unwrap();
    add_acl(
        dir.path(),
        "user:nobody allow list,search,read,file_inherit,directory_inherit",
    );
    assert_eq!(
        RunDir::create(&runs_root(dir.path())).err(),
        Some(HelmError::UnsafeRunDirectory)
    );

    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    std::fs::create_dir(&private).unwrap();
    add_acl(&private, "user:nobody allow read,file_inherit");
    assert_eq!(
        write_private(&private, b"credential").err(),
        Some(HelmError::UnsafeRunDirectory)
    );
    let written = std::fs::read(private.join("kubeconfig")).unwrap_or_default();
    assert!(written.is_empty(), "nothing is written to a shared file");
}
