//! The teardown checks (UP-90, UP-91) in the cases a plain app test does not reach: edits that
//! name their files, a changing search order, re-pointed contexts, servers that stop early and
//! servers spelled `localhost`. No app runs: each test plays the app with its own requests.

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

use flint_e2e::fake_api::{FakeApiServer, Options, write_kubeconfigs};
use flint_e2e::teardown::Teardown;

struct Fixture {
    a: FakeApiServer,
    b: FakeApiServer,
    dir: tempfile::TempDir,
    runtime: tokio::runtime::Runtime,
}

impl Fixture {
    fn start() -> Fixture {
        Fixture {
            a: FakeApiServer::start_blocking(Options::default()).unwrap(),
            b: FakeApiServer::start_blocking(Options::default()).unwrap(),
            dir: tempfile::tempdir().unwrap(),
            runtime: tokio::runtime::Runtime::new().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// One request to `server` on a new connection, as the app would send it.
    fn contact(&self, server: &FakeApiServer) {
        self.runtime.block_on(async {
            let client = server.client().await.unwrap();
            client.apiserver_version().await.unwrap();
        });
    }

    fn finish(&self, teardown: &mut Teardown) -> Vec<String> {
        teardown.finish(Ok(ExitStatus::from_raw(0)), &self.path("no-socket"))
    }
}

fn watching(paths: &[&Path]) -> Teardown {
    let paths: Vec<PathBuf> = paths.iter().map(|p| p.to_path_buf()).collect();
    Teardown::at_launch(Box::new(move || paths.clone()))
}

fn edit(teardown: &mut Teardown, path: &Path, write: impl FnOnce()) {
    let files = [path.to_path_buf()];
    teardown.kubeconfig_editing(&files);
    write();
    teardown.kubeconfig_edited(&files);
}

// covers UP-90
#[test]
fn an_edit_keeps_the_baseline_of_files_it_does_not_name() {
    let f = Fixture::start();
    let (edited, other) = (f.path("edited"), f.path("other"));
    write_kubeconfigs(&edited, &[(&f.a, "a", None)]).unwrap();
    write_kubeconfigs(&other, &[(&f.b, "b", None)]).unwrap();
    let mut teardown = watching(&[&edited, &other]);

    std::fs::write(&other, "rewritten by the app").unwrap();
    edit(&mut teardown, &edited, || {
        write_kubeconfigs(&edited, &[(&f.a, "a", Some("demo"))]).unwrap();
    });

    let violations = f.finish(&mut teardown);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].starts_with(&format!("UP-90: the kubeconfig {}", other.display())),
        "{violations:?}"
    );
}

// covers UP-90
#[test]
fn an_edit_does_not_hide_an_earlier_change_to_its_own_file() {
    let f = Fixture::start();
    let path = f.path("kubeconfig");
    write_kubeconfigs(&path, &[(&f.a, "a", None)]).unwrap();
    let mut teardown = watching(&[&path]);

    std::fs::write(&path, "rewritten by the app").unwrap();
    edit(&mut teardown, &path, || {
        write_kubeconfigs(&path, &[(&f.a, "a", None)]).unwrap();
    });

    let violations = f.finish(&mut teardown);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(violations[0].starts_with("UP-90"), "{violations:?}");
}

// covers UP-91
#[test]
fn a_selection_follows_the_current_search_order() {
    let f = Fixture::start();
    let (first, second) = (f.path("first"), f.path("second"));
    write_kubeconfigs(&first, &[(&f.a, "x", None)]).unwrap();
    write_kubeconfigs(&second, &[(&f.b, "x", None)]).unwrap();
    let order = Arc::new(Mutex::new(vec![first.clone(), second.clone()]));
    let current = order.clone();
    let mut teardown = Teardown::at_launch(Box::new(move || current.lock().unwrap().clone()));

    // `second` now comes first, so "x" is its server; `first` stays as evidence only.
    *order.lock().unwrap() = vec![second, first];
    teardown.selecting("x");
    f.contact(&f.b);
    f.contact(&f.a);

    let violations = f.finish(&mut teardown);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains(&f.a.url()) && violations[0].contains("never selected \"x\""),
        "{violations:?}"
    );
}

// covers UP-91
#[test]
fn a_selection_moved_onto_a_tracked_server_checks_it_first() {
    let f = Fixture::start();
    let path = f.path("kubeconfig");
    write_kubeconfigs(&path, &[(&f.a, "x", None), (&f.b, "y", None)]).unwrap();
    let mut teardown = watching(&[&path]);
    teardown.selecting("x");
    f.contact(&f.b);

    edit(&mut teardown, &path, || {
        write_kubeconfigs(&path, &[(&f.b, "x", None)]).unwrap();
    });

    let violations = f.finish(&mut teardown);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains(&f.b.url())
            && violations[0].contains("before the test selected \"x\""),
        "{violations:?}"
    );
}

// covers UP-91: the app may react to the edit before the harness reads it back.
#[test]
fn a_selection_moved_onto_a_quiet_server_allows_requests_after_it() {
    let f = Fixture::start();
    let path = f.path("kubeconfig");
    write_kubeconfigs(&path, &[(&f.a, "x", None), (&f.b, "y", None)]).unwrap();
    let mut teardown = watching(&[&path]);
    teardown.selecting("x");

    edit(&mut teardown, &path, || {
        write_kubeconfigs(&path, &[(&f.b, "x", None)]).unwrap();
        f.contact(&f.b);
    });
    f.contact(&f.b);

    assert_eq!(f.finish(&mut teardown), Vec::<String>::new());
}

// covers UP-91
#[test]
fn requests_to_a_server_that_stopped_are_still_reported() {
    let Fixture { a, b, dir, runtime } = Fixture::start();
    let path = dir.path().join("kubeconfig");
    write_kubeconfigs(&path, &[(&a, "a", None), (&b, "b", None)]).unwrap();
    let mut teardown = watching(&[&path]);
    let url = b.url();
    runtime.block_on(async {
        b.client().await.unwrap().apiserver_version().await.unwrap();
    });
    drop(b);

    let violations = teardown.finish(Ok(ExitStatus::from_raw(0)), &dir.path().join("no-socket"));
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains(&url) && violations[0].contains("never selected \"b\""),
        "{violations:?}"
    );
}

// covers UP-91
#[test]
fn a_server_addressed_as_localhost_is_checked() {
    let f = Fixture::start();
    let path = f.path("kubeconfig");
    write_kubeconfigs(&path, &[(&f.a, "a", None), (&f.b, "b", None)]).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("https://127.0.0.1:"), "{text}");
    std::fs::write(
        &path,
        text.replace("https://127.0.0.1:", "https://localhost:"),
    )
    .unwrap();
    let mut teardown = watching(&[&path]);
    teardown.selecting("a");
    f.contact(&f.a);
    f.contact(&f.b);

    let violations = f.finish(&mut teardown);
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert!(
        violations[0].contains(&f.b.url()) && violations[0].contains("never selected \"b\""),
        "{violations:?}"
    );
}
